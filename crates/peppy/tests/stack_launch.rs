use config::node::Toolchain;
use peppy::commands::repo::{RepoCommand, RepoCommands};
use peppy::test_support::{
    LogCapture, ServeCommandEmulation, override_build_cmd, override_run_cmd_silent,
};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use config::consts::{NODE_CONFIG_FILE, PEPPYGEN_OUTPUT_PATH};
use core_node_api::SerializedNodeGraph;
use core_node_api::encoding::StackListRequest;
use daemon_config::consts::PEPPY_OUTPUT_DIR;
use peppy::commands::Command;
use peppy::commands::node::{NodeCommand, NodeCommands};
use peppy::commands::stack::{StackCommand, StackCommands};
use peppy::context::AppContext;
use peppylib::MessengerHandle;
use peppylib::services::health::listen_for_node_health;
use peppylib::services::ready::listen_for_node_ready;
use peppylib::services::shutdown::listen_for_shutdown;

use super::common::test_node_target;
use peppylib::core_node::transport::poll;
const CALLER_INSTANCE_ID: &str = "peppy-test";

/// The on-disk layout the daemon expects of a staged node, written once: the
/// manifest, its codegen fingerprint, and the `git.hash` under the peppy output
/// dir. Every helper below stages a node this way and differs only in how it
/// produces `manifest`.
fn install_node_manifest(
    nodes_directory: &Path,
    node_name: &str,
    git_hash: &str,
    manifest: &str,
) -> PathBuf {
    let node_dir = nodes_directory.join(node_name);
    fs::create_dir_all(&node_dir).expect("failed to create node directory");
    let node_config_path = node_dir.join(NODE_CONFIG_FILE);
    fs::write(&node_config_path, manifest).expect("failed to write node config");
    config::fingerprint::create_codegen_fingerprint(
        &node_config_path,
        Path::new(PEPPYGEN_OUTPUT_PATH),
    );

    let peppy_output_dir = node_dir.join(PEPPY_OUTPUT_DIR);
    fs::create_dir_all(&peppy_output_dir).expect("failed to create peppy output directory");
    fs::write(peppy_output_dir.join("git.hash"), git_hash).expect("failed to write node git hash");
    node_dir
}

/// The `run_cmd` array body of a manifest, one JSON5 string per argument.
fn run_cmd_json5(run_cmd: &[impl AsRef<str>]) -> String {
    run_cmd
        .iter()
        .map(|arg| serde_json::to_string(arg.as_ref()).expect("run_cmd arg should serialize"))
        .collect::<Vec<_>>()
        .join(", ")
}

fn write_node_config(
    nodes_directory: &Path,
    node_name: &str,
    node_tag: &str,
    git_hash: &str,
    run_cmd: &[&str],
) -> PathBuf {
    let run_cmd_json5 = run_cmd_json5(run_cmd);
    install_node_manifest(
        nodes_directory,
        node_name,
        git_hash,
        &format!(
            r#"{{
                peppy_schema: "node/v1",
                manifest: {{
                    name: "{node_name}",
                    tag: "{node_tag}",
                }},
                execution: {{
                    language: "rust",
                    run_cmd: [{run_cmd_json5}]
                }}
            }}"#
        ),
    )
}

/// Registers node directories in the daemon's node cache (`cache/nodes.json5`)
/// so a launcher's `name:tag` deployment sources resolve without a full
/// `repo refresh`.
/// `peppy_root` is the serve emulation's peppy dir root (`serve.temp_dir()`).
/// Call this AFTER the node configs' final bytes are on disk (a pinned add
/// verifies the manifest bytes against the entry's fingerprint) and after any
/// `repo refresh` in the test, which rewrites the cache file.
fn register_repo_caches(peppy_root: impl AsRef<Path>, nodes: &[(&str, &str, &Path)]) {
    let peppy_dirs = daemon_config::consts::PeppyDirs::new(peppy_root.as_ref());
    fs::create_dir_all(peppy_dirs.cache_dir()).expect("failed to create cache dir");

    let node_entries: Vec<serde_json::Value> = nodes
        .iter()
        .map(|(name, tag, dir)| {
            let manifest_path = dir.join(NODE_CONFIG_FILE);
            let bytes = fs::read(&manifest_path).expect("node manifest should exist");
            serde_json::json!({
                "node_name": name,
                "node_tag": tag,
                "sha256": config::fingerprint::fingerprint_for_bytes(&bytes),
                // The origin is serialized from the type `repo refresh`
                // writes, so a change to its shape reaches this fixture.
                "origin": daemon_config::repository::EntryOrigin::Fs { path: manifest_path },
            })
        })
        .collect();
    fs::write(
        core_node::nodes_repo_cache_path(&peppy_dirs),
        serde_json::to_string_pretty(&node_entries).expect("serialize nodes cache"),
    )
    .expect("failed to write nodes.json5");
}

/// Stages a checked-in hub fixture node into a temp nodes directory, ready for
/// [`register_repo_caches`].
///
/// The fixture is parsed into a [`config::node::NodeConfig`], its `execution`
/// block is replaced with the harness conventions, and the config is
/// re-serialized, so the staged manifest carries the fixture's values but not
/// its comments or formatting. `execution` is replaced because a fixture cannot
/// name a run command that outlives the test (the keep-alive path is created
/// per run) and these tests never build a container.
fn stage_hub_fixture_node(
    nodes_directory: &Path,
    fixture_name: &str,
    git_hash: &str,
    run_cmd: &[String],
) -> PathBuf {
    let fixture_path = Path::new("tests/fixtures/hub/nodes")
        .join(fixture_name)
        .join(NODE_CONFIG_FILE);
    let source = fs::read_to_string(&fixture_path).unwrap_or_else(|e| {
        panic!(
            "hub fixture {} should be readable: {e}",
            fixture_path.display()
        )
    });
    let mut node_config: config::node::NodeConfig =
        serde_json5::from_str(&source).expect("hub fixture should parse as a node manifest");
    node_config.execution.build_cmd = None;
    node_config.execution.run_cmd = Some(run_cmd.to_vec());

    install_node_manifest(
        nodes_directory,
        fixture_name,
        git_hash,
        &serde_json5::to_string(&node_config).expect("staged manifest should serialize"),
    )
}

/// Copies checked-in hub pairing and contract documents into a repo directory,
/// so a test can seed exactly the documents its fixtures reference.
fn stage_hub_docs(repo_dir: &Path, pairings: &[&str], contracts: &[&str]) {
    for (kind, names) in [("pairings", pairings), ("contracts", contracts)] {
        for name in names {
            let file = format!("{name}.json5");
            let source = Path::new("tests/fixtures/hub").join(kind).join(&file);
            fs::copy(&source, repo_dir.join(&file))
                .unwrap_or_else(|e| panic!("hub doc {} should copy: {e}", source.display()));
        }
    }
}

fn read_daemon_git_hash(daemon_state_path: &Path) -> String {
    let contents =
        fs::read_to_string(daemon_state_path).expect("daemon state file should be readable");
    let value: serde_json::Value =
        serde_json5::from_str(&contents).expect("daemon state should parse as JSON5");
    value
        .get("git_hash")
        .and_then(|v| v.as_str())
        .filter(|git_hash| !git_hash.is_empty())
        .expect("daemon state should include a non-empty git_hash")
        .to_string()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn node_launch_command_succeed() {
    let serve = ServeCommandEmulation::with_mock()
        .await
        .expect("failed to create serve emulation");
    let shared_messenger = serve.messenger();
    let core_node_name = serve.core_node_name().to_string();
    assert!(
        !core_node_name.is_empty(),
        "core_node_name should not be empty"
    );

    let nodes_dir = tempfile::tempdir().expect("failed to create temp nodes directory");
    let node_a_name = "launch_succeed_node_a";
    let node_b_name = "launch_succeed_node_b";
    let node_tag = "v1";
    let git_hash = read_daemon_git_hash(serve.daemon_state_path());
    let node_a_path = write_node_config(
        nodes_dir.path(),
        node_a_name,
        node_tag,
        &git_hash,
        &["sh", "-c", "exit 0"],
    );

    let ctx = Arc::new(
        AppContext::with_messenger(nodes_dir.path(), Arc::clone(&shared_messenger))
            .with_daemon_state_file(serve.daemon_state_path()),
    );

    let log_capture = LogCapture::new();
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .without_time()
        .with_writer(log_capture.clone())
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);

    NodeCommand {
        command: NodeCommands::Add {
            source: Some(node_a_path.display().to_string()),
            git_ref: None,
            sync: false,
            build: true,
            run: false,
            args: Vec::new(),
            instance_id: None,
            links: Vec::new(),
            vacant_links: Vec::new(),
            idle_timeout: 60,
            max_timeout: 3600,
            force: false,
        },
    }
    .execute(&ctx)
    .expect("node add command should succeed");

    NodeCommand {
        command: NodeCommands::Init {
            node_name: peppy::commands::node::NodeName::new(node_b_name).expect("valid node name"),
            to_dir: None,
            toolchain: Toolchain::Cargo,
            with_container: false,
        },
    }
    .execute(&ctx)
    .expect("node init command should succeed");

    let node_b_path = nodes_dir.path().join(node_b_name);
    let node_b_peppy_json5_path = node_b_path.join(NODE_CONFIG_FILE);
    // The launched instance has to survive the post-launch
    // `instance_count() == 1` check and the explicit stop that follows, so tie
    // it to the test instead of a fixed `sleep`.
    let instances = peppy::test_support::InstanceLifetime::new();
    peppy::test_support::override_run_cmd_while(&node_b_peppy_json5_path, &instances.sentinel());

    let messenger_handle = ctx
        .messenger_handle()
        .expect("messenger handle should be available");

    let response = poll(
        &StackListRequest::new(),
        messenger_handle,
        &core_node_name,
        CALLER_INSTANCE_ID,
        &core_node_name,
        Duration::from_secs(5),
    )
    .await
    .expect("stack_list request should complete");

    let graph: SerializedNodeGraph =
        serde_json::from_str(&response.graph_json).expect("graph_json should parse");

    assert!(
        graph
            .nodes
            .iter()
            .any(|n| n.label().contains(&format!("{node_a_name}:{node_tag}"))),
        "graph should contain node_a before launch. Got: {:?}",
        graph.nodes.iter().map(|n| n.label()).collect::<Vec<_>>()
    );

    let instance_id = "node_b_instance";
    let node_messenger = MessengerHandle::from_shared(Arc::clone(&shared_messenger));
    let _node_ready_handle = listen_for_node_ready(
        &node_messenger,
        &core_node_name,
        instance_id,
        test_node_target(node_b_name),
    )
    .await
    .expect("node ready service should start");
    let _node_health_handle = listen_for_node_health(
        &node_messenger,
        &core_node_name,
        instance_id,
        test_node_target(node_b_name),
    )
    .await
    .expect("node health service should start");
    let (_node_shutdown_handle, node_shutdown_rx) = listen_for_shutdown(
        &node_messenger,
        &core_node_name,
        instance_id,
        test_node_target(node_b_name),
    )
    .await
    .expect("node shutdown service should start");

    // Model a node that actually exits when asked: end the keep-alive as soon
    // as the cooperative shutdown request arrives. Without this the process
    // would sit there until the daemon's force-kill grace elapsed, adding the
    // whole grace period to the stop below.
    tokio::spawn(async move {
        let _ = node_shutdown_rx.await;
        drop(instances);
    });

    let launcher_path = nodes_dir.path().join("peppy_launcher.json5");
    let node_b_path = nodes_dir.path().join(node_b_name);
    let launcher_json5 = format!(
        r#"{{
            peppy_schema: "launcher/v1",
            deployments: [
                {{
                    source: {{ name: "{node_b_name}:{node_tag}" }},
                    instances: [{{ instance_id: "{instance_id}" }}]
                }}
            ]
        }}"#
    );
    fs::write(&launcher_path, launcher_json5).expect("launcher config should be writable");
    register_repo_caches(serve.temp_dir(), &[(node_b_name, node_tag, &node_b_path)]);

    StackCommand {
        command: StackCommands::Launch {
            place: Vec::new(),
            local: false,
            launcher_config_path: launcher_path,
            node_add_idle_timeout_secs: 60,
            node_build_idle_timeout_secs: 60,
            node_run_idle_timeout_secs: 60,
            max_timeout_secs: Some(3600),
        },
    }
    .execute(&ctx)
    .expect("launch command should succeed");

    let response = poll(
        &StackListRequest::new(),
        messenger_handle,
        &core_node_name,
        CALLER_INSTANCE_ID,
        &core_node_name,
        Duration::from_secs(5),
    )
    .await
    .expect("stack_list request should complete after launch");

    let graph: SerializedNodeGraph =
        serde_json::from_str(&response.graph_json).expect("graph_json should parse after launch");

    assert!(
        !graph
            .nodes
            .iter()
            .any(|n| n.label().contains(&format!("{node_a_name}:{node_tag}"))),
        "graph should not contain node_a after launch. Got: {:?}",
        graph.nodes.iter().map(|n| n.label()).collect::<Vec<_>>()
    );

    let node_b = graph.find_node(node_b_name, node_tag).unwrap_or_else(|| {
        panic!(
            "graph should contain node_b after launch. Got: {:?}",
            graph.nodes.iter().map(|n| n.label()).collect::<Vec<_>>()
        )
    });
    assert_eq!(
        node_b.instance_count(),
        1,
        "graph should contain node_b with 1 instance after launch. Got: {:?}",
        graph
            .nodes
            .iter()
            .map(|n| (n.label(), n.instance_count()))
            .collect::<Vec<_>>()
    );

    // TODO we shouldn't need to stop the instances manually. If the core node stops, all child instances pid should stop too
    NodeCommand {
        command: NodeCommands::Stop {
            instance_id: instance_id.to_string(),
        },
    }
    .execute(&ctx)
    .expect("node stop command should succeed");

    let response = poll(
        &StackListRequest::new(),
        messenger_handle,
        &core_node_name,
        CALLER_INSTANCE_ID,
        &core_node_name,
        Duration::from_secs(5),
    )
    .await
    .expect("stack_list request should complete after stop");

    let graph: SerializedNodeGraph =
        serde_json::from_str(&response.graph_json).expect("graph_json should parse after stop");

    let node_b = graph.find_node(node_b_name, node_tag).unwrap_or_else(|| {
        panic!(
            "graph should contain node_b after stop. Got: {:?}",
            graph.nodes.iter().map(|n| n.label()).collect::<Vec<_>>()
        )
    });
    assert_eq!(
        node_b.instance_count(),
        0,
        "graph should contain node_b with 0 instances after stop. Got: {:?}",
        graph
            .nodes
            .iter()
            .map(|n| (n.label(), n.instance_count()))
            .collect::<Vec<_>>()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn node_launch_command_fails_when_node_never_becomes_healthy_and_clears_stack() {
    let serve = ServeCommandEmulation::with_mock()
        .await
        .expect("failed to create serve emulation");
    let shared_messenger = serve.messenger();
    let core_node_name = serve.core_node_name().to_string();
    assert!(
        !core_node_name.is_empty(),
        "core_node_name should not be empty"
    );

    let nodes_dir = tempfile::tempdir().expect("failed to create temp nodes directory");
    let node_a_name = "launch_node_a";
    let node_b_name = "launch_node_b";
    let node_tag = "v1";
    let git_hash = read_daemon_git_hash(serve.daemon_state_path());
    let node_a_path = write_node_config(
        nodes_dir.path(),
        node_a_name,
        node_tag,
        &git_hash,
        &["sh", "-c", "exit 0"],
    );
    let node_b_path = write_node_config(
        nodes_dir.path(),
        node_b_name,
        node_tag,
        &git_hash,
        &["sh", "-c", "exit 0"],
    );

    let ctx = Arc::new(
        AppContext::with_messenger(nodes_dir.path(), Arc::clone(&shared_messenger))
            .with_daemon_state_file(serve.daemon_state_path()),
    );

    let log_capture = LogCapture::new();
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .without_time()
        .with_writer(log_capture.clone())
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);

    NodeCommand {
        command: NodeCommands::Add {
            source: Some(node_a_path.display().to_string()),
            git_ref: None,
            sync: false,
            build: true,
            run: false,
            args: Vec::new(),
            instance_id: None,
            links: Vec::new(),
            vacant_links: Vec::new(),
            idle_timeout: 60,
            max_timeout: 3600,
            force: false,
        },
    }
    .execute(&ctx)
    .expect("node add command should succeed");

    let messenger_handle = ctx
        .messenger_handle()
        .expect("messenger handle should be available");

    let response = poll(
        &StackListRequest::new(),
        messenger_handle,
        &core_node_name,
        CALLER_INSTANCE_ID,
        &core_node_name,
        Duration::from_secs(5),
    )
    .await
    .expect("stack_list request should complete");

    let graph: SerializedNodeGraph =
        serde_json::from_str(&response.graph_json).expect("graph_json should parse");

    assert!(
        graph
            .nodes
            .iter()
            .any(|n| n.label().contains(&format!("{node_a_name}:{node_tag}"))),
        "graph should contain node_a before launch. Got: {:?}",
        graph.nodes.iter().map(|n| n.label()).collect::<Vec<_>>()
    );

    let launcher_path = nodes_dir.path().join("peppy_launcher.json5");
    let launcher_json5 = format!(
        r#"{{
            peppy_schema: "launcher/v1",
            deployments: [
                {{
                    source: {{ name: "{node_b_name}:{node_tag}" }},
                    instances: [{{ instance_id: "node_b_instance" }}]
                }}
            ]
        }}"#
    );
    fs::write(&launcher_path, launcher_json5).expect("launcher config should be writable");
    register_repo_caches(serve.temp_dir(), &[(node_b_name, node_tag, &node_b_path)]);

    let launch_result = StackCommand {
        command: StackCommands::Launch {
            place: Vec::new(),
            local: false,
            launcher_config_path: launcher_path,
            node_add_idle_timeout_secs: 60,
            node_build_idle_timeout_secs: 60,
            node_run_idle_timeout_secs: 60,
            max_timeout_secs: Some(3600),
        },
    }
    .execute(&ctx);

    assert!(
        launch_result.is_err(),
        "launch command should fail because the launched node never becomes healthy"
    );

    let response = poll(
        &StackListRequest::new(),
        messenger_handle,
        &core_node_name,
        CALLER_INSTANCE_ID,
        &core_node_name,
        Duration::from_secs(5),
    )
    .await
    .expect("stack_list request should complete after launch");

    let graph: SerializedNodeGraph =
        serde_json::from_str(&response.graph_json).expect("graph_json should parse after launch");

    // New contract: a launch replaces the whole stack, tearing it down at the
    // clear step, so a failed launch leaves a clean stack with only the root
    // core node. node_a is not rolled back; it is torn down along with the
    // partial new stack. This mirrors the core-node-internal contract test
    // listen_for_launch_configuration_fails_when_one_node_never_becomes_healthy_and_clears_stack.
    assert!(
        !graph
            .nodes
            .iter()
            .any(|n| n.label().contains(&format!("{node_a_name}:{node_tag}"))),
        "node_a should be torn down by the failed launch, not restored. Got: {:?}",
        graph.nodes.iter().map(|n| n.label()).collect::<Vec<_>>()
    );

    assert!(
        !graph
            .nodes
            .iter()
            .any(|n| n.label().contains(&format!("{node_b_name}:{node_tag}"))),
        "node_b should not be present after a failed launch. Got: {:?}",
        graph.nodes.iter().map(|n| n.label()).collect::<Vec<_>>()
    );

    assert_eq!(
        graph.nodes.len(),
        1,
        "only the root core node should remain after a failed launch. Got: {:?}",
        graph.nodes.iter().map(|n| n.label()).collect::<Vec<_>>()
    );
}

/// Common setup for the timeout-firing tests. Returns the launcher path and the temp node
/// peppy.json5, with node_b initialized via `node init` so the caller can `override_*_cmd`
/// before launch. The error message bubbles up via the returned `Result` from
/// `StackCommand::execute`, so we don't need a `LogCapture` here.
struct TimeoutTestHarness {
    ctx: Arc<AppContext>,
    node_b_name: &'static str,
    node_b_path: PathBuf,
    launcher_path: PathBuf,
    node_b_peppy_json5: PathBuf,
    _serve: ServeCommandEmulation,
    // Declared last so it drops last: the nodes dir is removed only after `ctx`
    // and the serve emulation have torn down. Held (not `.keep()`-leaked) so the
    // directory does not survive between test runs.
    _nodes_dir: tempfile::TempDir,
}

async fn setup_timeout_test(node_b_name: &'static str) -> TimeoutTestHarness {
    let serve = ServeCommandEmulation::with_mock()
        .await
        .expect("failed to create serve emulation");
    let shared_messenger = serve.messenger();
    let nodes_dir = tempfile::tempdir().expect("failed to create temp nodes directory");
    // Keep an owned copy of the path for building the config below; the `TempDir`
    // guard itself is moved into the harness (see `_nodes_dir`) so the directory
    // is reclaimed when the test ends instead of leaking across runs.
    let nodes_dir_path = nodes_dir.path().to_path_buf();

    let ctx = Arc::new(
        AppContext::with_messenger(&nodes_dir_path, Arc::clone(&shared_messenger))
            .with_daemon_state_file(serve.daemon_state_path()),
    );

    NodeCommand {
        command: NodeCommands::Init {
            node_name: peppy::commands::node::NodeName::new(node_b_name).expect("valid node name"),
            to_dir: None,
            toolchain: Toolchain::Cargo,
            with_container: false,
        },
    }
    .execute(&ctx)
    .expect("node init command should succeed");

    let node_b_path = nodes_dir_path.join(node_b_name);
    let node_b_peppy_json5 = node_b_path.join(NODE_CONFIG_FILE);
    let launcher_path = nodes_dir_path.join("peppy_launcher.json5");
    let launcher_json5 = format!(
        r#"{{
            peppy_schema: "launcher/v1",
            deployments: [
                {{
                    source: {{ name: "{node_b_name}:v1" }},
                    instances: [{{ instance_id: "node_b_instance" }}]
                }}
            ]
        }}"#
    );
    fs::write(&launcher_path, launcher_json5).expect("launcher config should be writable");

    TimeoutTestHarness {
        ctx,
        node_b_name,
        node_b_path,
        launcher_path,
        node_b_peppy_json5,
        _serve: serve,
        _nodes_dir: nodes_dir,
    }
}

impl TimeoutTestHarness {
    /// Registers node_b in the daemon's node cache. Called by each test
    /// after its `override_*` edit so the recorded fingerprint matches the
    /// bytes the launch materializes.
    fn register_caches(&self) {
        register_repo_caches(
            self._serve.temp_dir(),
            &[(self.node_b_name, "v1", &self.node_b_path)],
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn node_launch_fails_when_node_build_idle_timeout_is_hit() {
    let harness = setup_timeout_test("build_idle_node_b").await;

    override_build_cmd(
        &harness.node_b_peppy_json5,
        vec!["sh".to_string(), "-c".to_string(), "sleep 30".to_string()],
    );
    harness.register_caches();

    let started = Instant::now();
    let result = StackCommand {
        command: StackCommands::Launch {
            place: Vec::new(),
            local: false,
            launcher_config_path: harness.launcher_path.clone(),
            node_add_idle_timeout_secs: 60,
            node_build_idle_timeout_secs: 1,
            node_run_idle_timeout_secs: 60,
            max_timeout_secs: None,
        },
    }
    .execute(&harness.ctx);

    let elapsed = started.elapsed();
    let err_msg = result
        .expect_err("launch should fail when build idle timeout fires")
        .to_string();
    assert!(
        elapsed >= Duration::from_secs(1),
        "build idle timeout (1s) cannot fire earlier than its configured duration; took {elapsed:?}",
    );
    assert!(
        elapsed < Duration::from_secs(20),
        "launch should fail in well under 30s; took {elapsed:?}",
    );
    assert!(
        err_msg.contains("timeout") && err_msg.contains("build"),
        "error message should mention 'timeout' and 'build' on build idle failure. Got: {err_msg}",
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn node_launch_fails_when_node_run_idle_timeout_is_hit() {
    let harness = setup_timeout_test("run_idle_node_b").await;

    // Silent run command that never produces output and never becomes ready.
    override_run_cmd_silent(&harness.node_b_peppy_json5);
    harness.register_caches();

    let started = Instant::now();
    let result = StackCommand {
        command: StackCommands::Launch {
            place: Vec::new(),
            local: false,
            launcher_config_path: harness.launcher_path.clone(),
            node_add_idle_timeout_secs: 60,
            node_build_idle_timeout_secs: 60,
            node_run_idle_timeout_secs: 1,
            max_timeout_secs: None,
        },
    }
    .execute(&harness.ctx);

    let elapsed = started.elapsed();
    let err_msg = result
        .expect_err("launch should fail when run idle timeout fires")
        .to_string();
    assert!(
        elapsed >= Duration::from_secs(1),
        "run idle timeout (1s) cannot fire earlier than its configured duration; took {elapsed:?}",
    );
    assert!(
        elapsed < Duration::from_secs(20),
        "launch should fail in well under 30s; took {elapsed:?}",
    );
    assert!(
        err_msg.contains("timeout") && err_msg.contains("run"),
        "error message should mention 'timeout' and 'run' on run idle failure. Got: {err_msg}",
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn node_launch_fails_when_max_timeout_is_hit() {
    let harness = setup_timeout_test("max_timeout_node_b").await;

    // Continuous output for ~10s so idle never fires, but `max_timeout_secs=2` does.
    // POSIX-portable shell loop (`seq` is bash-only).
    override_build_cmd(
        &harness.node_b_peppy_json5,
        vec![
            "sh".to_string(),
            "-c".to_string(),
            "i=0; while [ $i -lt 50 ]; do echo step-$i; i=$((i+1)); sleep 0.2; done".to_string(),
        ],
    );
    harness.register_caches();

    let started = Instant::now();
    let result = StackCommand {
        command: StackCommands::Launch {
            place: Vec::new(),
            local: false,
            launcher_config_path: harness.launcher_path.clone(),
            node_add_idle_timeout_secs: 60,
            node_build_idle_timeout_secs: 60,
            node_run_idle_timeout_secs: 60,
            max_timeout_secs: Some(2),
        },
    }
    .execute(&harness.ctx);

    let elapsed = started.elapsed();
    let err_msg = result
        .expect_err("launch should fail when max launch timeout fires")
        .to_string();
    assert!(
        elapsed >= Duration::from_secs(2),
        "max timeout (2s) cannot fire earlier than its configured duration; took {elapsed:?}",
    );
    assert!(
        elapsed < Duration::from_secs(20),
        "launch should fail in well under 30s; took {elapsed:?}",
    );
    assert!(
        err_msg.contains("timeout") && err_msg.contains("max"),
        "error message should mention 'timeout' and 'max' on max launch failure. Got: {err_msg}",
    );
}

/// Writes a minimal peppy.json5 with an explicit `run_cmd` (so the daemon's
/// build phase is skipped), optional `depends_on` / `implements` manifest
/// blocks, and an optional top-level `interfaces` block. Mirrors
/// `write_node_config` but accepts run_cmd as owned strings and
/// manifest/interfaces extensions for tests that need to exercise binding
/// resolution against `manifest.implements`.
#[allow(clippy::too_many_arguments)]
fn write_node_config_for_helper(
    nodes_directory: &Path,
    node_name: &str,
    node_tag: &str,
    git_hash: &str,
    run_cmd: &[String],
    depends_on_json5: Option<&str>,
    implements_json5: Option<&str>,
    interfaces_json5: Option<&str>,
) -> PathBuf {
    let run_cmd_json5 = run_cmd_json5(run_cmd);
    let manifest_extra = implements_json5
        .map(|implements| format!(",\n            implements: {implements}"))
        .unwrap_or_default()
        + &depends_on_json5
            .map(|deps| format!(",\n            depends_on: {deps}"))
            .unwrap_or_default();
    let interfaces_extra = interfaces_json5
        .map(|ifaces| format!(",\n            interfaces: {ifaces}"))
        .unwrap_or_default();
    let body = format!(
        r#"{{
            peppy_schema: "node/v1",
            manifest: {{
                name: "{node_name}",
                tag: "{node_tag}"{manifest_extra}
            }}{interfaces_extra},
            execution: {{
                language: "rust",
                run_cmd: [{run_cmd_json5}]
            }}
        }}"#
    );
    install_node_manifest(nodes_directory, node_name, git_hash, &body)
}

/// Regression for the launcher's `bindings` field actually wiring
/// producer `link_ids` at launch time. The dummy `sh` run_cmd dumps the
/// `RuntimeConfig` it received from the daemon (via the
/// `PEPPY_RUNTIME_CONFIG` env var, which points at a JSON5 file) to a
/// known location; the test then parses that dump and asserts the
/// producer's `link_ids` vec was populated from the consumer's binding.
/// Without the fix the vec stays empty (defaulted to `_` later by the
/// runtime), so this is the boundary that surfaces the silent-loss bug.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stack_launch_populates_link_ids_from_launcher_bindings() {
    // Instances must stay in the stack until the assertions below have read
    // them; a fixed `sleep` would make that a race against machine load.
    let instances = peppy::test_support::InstanceLifetime::new();
    let serve = ServeCommandEmulation::with_zenoh()
        .await
        .expect("failed to create zenoh serve emulation");
    let core_node_name = serve.core_node_name().to_string();
    assert!(
        !core_node_name.is_empty(),
        "core_node_name should not be empty"
    );

    let nodes_dir = tempfile::tempdir().expect("failed to create temp nodes directory");
    let dump_dir = tempfile::tempdir().expect("failed to create temp dump directory");
    let producer_dump = dump_dir.path().join("producer.json5");
    let consumer_dump = dump_dir.path().join("consumer.json5");

    let producer_name = "binding_producer";
    let consumer_name = "binding_consumer";
    let node_tag = "v1";
    let producer_instance_id = "binding_prod_inst";
    let consumer_instance_id = "binding_cons_inst";
    let link_id = "main";
    let git_hash = read_daemon_git_hash(serve.daemon_state_path());

    // Each instance's run_cmd snapshots its own `PEPPY_RUNTIME_CONFIG`
    // file to the test-owned dump location, then sleeps long enough that
    // the test process has time to read the snapshot before issuing Stop.
    let producer_run_cmd = vec![
        "sh".to_string(),
        "-c".to_string(),
        format!(
            "cp \"$PEPPY_RUNTIME_CONFIG\" \"{}\" && {}",
            producer_dump.display(),
            instances.keep_alive_script(),
        ),
    ];
    let producer_path = write_node_config_for_helper(
        nodes_dir.path(),
        producer_name,
        node_tag,
        &git_hash,
        &producer_run_cmd,
        None,
        None,
        None,
    );

    let consumer_run_cmd = vec![
        "sh".to_string(),
        "-c".to_string(),
        format!(
            "cp \"$PEPPY_RUNTIME_CONFIG\" \"{}\" && {}",
            consumer_dump.display(),
            instances.keep_alive_script(),
        ),
    ];
    let consumer_depends_on = format!(
        r#"{{ nodes: [{{ name: "{producer_name}", tag: "{node_tag}", link_id: "{link_id}" }}] }}"#
    );
    let consumer_path = write_node_config_for_helper(
        nodes_dir.path(),
        consumer_name,
        node_tag,
        &git_hash,
        &consumer_run_cmd,
        Some(&consumer_depends_on),
        None,
        None,
    );

    let ctx = Arc::new(
        AppContext::with_messenger(nodes_dir.path(), Arc::clone(&serve.messenger()))
            .with_daemon_state_file(serve.daemon_state_path()),
    );

    // The dummy `sh` subprocess does not expose `node_ready`/`node_health`/
    // `shutdown`, so impersonate them from the test process for each
    // instance the launcher will spawn. The daemon's `wait_for_ready_signal`
    // queries via Zenoh with a wildcard `link_id`, so the queryables
    // declared here (with default link_ids) still match.
    let node_messenger = MessengerHandle::from_shared(Arc::clone(&serve.messenger()));
    let _ready_producer = listen_for_node_ready(
        &node_messenger,
        &core_node_name,
        producer_instance_id,
        test_node_target(producer_name),
    )
    .await
    .expect("producer ready service should start");
    let _health_producer = listen_for_node_health(
        &node_messenger,
        &core_node_name,
        producer_instance_id,
        test_node_target(producer_name),
    )
    .await
    .expect("producer health service should start");
    let (_shutdown_producer, _) = listen_for_shutdown(
        &node_messenger,
        &core_node_name,
        producer_instance_id,
        test_node_target(producer_name),
    )
    .await
    .expect("producer shutdown service should start");
    let _ready_consumer = listen_for_node_ready(
        &node_messenger,
        &core_node_name,
        consumer_instance_id,
        test_node_target(consumer_name),
    )
    .await
    .expect("consumer ready service should start");
    let _health_consumer = listen_for_node_health(
        &node_messenger,
        &core_node_name,
        consumer_instance_id,
        test_node_target(consumer_name),
    )
    .await
    .expect("consumer health service should start");
    let (_shutdown_consumer, _) = listen_for_shutdown(
        &node_messenger,
        &core_node_name,
        consumer_instance_id,
        test_node_target(consumer_name),
    )
    .await
    .expect("consumer shutdown service should start");

    let launcher_path = nodes_dir.path().join("peppy_launcher.json5");
    let launcher_json5 = format!(
        r#"{{
            peppy_schema: "launcher/v1",
            deployments: [
                {{
                    source: {{ name: "{producer_name}:{node_tag}" }},
                    instances: [{{ instance_id: "{producer_instance_id}" }}]
                }},
                {{
                    source: {{ name: "{consumer_name}:{node_tag}" }},
                    instances: [{{
                        instance_id: "{consumer_instance_id}",
                        links: {{ {link_id}: "{producer_instance_id}" }}
                    }}]
                }}
            ]
        }}"#
    );
    fs::write(&launcher_path, launcher_json5).expect("launcher config should be writable");
    register_repo_caches(
        serve.temp_dir(),
        &[
            (producer_name, node_tag, &producer_path),
            (consumer_name, node_tag, &consumer_path),
        ],
    );

    StackCommand {
        command: StackCommands::Launch {
            place: Vec::new(),
            local: false,
            launcher_config_path: launcher_path,
            node_add_idle_timeout_secs: 60,
            node_build_idle_timeout_secs: 60,
            node_run_idle_timeout_secs: 60,
            max_timeout_secs: Some(120),
        },
    }
    .execute(&ctx)
    .expect("launch command should succeed");

    // The `sh` wrappers copy the runtime config before sleeping. Poll
    // BOTH dumps before stopping anything: stopping the consumer before
    // its dump has flushed would race with the negative-case assertion
    // below.
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut producer_config: Option<config::runtime::RuntimeConfig> = None;
    let mut consumer_config: Option<config::runtime::RuntimeConfig> = None;
    while Instant::now() < deadline {
        if producer_config.is_none()
            && let Ok(content) = fs::read_to_string(&producer_dump)
            && let Ok(cfg) = serde_json5::from_str::<config::runtime::RuntimeConfig>(&content)
        {
            producer_config = Some(cfg);
        }
        if consumer_config.is_none()
            && let Ok(content) = fs::read_to_string(&consumer_dump)
            && let Ok(cfg) = serde_json5::from_str::<config::runtime::RuntimeConfig>(&content)
        {
            consumer_config = Some(cfg);
        }
        if producer_config.is_some() && consumer_config.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // The dumps are captured, so nothing below needs these instances. Ending
    // the keep-alive first lets each Stop observe an already-exited process
    // instead of waiting out the daemon's force-kill grace.
    drop(instances);
    for instance_id in [consumer_instance_id, producer_instance_id] {
        let _ = NodeCommand {
            command: NodeCommands::Stop {
                instance_id: instance_id.to_string(),
            },
        }
        .execute(&ctx);
    }

    let producer_config = producer_config.unwrap_or_else(|| {
        panic!(
            "producer runtime config dump never appeared / parsed at {}",
            producer_dump.display()
        )
    });
    assert_eq!(
        producer_config.node_instance.instance_id.as_str(),
        producer_instance_id,
    );
    assert!(
        producer_config.node_instance.slot_bindings.is_empty(),
        "producers do not declare slot_bindings; the binding lives on the consumer",
    );

    let consumer_config = consumer_config.unwrap_or_else(|| {
        panic!(
            "consumer runtime config dump never appeared / parsed at {}",
            consumer_dump.display()
        )
    });
    assert_eq!(
        consumer_config.node_instance.slot_bindings.get(link_id),
        Some(&config::runtime::BoundProducers::from(
            config::runtime::ProducerRef::new(&core_node_name, producer_instance_id),
        )),
        "the launcher's binding `{link_id} -> {producer_instance_id}` should be present on the \
         consumer's runtime config as a Pinned slot binding stamped with the daemon's core_node",
    );
}

/// A `one_or_more` slot bound to two producer instances through the real
/// daemon launch path: the launcher's array binding must reach the
/// consumer's runtime config as the ordered two-member producer set, each
/// member stamped with the daemon's core_node. This is the daemon-side
/// twin of the validator unit tests: it proves the array shape survives
/// launcher parse, plan validation, and boot-config serialization.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stack_launch_binds_multi_cardinality_slot_to_ordered_producer_set() {
    // Instances must stay in the stack until the assertions below have read
    // them; a fixed `sleep` would make that a race against machine load.
    let instances = peppy::test_support::InstanceLifetime::new();
    let serve = ServeCommandEmulation::with_zenoh()
        .await
        .expect("failed to create zenoh serve emulation");
    let core_node_name = serve.core_node_name().to_string();

    let nodes_dir = tempfile::tempdir().expect("failed to create temp nodes directory");
    let dump_dir = tempfile::tempdir().expect("failed to create temp dump directory");
    let consumer_dump = dump_dir.path().join("consumer.json5");

    let producer_name = "multi_binding_producer";
    let consumer_name = "multi_binding_consumer";
    let node_tag = "v1";
    let front_instance_id = "front_camera";
    let rear_instance_id = "rear_camera";
    let consumer_instance_id = "multi_cons_inst";
    let link_id = "cameras";
    let git_hash = read_daemon_git_hash(serve.daemon_state_path());

    let producer_run_cmd = vec![
        "sh".to_string(),
        "-c".to_string(),
        instances.keep_alive_script(),
    ];
    let producer_path = write_node_config_for_helper(
        nodes_dir.path(),
        producer_name,
        node_tag,
        &git_hash,
        &producer_run_cmd,
        None,
        None,
        None,
    );

    let consumer_run_cmd = vec![
        "sh".to_string(),
        "-c".to_string(),
        format!(
            "cp \"$PEPPY_RUNTIME_CONFIG\" \"{}\" && {}",
            consumer_dump.display(),
            instances.keep_alive_script(),
        ),
    ];
    let consumer_depends_on = format!(
        r#"{{ nodes: [{{ name: "{producer_name}", tag: "{node_tag}", link_id: "{link_id}", cardinality: "one_or_more" }}] }}"#
    );
    let consumer_path = write_node_config_for_helper(
        nodes_dir.path(),
        consumer_name,
        node_tag,
        &git_hash,
        &consumer_run_cmd,
        Some(&consumer_depends_on),
        None,
        None,
    );

    let ctx = Arc::new(
        AppContext::with_messenger(nodes_dir.path(), Arc::clone(&serve.messenger()))
            .with_daemon_state_file(serve.daemon_state_path()),
    );

    // Impersonate the framework services the dummy `sh` subprocesses do
    // not expose, for all three spawned instances.
    let node_messenger = MessengerHandle::from_shared(Arc::clone(&serve.messenger()));
    let mut service_guards = Vec::new();
    for (node_name, instance_id) in [
        (producer_name, front_instance_id),
        (producer_name, rear_instance_id),
        (consumer_name, consumer_instance_id),
    ] {
        let ready = listen_for_node_ready(
            &node_messenger,
            &core_node_name,
            instance_id,
            test_node_target(node_name),
        )
        .await
        .expect("ready service should start");
        let health = listen_for_node_health(
            &node_messenger,
            &core_node_name,
            instance_id,
            test_node_target(node_name),
        )
        .await
        .expect("health service should start");
        let (shutdown, _) = listen_for_shutdown(
            &node_messenger,
            &core_node_name,
            instance_id,
            test_node_target(node_name),
        )
        .await
        .expect("shutdown service should start");
        service_guards.push((ready, health, shutdown));
    }

    // The binding array deliberately lists `rear` before `front` so the
    // order assertion below cannot pass by accident (BTreeMap iteration or
    // spawn order would both yield `front` first).
    let launcher_path = nodes_dir.path().join("peppy_launcher.json5");
    let launcher_json5 = format!(
        r#"{{
            peppy_schema: "launcher/v1",
            deployments: [
                {{
                    source: {{ name: "{producer_name}:{node_tag}" }},
                    instances: [
                        {{ instance_id: "{front_instance_id}" }},
                        {{ instance_id: "{rear_instance_id}" }}
                    ]
                }},
                {{
                    source: {{ name: "{consumer_name}:{node_tag}" }},
                    instances: [{{
                        instance_id: "{consumer_instance_id}",
                        links: {{ {link_id}: ["{rear_instance_id}", "{front_instance_id}"] }}
                    }}]
                }}
            ]
        }}"#
    );
    fs::write(&launcher_path, launcher_json5).expect("launcher config should be writable");
    register_repo_caches(
        serve.temp_dir(),
        &[
            (producer_name, node_tag, &producer_path),
            (consumer_name, node_tag, &consumer_path),
        ],
    );

    StackCommand {
        command: StackCommands::Launch {
            place: Vec::new(),
            local: false,
            launcher_config_path: launcher_path,
            node_add_idle_timeout_secs: 60,
            node_build_idle_timeout_secs: 60,
            node_run_idle_timeout_secs: 60,
            max_timeout_secs: Some(120),
        },
    }
    .execute(&ctx)
    .expect("launch command should succeed");

    let deadline = Instant::now() + Duration::from_secs(5);
    let mut consumer_config: Option<config::runtime::RuntimeConfig> = None;
    while Instant::now() < deadline {
        if let Ok(content) = fs::read_to_string(&consumer_dump)
            && let Ok(cfg) = serde_json5::from_str::<config::runtime::RuntimeConfig>(&content)
        {
            consumer_config = Some(cfg);
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // The dumps are captured, so nothing below needs these instances. Ending
    // the keep-alive first lets each Stop observe an already-exited process
    // instead of waiting out the daemon's force-kill grace three times over.
    drop(instances);
    for instance_id in [consumer_instance_id, front_instance_id, rear_instance_id] {
        let _ = NodeCommand {
            command: NodeCommands::Stop {
                instance_id: instance_id.to_string(),
            },
        }
        .execute(&ctx);
    }

    let consumer_config = consumer_config.unwrap_or_else(|| {
        panic!(
            "consumer runtime config dump never appeared / parsed at {}",
            consumer_dump.display()
        )
    });
    let expected = config::runtime::BoundProducers::try_from(vec![
        config::runtime::ProducerRef::new(&core_node_name, rear_instance_id),
        config::runtime::ProducerRef::new(&core_node_name, front_instance_id),
    ])
    .expect("distinct producers");
    assert_eq!(
        consumer_config.node_instance.slot_bindings.get(link_id),
        Some(&expected),
        "the launcher's array binding must reach the consumer's boot config as the \
         ordered two-member set, in binding declaration order",
    );
}

/// The value-shape rule at the daemon boundary: an array binding on a
/// default-cardinality (`one`) slot must fail the launch at plan
/// validation, before any node is added, built, or spawned.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stack_launch_rejects_array_binding_on_a_one_slot() {
    // Instances must stay in the stack until the assertions below have read
    // them; a fixed `sleep` would make that a race against machine load.
    let instances = peppy::test_support::InstanceLifetime::new();
    let serve = ServeCommandEmulation::with_mock()
        .await
        .expect("failed to create serve emulation");
    let core_node_name = serve.core_node_name().to_string();
    assert!(!core_node_name.is_empty());

    let nodes_dir = tempfile::tempdir().expect("failed to create temp nodes directory");
    let producer_name = "one_slot_producer";
    let consumer_name = "one_slot_consumer";
    let node_tag = "v1";
    let git_hash = read_daemon_git_hash(serve.daemon_state_path());

    let run_cmd = vec![
        "sh".to_string(),
        "-c".to_string(),
        instances.keep_alive_script(),
    ];
    let producer_path = write_node_config_for_helper(
        nodes_dir.path(),
        producer_name,
        node_tag,
        &git_hash,
        &run_cmd,
        None,
        None,
        None,
    );
    // No `cardinality` on the slot: the default is `one`.
    let consumer_depends_on = format!(
        r#"{{ nodes: [{{ name: "{producer_name}", tag: "{node_tag}", link_id: "main" }}] }}"#
    );
    let consumer_path = write_node_config_for_helper(
        nodes_dir.path(),
        consumer_name,
        node_tag,
        &git_hash,
        &run_cmd,
        Some(&consumer_depends_on),
        None,
        None,
    );

    let ctx = Arc::new(
        AppContext::with_messenger(nodes_dir.path(), Arc::clone(&serve.messenger()))
            .with_daemon_state_file(serve.daemon_state_path()),
    );

    let launcher_path = nodes_dir.path().join("peppy_launcher.json5");
    let launcher_json5 = format!(
        r#"{{
            peppy_schema: "launcher/v1",
            deployments: [
                {{
                    source: {{ name: "{producer_name}:{node_tag}" }},
                    instances: [{{ instance_id: "solo_prod" }}]
                }},
                {{
                    source: {{ name: "{consumer_name}:{node_tag}" }},
                    instances: [{{
                        instance_id: "solo_cons",
                        links: {{ main: ["solo_prod"] }}
                    }}]
                }}
            ]
        }}"#
    );
    fs::write(&launcher_path, launcher_json5).expect("launcher config should be writable");
    register_repo_caches(
        serve.temp_dir(),
        &[
            (producer_name, node_tag, &producer_path),
            (consumer_name, node_tag, &consumer_path),
        ],
    );

    let err = StackCommand {
        command: StackCommands::Launch {
            place: Vec::new(),
            local: false,
            launcher_config_path: launcher_path,
            node_add_idle_timeout_secs: 60,
            node_build_idle_timeout_secs: 60,
            node_run_idle_timeout_secs: 60,
            max_timeout_secs: Some(120),
        },
    }
    .execute(&ctx)
    .expect_err("an array binding on a `one` slot must fail the launch");
    let msg = err.to_string();
    assert!(
        msg.contains("main") && msg.contains("cardinality") && msg.contains("one"),
        "error should name the slot and the cardinality rule: {msg}"
    );
}

/// Stack-wide `instance_id` uniqueness (spec rule 7): two instances
/// anywhere in the launcher, even across different `(node_name,
/// node_tag)` pairs, sharing an `instance_id` must fail at the parse
/// stage, before any node is added, built, or spawned. The binding
/// model addresses producers by raw `instance_id` so a duplicate
/// would make `--link KEY@id` ambiguous.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stack_launch_rejects_stack_wide_duplicate_instance_id() {
    let serve = ServeCommandEmulation::with_mock()
        .await
        .expect("failed to create serve emulation");
    let shared_messenger = serve.messenger();
    let core_node_name = serve.core_node_name().to_string();

    let nodes_dir = tempfile::tempdir().expect("failed to create temp nodes directory");
    let camera_name = "stackdup_camera";
    let lidar_name = "stackdup_lidar";
    let node_tag = "v1";
    let git_hash = read_daemon_git_hash(serve.daemon_state_path());

    let camera_path = write_node_config(
        nodes_dir.path(),
        camera_name,
        node_tag,
        &git_hash,
        &["sh", "-c", "sleep 30"],
    );
    let lidar_path = write_node_config(
        nodes_dir.path(),
        lidar_name,
        node_tag,
        &git_hash,
        &["sh", "-c", "sleep 30"],
    );

    let ctx = Arc::new(
        AppContext::with_messenger(nodes_dir.path(), Arc::clone(&shared_messenger))
            .with_daemon_state_file(serve.daemon_state_path()),
    );

    let log_capture = LogCapture::new();
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .without_time()
        .with_writer(log_capture.clone())
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);

    // Two completely separate node types both claiming `shared_inst`
    // as their instance_id. Under the new spec, this is rejected at
    // the launcher level: instance_ids must be unique across the
    // entire stack, not merely within a `(node_name, node_tag)` group.
    let launcher_path = nodes_dir.path().join("peppy_launcher.json5");
    let launcher_json5 = format!(
        r#"{{
            peppy_schema: "launcher/v1",
            deployments: [
                {{
                    source: {{ name: "{camera_name}:{node_tag}" }},
                    instances: [{{ instance_id: "shared_inst" }}]
                }},
                {{
                    source: {{ name: "{lidar_name}:{node_tag}" }},
                    instances: [{{ instance_id: "shared_inst" }}]
                }}
            ]
        }}"#
    );
    fs::write(&launcher_path, launcher_json5).expect("launcher config should be writable");
    register_repo_caches(
        serve.temp_dir(),
        &[
            (camera_name, node_tag, &camera_path),
            (lidar_name, node_tag, &lidar_path),
        ],
    );

    let result = StackCommand {
        command: StackCommands::Launch {
            place: Vec::new(),
            local: false,
            launcher_config_path: launcher_path,
            node_add_idle_timeout_secs: 60,
            node_build_idle_timeout_secs: 60,
            node_run_idle_timeout_secs: 60,
            max_timeout_secs: Some(3600),
        },
    }
    .execute(&ctx);

    let err_msg = result
        .expect_err("launch must fail on stack-wide duplicate instance_id")
        .to_string();
    assert!(
        err_msg.contains("shared_inst"),
        "error should name the colliding instance_id. Got:\n{err_msg}"
    );
    assert!(
        err_msg.contains(camera_name) && err_msg.contains(lidar_name),
        "error should name both colliding nodes. Got:\n{err_msg}"
    );

    // No spawn side-effect: neither node should appear in the stack.
    let messenger_handle = ctx
        .messenger_handle()
        .expect("messenger handle should be available");
    let response = poll(
        &StackListRequest::new(),
        messenger_handle,
        &core_node_name,
        CALLER_INSTANCE_ID,
        &core_node_name,
        Duration::from_secs(5),
    )
    .await
    .expect("stack_list request should complete");
    let graph: SerializedNodeGraph =
        serde_json::from_str(&response.graph_json).expect("graph_json should parse");
    assert!(
        !graph
            .nodes
            .iter()
            .any(|n| n.name == camera_name || n.name == lidar_name),
        "rejected launcher must not have added or spawned anything. Graph: {:?}",
        graph.nodes.iter().map(|n| n.label()).collect::<Vec<_>>()
    );
}

/// Writes a minimal `peppy_schema: "contract/v1"` document at `path` with
/// a single `video_stream` topic. Used by the implementation-binding
/// integration tests to materialize the contract document on disk
/// alongside the producer/consumer node configs that reference it.
fn write_contract_v1_doc(path: &Path, name: &str, tag: &str) {
    write_contract_v1_doc_with_topic(
        path,
        name,
        tag,
        "video_stream",
        r#"{
            width: "u32",
            height: "u32",
            encoding: "string"
        }"#,
    );
}

/// Like [`write_contract_v1_doc`] but parameterized on the single topic
/// name and its `message_format` body. The bidirectional bindings test
/// uses this to materialize two distinct per-direction contracts
/// (`joint_states` and `joint_commands`) rather than the default
/// `video_stream` shape.
fn write_contract_v1_doc_with_topic(
    path: &Path,
    name: &str,
    tag: &str,
    topic_name: &str,
    message_format_json5: &str,
) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("failed to create contract parent dir");
    }
    let body = format!(
        r#"{{
            peppy_schema: "contract/v1",
            manifest: {{ name: "{name}", tag: "{tag}" }},
            interfaces: {{
                topics: [
                    {{
                        name: "{topic_name}",
                        qos_profile: "sensor_data",
                        message_format: {message_format_json5}
                    }}
                ]
            }}
        }}"#
    );
    fs::write(path, body).expect("failed to write contract/v1 doc");
}

/// End-to-end check that a pinned contract binding resolves against
/// the producer's `manifest.implements` declaration. Exercises a real
/// `peppy_schema: "contract/v1"` document on disk alongside a `node/v1`
/// producer implementing the contract and a `node/v1` consumer declaring
/// the contract dep; pairs the unit-level binding validator tests
/// with the full launch pipeline (cache resolution + daemon node-add).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stack_launch_resolves_implements_binding_with_real_contract_doc() {
    // Instances must stay in the stack until the assertions below have read
    // them; a fixed `sleep` would make that a race against machine load.
    let instances = peppy::test_support::InstanceLifetime::new();
    let serve = ServeCommandEmulation::with_zenoh()
        .await
        .expect("failed to create zenoh serve emulation");
    let core_node_name = serve.core_node_name().to_string();

    let nodes_dir = tempfile::tempdir().expect("failed to create temp nodes directory");
    let dump_dir = tempfile::tempdir().expect("failed to create temp dump directory");
    let contract_repo_dir = tempfile::tempdir().expect("failed to create temp contract repo");
    let consumer_dump = dump_dir.path().join("consumer.json5");

    let contract_name = "depth_camera_iface";
    let contract_tag = "v1";
    let producer_name = "realsense_d405";
    let consumer_name = "video_reconstruction";
    let node_tag = "v1";
    let producer_instance_id = "depth_cam_inst1";
    let consumer_instance_id = "video_rec_1";
    let link_id = "rear_camera";
    let git_hash = read_daemon_git_hash(serve.daemon_state_path());

    // Materialize the contract document on disk and register
    // the containing directory as an fs-type repo. The launcher binding
    // validator only inspects `implements` claims, but the daemon's
    // node-add path also resolves the contract document from cache
    // for consumers declaring `depends_on.contracts`; without the
    // repo refresh the consumer node-add would fail before
    // `validate_bindings` ever runs.
    write_contract_v1_doc(
        &contract_repo_dir
            .path()
            .join("depth_camera_iface/peppy.json5"),
        contract_name,
        contract_tag,
    );
    super::common::publish_repo_index(contract_repo_dir.path());
    let conf_dir = serve.temp_dir().join("conf");
    fs::create_dir_all(&conf_dir).expect("create conf dir");
    let repos_content = serde_json::to_string_pretty(&serde_json::json!([
        { "id": 1, "type": "fs", "path": contract_repo_dir.path().to_string_lossy() }
    ]))
    .expect("serialize repos");
    fs::write(conf_dir.join("repositories.json5"), repos_content).expect("write repos");

    let producer_run_cmd = vec![
        "sh".to_string(),
        "-c".to_string(),
        instances.keep_alive_script(),
    ];
    let producer_implements =
        format!(r#"[{{ name: "{contract_name}", tag: "{contract_tag}", link_id: "cam" }}]"#);
    let producer_interfaces = r#"{
            topics: { emits: [{ link_id: "cam", name: "video_stream" }] }
        }"#;
    let producer_path = write_node_config_for_helper(
        nodes_dir.path(),
        producer_name,
        node_tag,
        &git_hash,
        &producer_run_cmd,
        None,
        Some(&producer_implements),
        Some(producer_interfaces),
    );

    let consumer_run_cmd = vec![
        "sh".to_string(),
        "-c".to_string(),
        format!(
            "cp \"$PEPPY_RUNTIME_CONFIG\" \"{}\" && {}",
            consumer_dump.display(),
            instances.keep_alive_script(),
        ),
    ];
    let consumer_depends_on = format!(
        r#"{{
            nodes: [],
            contracts: [{{
                name: "{contract_name}",
                tag: "{contract_tag}",
                link_id: "{link_id}"
            }}]
        }}"#
    );
    let consumer_path = write_node_config_for_helper(
        nodes_dir.path(),
        consumer_name,
        node_tag,
        &git_hash,
        &consumer_run_cmd,
        Some(&consumer_depends_on),
        None,
        None,
    );

    let ctx = Arc::new(
        AppContext::with_messenger(nodes_dir.path(), Arc::clone(&serve.messenger()))
            .with_daemon_state_file(serve.daemon_state_path()),
    );

    RepoCommand {
        command: RepoCommands::Refresh,
    }
    .execute(&ctx)
    .expect("repo refresh should populate contract cache");

    let node_messenger = MessengerHandle::from_shared(Arc::clone(&serve.messenger()));
    let _ready_producer = listen_for_node_ready(
        &node_messenger,
        &core_node_name,
        producer_instance_id,
        test_node_target(producer_name),
    )
    .await
    .expect("producer ready service should start");
    let _health_producer = listen_for_node_health(
        &node_messenger,
        &core_node_name,
        producer_instance_id,
        test_node_target(producer_name),
    )
    .await
    .expect("producer health service should start");
    let (_shutdown_producer, _) = listen_for_shutdown(
        &node_messenger,
        &core_node_name,
        producer_instance_id,
        test_node_target(producer_name),
    )
    .await
    .expect("producer shutdown service should start");
    let _ready_consumer = listen_for_node_ready(
        &node_messenger,
        &core_node_name,
        consumer_instance_id,
        test_node_target(consumer_name),
    )
    .await
    .expect("consumer ready service should start");
    let _health_consumer = listen_for_node_health(
        &node_messenger,
        &core_node_name,
        consumer_instance_id,
        test_node_target(consumer_name),
    )
    .await
    .expect("consumer health service should start");
    let (_shutdown_consumer, _) = listen_for_shutdown(
        &node_messenger,
        &core_node_name,
        consumer_instance_id,
        test_node_target(consumer_name),
    )
    .await
    .expect("consumer shutdown service should start");

    let launcher_path = nodes_dir.path().join("peppy_launcher.json5");
    let launcher_json5 = format!(
        r#"{{
            peppy_schema: "launcher/v1",
            deployments: [
                {{
                    source: {{ name: "{producer_name}:{node_tag}" }},
                    instances: [{{ instance_id: "{producer_instance_id}" }}]
                }},
                {{
                    source: {{ name: "{consumer_name}:{node_tag}" }},
                    instances: [{{
                        instance_id: "{consumer_instance_id}",
                        links: {{ {link_id}: "{producer_instance_id}" }}
                    }}]
                }}
            ]
        }}"#
    );
    fs::write(&launcher_path, launcher_json5).expect("launcher config should be writable");
    // After any refresh in this test: refresh rewrites nodes.json5, so
    // registration must come later to survive.
    register_repo_caches(
        serve.temp_dir(),
        &[
            (producer_name, node_tag, &producer_path),
            (consumer_name, node_tag, &consumer_path),
        ],
    );

    StackCommand {
        command: StackCommands::Launch {
            place: Vec::new(),
            local: false,
            launcher_config_path: launcher_path,
            node_add_idle_timeout_secs: 60,
            node_build_idle_timeout_secs: 60,
            node_run_idle_timeout_secs: 60,
            max_timeout_secs: Some(120),
        },
    }
    .execute(&ctx)
    .expect("launch should succeed with implementing producer");

    let deadline = Instant::now() + Duration::from_secs(5);
    let mut consumer_config: Option<config::runtime::RuntimeConfig> = None;
    while Instant::now() < deadline {
        if let Ok(content) = fs::read_to_string(&consumer_dump)
            && let Ok(cfg) = serde_json5::from_str::<config::runtime::RuntimeConfig>(&content)
        {
            consumer_config = Some(cfg);
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // The dumps are captured, so nothing below needs these instances. Ending
    // the keep-alive first lets each Stop observe an already-exited process
    // instead of waiting out the daemon's force-kill grace.
    drop(instances);
    for instance_id in [consumer_instance_id, producer_instance_id] {
        let _ = NodeCommand {
            command: NodeCommands::Stop {
                instance_id: instance_id.to_string(),
            },
        }
        .execute(&ctx);
    }

    let consumer_config = consumer_config.unwrap_or_else(|| {
        panic!(
            "consumer runtime config dump never appeared / parsed at {}",
            consumer_dump.display()
        )
    });
    assert_eq!(
        consumer_config.node_instance.slot_bindings.get(link_id),
        Some(&config::runtime::BoundProducers::from(
            config::runtime::ProducerRef::new(&core_node_name, producer_instance_id),
        )),
        "contract dep `{link_id}` should resolve to the implementing producer's instance \
         stamped with the daemon's core_node",
    );
}

/// Contract satisfaction is determined solely by `manifest.implements`,
/// never by node identity: a producer whose node name coincidentally
/// matches a consumer's contract dep name+tag but who declares no
/// `implements` must be rejected at binding validation.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stack_launch_rejects_binding_when_producer_omits_implements() {
    let serve = ServeCommandEmulation::with_mock()
        .await
        .expect("failed to create serve emulation");

    let nodes_dir = tempfile::tempdir().expect("failed to create temp nodes directory");

    // Contract name and producer node name intentionally collide on
    // `depth_camera:v1` to confirm the validator ignores node-identity
    // coincidence and requires an explicit `implements` declaration.
    let contract_name = "depth_camera";
    let contract_tag = "v1";
    let producer_name = "depth_camera"; // intentional coincidence
    let consumer_name = "video_reconstruction";
    let node_tag = "v1";
    let producer_instance_id = "depth_cam_inst1";
    let consumer_instance_id = "video_rec_1";
    let link_id = "rear_camera";
    let git_hash = read_daemon_git_hash(serve.daemon_state_path());

    let contract_doc_path = nodes_dir.path().join("interfaces/depth_camera.peppy.json5");
    write_contract_v1_doc(&contract_doc_path, contract_name, contract_tag);

    let run_cmd = vec!["sh".to_string(), "-c".to_string(), "sleep 30".to_string()];
    // Producer omits `manifest.implements` entirely.
    let producer_path = write_node_config_for_helper(
        nodes_dir.path(),
        producer_name,
        node_tag,
        &git_hash,
        &run_cmd,
        None,
        None,
        None,
    );

    let consumer_depends_on = format!(
        r#"{{
            nodes: [],
            contracts: [{{
                name: "{contract_name}",
                tag: "{contract_tag}",
                link_id: "{link_id}"
            }}]
        }}"#
    );
    let consumer_path = write_node_config_for_helper(
        nodes_dir.path(),
        consumer_name,
        node_tag,
        &git_hash,
        &run_cmd,
        Some(&consumer_depends_on),
        None,
        None,
    );

    let ctx = Arc::new(
        AppContext::with_messenger(nodes_dir.path(), Arc::clone(&serve.messenger()))
            .with_daemon_state_file(serve.daemon_state_path()),
    );

    let launcher_path = nodes_dir.path().join("peppy_launcher.json5");
    let launcher_json5 = format!(
        r#"{{
            peppy_schema: "launcher/v1",
            deployments: [
                {{
                    source: {{ name: "{producer_name}:{node_tag}" }},
                    instances: [{{ instance_id: "{producer_instance_id}" }}]
                }},
                {{
                    source: {{ name: "{consumer_name}:{node_tag}" }},
                    instances: [{{
                        instance_id: "{consumer_instance_id}",
                        links: {{ {link_id}: "{producer_instance_id}" }}
                    }}]
                }}
            ]
        }}"#
    );
    fs::write(&launcher_path, launcher_json5).expect("launcher config should be writable");
    // After any refresh in this test: refresh rewrites nodes.json5, so
    // registration must come later to survive.
    register_repo_caches(
        serve.temp_dir(),
        &[
            (producer_name, node_tag, &producer_path),
            (consumer_name, node_tag, &consumer_path),
        ],
    );

    let result = StackCommand {
        command: StackCommands::Launch {
            place: Vec::new(),
            local: false,
            launcher_config_path: launcher_path,
            node_add_idle_timeout_secs: 60,
            node_build_idle_timeout_secs: 60,
            node_run_idle_timeout_secs: 60,
            max_timeout_secs: Some(120),
        },
    }
    .execute(&ctx);

    let err_msg = result
        .expect_err("launch must fail when producer omits implements")
        .to_string();
    assert!(
        err_msg.contains("manifest.implements"),
        "error should mention manifest.implements. Got:\n{err_msg}"
    );
    assert!(
        err_msg.contains(link_id),
        "error should name the consumer's slot `{link_id}`. Got:\n{err_msg}"
    );
    assert!(
        err_msg.contains(producer_instance_id),
        "error should name the target producer instance. Got:\n{err_msg}"
    );
}

/// `implements` matching is strict on `(name, tag)`: a producer
/// implementing the right contract name but a different tag must be
/// rejected at binding validation.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stack_launch_rejects_binding_with_wrong_tag_in_implements() {
    let serve = ServeCommandEmulation::with_mock()
        .await
        .expect("failed to create serve emulation");

    let nodes_dir = tempfile::tempdir().expect("failed to create temp nodes directory");

    let contract_name = "depth_camera";
    let consumer_wants_tag = "v1";
    let producer_claims_tag = "v2"; // mismatch
    let producer_name = "realsense_d405";
    let consumer_name = "video_reconstruction";
    let node_tag = "v1";
    let producer_instance_id = "depth_cam_inst1";
    let consumer_instance_id = "video_rec_1";
    let link_id = "rear_camera";
    let git_hash = read_daemon_git_hash(serve.daemon_state_path());

    // Both contract tags exist on disk so the test isn't ambiguous
    // about whether the doc is missing vs. the wrong one. Only the v1
    // contract is what the consumer asks for.
    write_contract_v1_doc(
        &nodes_dir
            .path()
            .join("interfaces/depth_camera_v1.peppy.json5"),
        contract_name,
        consumer_wants_tag,
    );
    write_contract_v1_doc(
        &nodes_dir
            .path()
            .join("interfaces/depth_camera_v2.peppy.json5"),
        contract_name,
        producer_claims_tag,
    );

    let run_cmd = vec!["sh".to_string(), "-c".to_string(), "sleep 30".to_string()];
    let producer_implements =
        format!(r#"[{{ name: "{contract_name}", tag: "{producer_claims_tag}", link_id: "cam" }}]"#);
    let producer_interfaces = r#"{
            topics: { emits: [{ link_id: "cam", name: "video_stream" }] }
        }"#;
    let producer_path = write_node_config_for_helper(
        nodes_dir.path(),
        producer_name,
        node_tag,
        &git_hash,
        &run_cmd,
        None,
        Some(&producer_implements),
        Some(producer_interfaces),
    );

    let consumer_depends_on = format!(
        r#"{{
            nodes: [],
            contracts: [{{
                name: "{contract_name}",
                tag: "{consumer_wants_tag}",
                link_id: "{link_id}"
            }}]
        }}"#
    );
    let consumer_path = write_node_config_for_helper(
        nodes_dir.path(),
        consumer_name,
        node_tag,
        &git_hash,
        &run_cmd,
        Some(&consumer_depends_on),
        None,
        None,
    );

    let ctx = Arc::new(
        AppContext::with_messenger(nodes_dir.path(), Arc::clone(&serve.messenger()))
            .with_daemon_state_file(serve.daemon_state_path()),
    );

    let launcher_path = nodes_dir.path().join("peppy_launcher.json5");
    let launcher_json5 = format!(
        r#"{{
            peppy_schema: "launcher/v1",
            deployments: [
                {{
                    source: {{ name: "{producer_name}:{node_tag}" }},
                    instances: [{{ instance_id: "{producer_instance_id}" }}]
                }},
                {{
                    source: {{ name: "{consumer_name}:{node_tag}" }},
                    instances: [{{
                        instance_id: "{consumer_instance_id}",
                        links: {{ {link_id}: "{producer_instance_id}" }}
                    }}]
                }}
            ]
        }}"#
    );
    fs::write(&launcher_path, launcher_json5).expect("launcher config should be writable");
    // After any refresh in this test: refresh rewrites nodes.json5, so
    // registration must come later to survive.
    register_repo_caches(
        serve.temp_dir(),
        &[
            (producer_name, node_tag, &producer_path),
            (consumer_name, node_tag, &consumer_path),
        ],
    );

    let result = StackCommand {
        command: StackCommands::Launch {
            place: Vec::new(),
            local: false,
            launcher_config_path: launcher_path,
            node_add_idle_timeout_secs: 60,
            node_build_idle_timeout_secs: 60,
            node_run_idle_timeout_secs: 60,
            max_timeout_secs: Some(120),
        },
    }
    .execute(&ctx);

    let err_msg = result
        .expect_err("launch must fail when producer's implements tag mismatches")
        .to_string();
    assert!(
        err_msg.contains("manifest.implements"),
        "error should mention manifest.implements. Got:\n{err_msg}"
    );
    assert!(
        err_msg.contains(&format!("{contract_name}:{consumer_wants_tag}")),
        "error should name the requested contract `{contract_name}:{consumer_wants_tag}`. Got:\n{err_msg}"
    );
}

/// Bidirectional contract communication under explicit links: two
/// nodes each emit one contract (`manifest.implements`) and consume the other
/// through a contract dep, each slot bound to the other instance in the
/// launcher (implements-matched, not node-identity-matched). The
/// launcher must materialize both slots with the producer's full wire
/// address, and the stack must come up regardless of deployment order.
/// This is the end-to-end counterpart to the unit-level binding
/// validator tests and the wire-level flow test in
/// `peppylib/tests/topics.rs`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stack_launch_binds_contract_slots_in_both_directions() {
    // Instances must stay in the stack until the assertions below have read
    // them; a fixed `sleep` would make that a race against machine load.
    let instances = peppy::test_support::InstanceLifetime::new();
    let serve = ServeCommandEmulation::with_zenoh()
        .await
        .expect("failed to create zenoh serve emulation");
    let core_node_name = serve.core_node_name().to_string();

    let nodes_dir = tempfile::tempdir().expect("failed to create temp nodes directory");
    let dump_dir = tempfile::tempdir().expect("failed to create temp dump directory");
    let contract_repo_dir = tempfile::tempdir().expect("failed to create temp contract repo");
    let controller_dump = dump_dir.path().join("arm_controller.json5");
    let arm_dump = dump_dir.path().join("robot_arm.json5");

    let state_contract = "joint_state_source";
    let command_contract = "joint_command_source";
    let contract_tag = "v1";
    let controller_name = "arm_controller";
    let arm_name = "robot_arm";
    let node_tag = "v1";
    let controller_instance_id = "ctrl_1";
    let arm_instance_id = "arm_1";
    // The consumed-contract slot link_id on each side (the direction it reads).
    let controller_link_id = "arm"; // arm_controller consumes joint_states from the arm
    let arm_link_id = "controller"; // robot_arm consumes joint_commands from the controller
    let git_hash = read_daemon_git_hash(serve.daemon_state_path());

    // Two contract documents, one per direction, both registered in the
    // same fs repo so the consumer node-add can resolve each `(name, tag)`
    // from cache even though nothing is bound.
    write_contract_v1_doc_with_topic(
        &contract_repo_dir
            .path()
            .join("joint_state_source/peppy.json5"),
        state_contract,
        contract_tag,
        "joint_states",
        r#"{
            positions: { $type: "array", $items: "f64", $length: 3 },
            velocities: { $type: "array", $items: "f64", $length: 3 },
            timestamp: "time"
        }"#,
    );
    write_contract_v1_doc_with_topic(
        &contract_repo_dir
            .path()
            .join("joint_command_source/peppy.json5"),
        command_contract,
        contract_tag,
        "joint_commands",
        r#"{
            target_positions: { $type: "array", $items: "f64", $length: 3 },
            max_velocity: "f64"
        }"#,
    );
    super::common::publish_repo_index(contract_repo_dir.path());
    let conf_dir = serve.temp_dir().join("conf");
    fs::create_dir_all(&conf_dir).expect("create conf dir");
    let repos_content = serde_json::to_string_pretty(&serde_json::json!([
        { "id": 1, "type": "fs", "path": contract_repo_dir.path().to_string_lossy() }
    ]))
    .expect("serialize repos");
    fs::write(conf_dir.join("repositories.json5"), repos_content).expect("write repos");

    // arm_controller: emits joint_commands (implements joint_command_source),
    // consumes joint_states through its `arm` slot (bound in the launcher).
    let controller_run_cmd = vec![
        "sh".to_string(),
        "-c".to_string(),
        format!(
            "cp \"$PEPPY_RUNTIME_CONFIG\" \"{}\" && {}",
            controller_dump.display(),
            instances.keep_alive_script(),
        ),
    ];
    let controller_depends_on = format!(
        r#"{{
            nodes: [],
            contracts: [{{
                name: "{state_contract}",
                tag: "{contract_tag}",
                link_id: "{controller_link_id}"
            }}]
        }}"#
    );
    let controller_implements =
        format!(r#"[{{ name: "{command_contract}", tag: "{contract_tag}", link_id: "cmd_out" }}]"#);
    let controller_interfaces = r#"{
            topics: { emits: [{ link_id: "cmd_out", name: "joint_commands" }] }
        }"#;
    let controller_path = write_node_config_for_helper(
        nodes_dir.path(),
        controller_name,
        node_tag,
        &git_hash,
        &controller_run_cmd,
        Some(&controller_depends_on),
        Some(&controller_implements),
        Some(controller_interfaces),
    );

    // robot_arm: emits joint_states (implements joint_state_source),
    // consumes joint_commands through its `controller` slot, bound back
    // to the controller in the launcher (every declared slot must be
    // bound).
    let arm_run_cmd = vec![
        "sh".to_string(),
        "-c".to_string(),
        format!(
            "cp \"$PEPPY_RUNTIME_CONFIG\" \"{}\" && {}",
            arm_dump.display(),
            instances.keep_alive_script(),
        ),
    ];
    let arm_depends_on = format!(
        r#"{{
            nodes: [],
            contracts: [{{
                name: "{command_contract}",
                tag: "{contract_tag}",
                link_id: "{arm_link_id}"
            }}]
        }}"#
    );
    let arm_implements =
        format!(r#"[{{ name: "{state_contract}", tag: "{contract_tag}", link_id: "state_out" }}]"#);
    let arm_interfaces = r#"{
            topics: { emits: [{ link_id: "state_out", name: "joint_states" }] }
        }"#;
    let arm_path = write_node_config_for_helper(
        nodes_dir.path(),
        arm_name,
        node_tag,
        &git_hash,
        &arm_run_cmd,
        Some(&arm_depends_on),
        Some(&arm_implements),
        Some(arm_interfaces),
    );

    let ctx = Arc::new(
        AppContext::with_messenger(nodes_dir.path(), Arc::clone(&serve.messenger()))
            .with_daemon_state_file(serve.daemon_state_path()),
    );

    RepoCommand {
        command: RepoCommands::Refresh,
    }
    .execute(&ctx)
    .expect("repo refresh should populate contract cache");

    // The dummy `sh` subprocesses don't expose ready/health/shutdown, so
    // impersonate them from the test process for both instances (the
    // daemon's launch waits for each instance to report ready).
    let node_messenger = MessengerHandle::from_shared(Arc::clone(&serve.messenger()));
    let _ready_controller = listen_for_node_ready(
        &node_messenger,
        &core_node_name,
        controller_instance_id,
        test_node_target(controller_name),
    )
    .await
    .expect("controller ready service should start");
    let _health_controller = listen_for_node_health(
        &node_messenger,
        &core_node_name,
        controller_instance_id,
        test_node_target(controller_name),
    )
    .await
    .expect("controller health service should start");
    let (_shutdown_controller, _) = listen_for_shutdown(
        &node_messenger,
        &core_node_name,
        controller_instance_id,
        test_node_target(controller_name),
    )
    .await
    .expect("controller shutdown service should start");
    let _ready_arm = listen_for_node_ready(
        &node_messenger,
        &core_node_name,
        arm_instance_id,
        test_node_target(arm_name),
    )
    .await
    .expect("arm ready service should start");
    let _health_arm = listen_for_node_health(
        &node_messenger,
        &core_node_name,
        arm_instance_id,
        test_node_target(arm_name),
    )
    .await
    .expect("arm health service should start");
    let (_shutdown_arm, _) = listen_for_shutdown(
        &node_messenger,
        &core_node_name,
        arm_instance_id,
        test_node_target(arm_name),
    )
    .await
    .expect("arm shutdown service should start");

    // Bind both directions: the controller's slot to the arm and the
    // arm's slot to the controller — an unbound slot would be rejected
    // at the validation step.
    let launcher_path = nodes_dir.path().join("peppy_launcher.json5");
    let launcher_json5 = format!(
        r#"{{
            peppy_schema: "launcher/v1",
            deployments: [
                {{
                    source: {{ name: "{controller_name}:{node_tag}" }},
                    instances: [{{
                        instance_id: "{controller_instance_id}",
                        links: {{ {controller_link_id}: "{arm_instance_id}" }}
                    }}]
                }},
                {{
                    source: {{ name: "{arm_name}:{node_tag}" }},
                    instances: [{{
                        instance_id: "{arm_instance_id}",
                        links: {{ {arm_link_id}: "{controller_instance_id}" }}
                    }}]
                }}
            ]
        }}"#
    );
    fs::write(&launcher_path, launcher_json5).expect("launcher config should be writable");
    // After the refresh above: refresh rewrites nodes.json5, so these
    // registrations must come later to survive.
    register_repo_caches(
        serve.temp_dir(),
        &[
            (controller_name, node_tag, &controller_path),
            (arm_name, node_tag, &arm_path),
        ],
    );

    StackCommand {
        command: StackCommands::Launch {
            place: Vec::new(),
            local: false,
            launcher_config_path: launcher_path,
            node_add_idle_timeout_secs: 60,
            node_build_idle_timeout_secs: 60,
            node_run_idle_timeout_secs: 60,
            max_timeout_secs: Some(120),
        },
    }
    .execute(&ctx)
    .expect("launch should succeed with both contract slots bound");

    // Both `sh` wrappers snapshot their runtime config before sleeping.
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut controller_config: Option<config::runtime::RuntimeConfig> = None;
    let mut arm_config: Option<config::runtime::RuntimeConfig> = None;
    while Instant::now() < deadline {
        if controller_config.is_none()
            && let Ok(content) = fs::read_to_string(&controller_dump)
            && let Ok(cfg) = serde_json5::from_str::<config::runtime::RuntimeConfig>(&content)
        {
            controller_config = Some(cfg);
        }
        if arm_config.is_none()
            && let Ok(content) = fs::read_to_string(&arm_dump)
            && let Ok(cfg) = serde_json5::from_str::<config::runtime::RuntimeConfig>(&content)
        {
            arm_config = Some(cfg);
        }
        if controller_config.is_some() && arm_config.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // The dumps are captured, so nothing below needs these instances. Ending
    // the keep-alive first lets each Stop observe an already-exited process
    // instead of waiting out the daemon's force-kill grace.
    drop(instances);
    for instance_id in [controller_instance_id, arm_instance_id] {
        let _ = NodeCommand {
            command: NodeCommands::Stop {
                instance_id: instance_id.to_string(),
            },
        }
        .execute(&ctx);
    }

    let controller_config = controller_config.unwrap_or_else(|| {
        panic!(
            "arm_controller runtime config dump never appeared / parsed at {}",
            controller_dump.display()
        )
    });
    assert_eq!(
        controller_config
            .node_instance
            .slot_bindings
            .get(controller_link_id),
        Some(&config::runtime::BoundProducers::from(
            config::runtime::ProducerRef::new(&core_node_name, arm_instance_id),
        )),
        "arm_controller's `{controller_link_id}` interface slot should materialize with \
         the bound producer's full wire address",
    );

    let arm_config = arm_config.unwrap_or_else(|| {
        panic!(
            "robot_arm runtime config dump never appeared / parsed at {}",
            arm_dump.display()
        )
    });
    assert_eq!(
        arm_config.node_instance.slot_bindings.get(arm_link_id),
        Some(&config::runtime::BoundProducers::from(
            config::runtime::ProducerRef::new(&core_node_name, controller_instance_id),
        )),
        "robot_arm's `{arm_link_id}` interface slot should materialize with the bound \
         producer's full wire address",
    );
}

/// A `zero_or_one` producer slot through both of its states in one launch: two
/// instances of one consumer whose `depends_on.contracts` slot is declared
/// `cardinality: "zero_or_one"`, one linked to the implementing producer and
/// one declared `{ vacant: "<why>" }`. Both reach Running, and each instance's
/// own boot config says which state it is in: the bound instance carries the
/// one-member set, the vacant one an explicit empty set. The vacancy reason is
/// a launcher artifact and rides nowhere near the goal, which is exactly why
/// the empty set has to be in `slot_bindings` rather than absent from it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stack_launch_binds_one_zero_or_one_instance_and_vacates_another() {
    // Instances must stay in the stack until the assertions below have read
    // their dumps; a fixed `sleep` would make that a race against machine load.
    let instances = peppy::test_support::InstanceLifetime::new();
    let serve = ServeCommandEmulation::with_zenoh()
        .await
        .expect("failed to create zenoh serve emulation");
    let core_node_name = serve.core_node_name().to_string();

    let nodes_dir = tempfile::tempdir().expect("failed to create temp nodes directory");
    let ctx = Arc::new(
        AppContext::with_messenger(nodes_dir.path(), Arc::clone(&serve.messenger()))
            .with_daemon_state_file(serve.daemon_state_path()),
    );
    let dump_dir = tempfile::tempdir().expect("failed to create temp dump directory");
    let contract_repo_dir = tempfile::tempdir().expect("failed to create temp contract repo");
    let bound_dump = dump_dir.path().join("bound.json5");
    let vacant_dump = dump_dir.path().join("vacant.json5");

    let contract_name = "depth_camera";
    let contract_tag = "v1";
    let producer_name = "depth_camera_driver";
    let consumer_name = "wrist_consumer";
    let node_tag = "v1";
    let producer_instance_id = "wrist_cam_1";
    let bound_instance_id = "cons_with_camera";
    let vacant_instance_id = "cons_without_camera";
    let link_id = "wrist_camera";
    let git_hash = read_daemon_git_hash(serve.daemon_state_path());

    write_contract_v1_doc(
        &contract_repo_dir.path().join("depth_camera/peppy.json5"),
        contract_name,
        contract_tag,
    );
    super::common::seed_docs_repo(&serve, &ctx, contract_repo_dir.path());

    let producer_run_cmd = vec![
        "sh".to_string(),
        "-c".to_string(),
        instances.keep_alive_script(),
    ];
    let producer_implements =
        format!(r#"[{{ name: "{contract_name}", tag: "{contract_tag}", link_id: "cam_out" }}]"#);
    let producer_interfaces = r#"{
            topics: { emits: [{ link_id: "cam_out", name: "video_stream" }] }
        }"#;
    let producer_path = write_node_config_for_helper(
        nodes_dir.path(),
        producer_name,
        node_tag,
        &git_hash,
        &producer_run_cmd,
        None,
        Some(&producer_implements),
        Some(producer_interfaces),
    );

    // One consumer node, spawned twice. Each instance dumps the boot config it
    // was handed to the path its own `env_vars` names, so the two runtime views
    // are read from the instances themselves rather than from the planner.
    let consumer_run_cmd = vec![
        "sh".to_string(),
        "-c".to_string(),
        format!(
            "cp \"$PEPPY_RUNTIME_CONFIG\" \"$NODE_DUMP_PATH\" && {}",
            instances.keep_alive_script(),
        ),
    ];
    let consumer_depends_on = format!(
        r#"{{
            contracts: [{{
                name: "{contract_name}",
                tag: "{contract_tag}",
                link_id: "{link_id}",
                cardinality: "zero_or_one"
            }}]
        }}"#
    );
    let consumer_path = write_node_config_for_helper(
        nodes_dir.path(),
        consumer_name,
        node_tag,
        &git_hash,
        &consumer_run_cmd,
        Some(&consumer_depends_on),
        None,
        None,
    );

    // The dummy `sh` subprocesses expose none of the framework services, so
    // impersonate them for all three instances.
    let node_messenger = MessengerHandle::from_shared(Arc::clone(&serve.messenger()));
    let mut service_guards = Vec::new();
    for (node_name, instance_id) in [
        (producer_name, producer_instance_id),
        (consumer_name, bound_instance_id),
        (consumer_name, vacant_instance_id),
    ] {
        let ready = listen_for_node_ready(
            &node_messenger,
            &core_node_name,
            instance_id,
            test_node_target(node_name),
        )
        .await
        .expect("ready service should start");
        let health = listen_for_node_health(
            &node_messenger,
            &core_node_name,
            instance_id,
            test_node_target(node_name),
        )
        .await
        .expect("health service should start");
        let (shutdown, _) = listen_for_shutdown(
            &node_messenger,
            &core_node_name,
            instance_id,
            test_node_target(node_name),
        )
        .await
        .expect("shutdown service should start");
        service_guards.push((ready, health, shutdown));
    }

    let launcher_path = nodes_dir.path().join("peppy_launcher.json5");
    let launcher_json5 = format!(
        r#"{{
            peppy_schema: "launcher/v1",
            deployments: [
                {{
                    source: {{ name: "{producer_name}:{node_tag}" }},
                    instances: [{{ instance_id: "{producer_instance_id}" }}]
                }},
                {{
                    source: {{ name: "{consumer_name}:{node_tag}" }},
                    instances: [
                        {{
                            instance_id: "{bound_instance_id}",
                            env_vars: {{ NODE_DUMP_PATH: "{bound_dump_path}" }},
                            links: {{ {link_id}: "{producer_instance_id}" }}
                        }},
                        {{
                            instance_id: "{vacant_instance_id}",
                            env_vars: {{ NODE_DUMP_PATH: "{vacant_dump_path}" }},
                            links: {{ {link_id}: {{ vacant: "this rig ships without a wrist camera" }} }}
                        }}
                    ]
                }}
            ]
        }}"#,
        bound_dump_path = bound_dump.display(),
        vacant_dump_path = vacant_dump.display(),
    );
    fs::write(&launcher_path, launcher_json5).expect("launcher config should be writable");
    // After the refresh above: refresh rewrites nodes.json5, so these
    // registrations must come later to survive.
    register_repo_caches(
        serve.temp_dir(),
        &[
            (producer_name, node_tag, &producer_path),
            (consumer_name, node_tag, &consumer_path),
        ],
    );

    StackCommand {
        command: StackCommands::Launch {
            place: Vec::new(),
            local: false,
            launcher_config_path: launcher_path,
            node_add_idle_timeout_secs: 60,
            node_build_idle_timeout_secs: 60,
            node_run_idle_timeout_secs: 60,
            max_timeout_secs: Some(120),
        },
    }
    .execute(&ctx)
    .expect("a launch mixing a bound and a vacant zero_or_one slot should succeed");

    let deadline = Instant::now() + Duration::from_secs(5);
    let mut bound_config: Option<config::runtime::RuntimeConfig> = None;
    let mut vacant_config: Option<config::runtime::RuntimeConfig> = None;
    while Instant::now() < deadline {
        for (dump, slot) in [
            (&bound_dump, &mut bound_config),
            (&vacant_dump, &mut vacant_config),
        ] {
            if slot.is_none()
                && let Ok(content) = fs::read_to_string(dump)
                && let Ok(cfg) = serde_json5::from_str::<config::runtime::RuntimeConfig>(&content)
            {
                *slot = Some(cfg);
            }
        }
        if bound_config.is_some() && vacant_config.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    drop(instances);
    for instance_id in [bound_instance_id, vacant_instance_id, producer_instance_id] {
        let _ = NodeCommand {
            command: NodeCommands::Stop {
                instance_id: instance_id.to_string(),
            },
        }
        .execute(&ctx);
    }

    let bound_config = bound_config.unwrap_or_else(|| {
        panic!(
            "bound instance runtime config dump never appeared / parsed at {}",
            bound_dump.display()
        )
    });
    assert_eq!(
        bound_config.node_instance.slot_bindings.get(link_id),
        Some(&config::runtime::BoundProducers::from(
            config::runtime::ProducerRef::new(&core_node_name, producer_instance_id),
        )),
        "the linked instance's `{link_id}` slot must carry the implementing producer's full \
         wire address",
    );

    let vacant_config = vacant_config.unwrap_or_else(|| {
        panic!(
            "vacant instance runtime config dump never appeared / parsed at {}",
            vacant_dump.display()
        )
    });
    assert_eq!(
        vacant_config.node_instance.slot_bindings.get(link_id),
        Some(&config::runtime::BoundProducers::default()),
        "the vacant instance's `{link_id}` slot must carry an EXPLICIT empty set: an absent \
         entry is what node startup rejects, and it is what would make a forgotten slot \
         indistinguishable from an emptied one",
    );
}

/// Every declared `depends_on` slot must be bound: a launcher that omits a
/// binding entry for a declared slot is rejected at the validation step —
/// before anything is added or spawned — with an error naming the instance
/// and the unfulfilled slot.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stack_launch_rejects_unbound_slot() {
    let serve = ServeCommandEmulation::with_mock()
        .await
        .expect("failed to create serve emulation");
    let shared_messenger = serve.messenger();
    let core_node_name = serve.core_node_name().to_string();

    let nodes_dir = tempfile::tempdir().expect("failed to create temp nodes directory");
    let producer_name = "unbound_camera";
    let consumer_name = "unbound_consumer";
    let node_tag = "v1";
    let link_id = "main_cam";
    let git_hash = read_daemon_git_hash(serve.daemon_state_path());

    let producer_path = write_node_config(
        nodes_dir.path(),
        producer_name,
        node_tag,
        &git_hash,
        &["sh", "-c", "sleep 30"],
    );
    let consumer_run_cmd = vec!["sh".to_string(), "-c".to_string(), "sleep 30".to_string()];
    let consumer_depends_on = format!(
        r#"{{
            nodes: [{{ name: "{producer_name}", tag: "{node_tag}", link_id: "{link_id}" }}]
        }}"#
    );
    let consumer_path = write_node_config_for_helper(
        nodes_dir.path(),
        consumer_name,
        node_tag,
        &git_hash,
        &consumer_run_cmd,
        Some(&consumer_depends_on),
        None,
        None,
    );

    let ctx = Arc::new(
        AppContext::with_messenger(nodes_dir.path(), Arc::clone(&shared_messenger))
            .with_daemon_state_file(serve.daemon_state_path()),
    );

    // The consumer instance carries no `links:` map at all, so its
    // declared `main_cam` slot is unfulfilled.
    let launcher_path = nodes_dir.path().join("peppy_launcher.json5");
    let launcher_json5 = format!(
        r#"{{
            peppy_schema: "launcher/v1",
            deployments: [
                {{
                    source: {{ name: "{producer_name}:{node_tag}" }},
                    instances: [{{ instance_id: "cam_1" }}]
                }},
                {{
                    source: {{ name: "{consumer_name}:{node_tag}" }},
                    instances: [{{ instance_id: "cons_1" }}]
                }}
            ]
        }}"#
    );
    fs::write(&launcher_path, launcher_json5).expect("launcher config should be writable");
    register_repo_caches(
        serve.temp_dir(),
        &[
            (producer_name, node_tag, &producer_path),
            (consumer_name, node_tag, &consumer_path),
        ],
    );

    let result = StackCommand {
        command: StackCommands::Launch {
            place: Vec::new(),
            local: false,
            launcher_config_path: launcher_path,
            node_add_idle_timeout_secs: 60,
            node_build_idle_timeout_secs: 60,
            node_run_idle_timeout_secs: 60,
            max_timeout_secs: Some(3600),
        },
    }
    .execute(&ctx);

    let err_msg = result
        .expect_err("launch must fail on an unfulfilled declared slot")
        .to_string();
    assert!(
        err_msg.contains("cons_1")
            && err_msg.contains("leaves slot `main_cam`")
            && err_msg.contains("unfulfilled"),
        "error should name the owning instance and the unfulfilled slot. Got:\n{err_msg}"
    );
    assert!(
        err_msg.contains("--link main_cam@"),
        "error should show the exact bind syntax that fixes it. Got:\n{err_msg}"
    );

    // No spawn side-effect: neither node should appear in the stack.
    let messenger_handle = ctx
        .messenger_handle()
        .expect("messenger handle should be available");
    let response = poll(
        &StackListRequest::new(),
        messenger_handle,
        &core_node_name,
        CALLER_INSTANCE_ID,
        &core_node_name,
        Duration::from_secs(5),
    )
    .await
    .expect("stack_list request should complete");
    let graph: SerializedNodeGraph =
        serde_json::from_str(&response.graph_json).expect("graph_json should parse");
    assert!(
        !graph
            .nodes
            .iter()
            .any(|n| n.name == producer_name || n.name == consumer_name),
        "rejected launcher must not have added or spawned anything. Graph: {:?}",
        graph.nodes.iter().map(|n| n.label()).collect::<Vec<_>>()
    );
}

/// Launcher `pairings:` establish pairs at launch: the earlier-started
/// endpoint boots with its slot covered, the later one carries the request,
/// and both
/// running instances receive their `peer_update` pins live. The dummy `sh`
/// nodes expose ready/health/shutdown/peer_update from the test process.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stack_launch_establishes_launcher_pairings() {
    let serve = ServeCommandEmulation::with_zenoh()
        .await
        .expect("failed to create zenoh serve emulation");
    let core_node_name = serve.core_node_name().to_string();

    let nodes_dir = tempfile::tempdir().expect("failed to create temp nodes directory");
    let ctx = Arc::new(
        AppContext::with_messenger(nodes_dir.path(), Arc::clone(&serve.messenger()))
            .with_daemon_state_file(serve.daemon_state_path()),
    );

    // The arm_link pairing doc must be in the daemon's pairing cache for
    // the launch-time `node add` codegen to resolve `depends_on.pairings`.
    let repo_dir = tempfile::tempdir().expect("temp repo dir");
    super::common::seed_pairing_repo(&serve, &ctx, repo_dir.path());

    let git_hash = read_daemon_git_hash(serve.daemon_state_path());
    // Keep-alive rather than `sleep 30`: both instances must be live for the
    // pin assertions, and the teardown below ends them explicitly instead of
    // leaving the daemon to wait out its force-kill deadline once per instance.
    let instances = peppy::test_support::InstanceLifetime::new();
    let run_cmd = instances.keep_alive_argv();
    let arm_path = write_node_config_for_helper(
        nodes_dir.path(),
        "robot_arm",
        "v1",
        &git_hash,
        &run_cmd,
        Some(
            r#"{ pairings: [{ name: "arm_link", tag: "v1", role: "arm", link_id: "controller" }] }"#,
        ),
        None,
        Some(
            r#"{ topics: {
                emits: [{ link_id: "controller", name: "joint_states" }],
                consumes: [{ link_id: "controller", name: "joint_commands" }]
            } }"#,
        ),
    );
    let ctrl_path = write_node_config_for_helper(
        nodes_dir.path(),
        "arm_controller",
        "v1",
        &git_hash,
        &run_cmd,
        Some(
            r#"{ pairings: [{ name: "arm_link", tag: "v1", role: "controller", link_id: "arm" }] }"#,
        ),
        None,
        Some(
            r#"{ topics: {
                emits: [{ link_id: "arm", name: "joint_commands" }],
                consumes: [{ link_id: "arm", name: "joint_states" }]
            } }"#,
        ),
    );

    // In-process node services for both instances, including the
    // `peer_update` endpoints whose watches observe the delivered pins.
    let node_messenger = MessengerHandle::from_shared(Arc::clone(&serve.messenger()));
    let mut watches = Vec::new();
    for (node_name, instance_id, link_id) in [
        ("robot_arm", "arm_1", "controller"),
        ("arm_controller", "ctrl_1", "arm"),
    ] {
        let _ready = listen_for_node_ready(
            &node_messenger,
            &core_node_name,
            instance_id,
            test_node_target(node_name),
        )
        .await
        .expect("ready service should start");
        let _health = listen_for_node_health(
            &node_messenger,
            &core_node_name,
            instance_id,
            test_node_target(node_name),
        )
        .await
        .expect("health service should start");
        let (_shutdown, _) = listen_for_shutdown(
            &node_messenger,
            &core_node_name,
            instance_id,
            test_node_target(node_name),
        )
        .await
        .expect("shutdown service should start");
        let (tx, rx) = tokio::sync::watch::channel(peppylib::messaging::PeerPinState::unpaired());
        let slots = Arc::new(std::collections::BTreeMap::from([(
            link_id.to_string(),
            tx,
        )]));
        peppylib::services::peer_update::listen_for_peer_update(
            &node_messenger,
            &core_node_name,
            instance_id,
            test_node_target(node_name),
            slots,
        )
        .await
        .expect("peer_update service should start");
        watches.push(rx);
    }

    // The pair is declared once, on the controller instance; the launcher
    // works out the establishment order.
    let launcher_path = nodes_dir.path().join("peppy_launcher.json5");
    let launcher_json5 = r#"{
            peppy_schema: "launcher/v1",
            deployments: [
                {
                    source: { name: "robot_arm:v1" },
                    instances: [{ instance_id: "arm_1" }]
                },
                {
                    source: { name: "arm_controller:v1" },
                    instances: [{
                        instance_id: "ctrl_1",
                        links: { arm: "arm_1" }
                    }]
                }
            ]
        }"#;
    fs::write(&launcher_path, launcher_json5).expect("launcher config should be writable");
    // After `seed_pairing_repo`'s refresh: refresh rewrites the node and
    // launcher caches, so these registrations must come later to survive.
    register_repo_caches(
        serve.temp_dir(),
        &[
            ("robot_arm", "v1", &arm_path),
            ("arm_controller", "v1", &ctrl_path),
        ],
    );

    StackCommand {
        command: StackCommands::Launch {
            place: Vec::new(),
            local: false,
            launcher_config_path: launcher_path,
            node_add_idle_timeout_secs: 60,
            node_build_idle_timeout_secs: 60,
            node_run_idle_timeout_secs: 60,
            max_timeout_secs: Some(120),
        },
    }
    .execute(&ctx)
    .expect("launch with pairings should succeed");

    // Both endpoints are pinned to each other by the time launch returns.
    let arm_pin = watches[0].borrow().clone();
    let pin = arm_pin.pin.expect("arm_1's slot should be pinned");
    assert_eq!(pin.producer.instance_id, "ctrl_1");
    assert_eq!(pin.peer_link_id, "arm");
    let ctrl_pin = watches[1].borrow().clone();
    let pin = ctrl_pin.pin.expect("ctrl_1's slot should be pinned");
    assert_eq!(pin.producer.instance_id, "arm_1");
    assert_eq!(pin.peer_link_id, "controller");

    // Assertions are done; ending the keep-alive first lets each Stop observe
    // an already-exited process instead of waiting out the force-kill deadline.
    drop(instances);
    for instance_id in ["ctrl_1", "arm_1"] {
        let _ = NodeCommand {
            command: NodeCommands::Stop {
                instance_id: instance_id.to_string(),
            },
        }
        .execute(&ctx);
    }
}

/// Vacancy is per instance, which is the whole reason it lives in the
/// launcher rather than the manifest: one node with one `optional: true` slot,
/// two instances of it in one deployment, one paired and one vacant. Both boot,
/// and only the paired one ends up with a pin, so a slot's manifest optionality
/// is a permission rather than a fate the node carries into every deployment.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stack_launch_pairs_one_instance_and_vacates_another_of_the_same_node() {
    let serve = ServeCommandEmulation::with_zenoh()
        .await
        .expect("failed to create zenoh serve emulation");
    let core_node_name = serve.core_node_name().to_string();

    let nodes_dir = tempfile::tempdir().expect("failed to create temp nodes directory");
    let ctx = Arc::new(
        AppContext::with_messenger(nodes_dir.path(), Arc::clone(&serve.messenger()))
            .with_daemon_state_file(serve.daemon_state_path()),
    );

    let repo_dir = tempfile::tempdir().expect("temp repo dir");
    super::common::seed_pairing_repo(&serve, &ctx, repo_dir.path());

    let git_hash = read_daemon_git_hash(serve.daemon_state_path());
    let instances = peppy::test_support::InstanceLifetime::new();
    let run_cmd = instances.keep_alive_argv();
    // One arm manifest, whose `controller` slot the node itself declares
    // optional: some rigs drive the arm, some only watch it.
    let arm_path = write_node_config_for_helper(
        nodes_dir.path(),
        "robot_arm",
        "v1",
        &git_hash,
        &run_cmd,
        Some(
            r#"{ pairings: [{ name: "arm_link", tag: "v1", role: "arm", link_id: "controller", optional: true }] }"#,
        ),
        None,
        Some(
            r#"{ topics: {
                emits: [{ link_id: "controller", name: "joint_states" }],
                consumes: [{ link_id: "controller", name: "joint_commands" }]
            } }"#,
        ),
    );
    let ctrl_path = write_node_config_for_helper(
        nodes_dir.path(),
        "arm_controller",
        "v1",
        &git_hash,
        &run_cmd,
        Some(
            r#"{ pairings: [{ name: "arm_link", tag: "v1", role: "controller", link_id: "arm" }] }"#,
        ),
        None,
        Some(
            r#"{ topics: {
                emits: [{ link_id: "arm", name: "joint_commands" }],
                consumes: [{ link_id: "arm", name: "joint_states" }]
            } }"#,
        ),
    );

    let node_messenger = MessengerHandle::from_shared(Arc::clone(&serve.messenger()));
    let mut watches = Vec::new();
    for (node_name, instance_id, link_id) in [
        ("robot_arm", "arm_governed", "controller"),
        ("robot_arm", "arm_watched", "controller"),
        ("arm_controller", "ctrl_1", "arm"),
    ] {
        let _ready = listen_for_node_ready(
            &node_messenger,
            &core_node_name,
            instance_id,
            test_node_target(node_name),
        )
        .await
        .expect("ready service should start");
        let _health = listen_for_node_health(
            &node_messenger,
            &core_node_name,
            instance_id,
            test_node_target(node_name),
        )
        .await
        .expect("health service should start");
        let (_shutdown, _) = listen_for_shutdown(
            &node_messenger,
            &core_node_name,
            instance_id,
            test_node_target(node_name),
        )
        .await
        .expect("shutdown service should start");
        let (tx, rx) = tokio::sync::watch::channel(peppylib::messaging::PeerPinState::unpaired());
        let slots = Arc::new(std::collections::BTreeMap::from([(
            link_id.to_string(),
            tx,
        )]));
        peppylib::services::peer_update::listen_for_peer_update(
            &node_messenger,
            &core_node_name,
            instance_id,
            test_node_target(node_name),
            slots,
        )
        .await
        .expect("peer_update service should start");
        watches.push(rx);
    }

    // Two instances of one node choosing different fates for the same slot:
    // `arm_governed` is claimed reciprocally by the controller's link, and
    // `arm_watched` says in its own words that nothing drives it.
    let launcher_path = nodes_dir.path().join("peppy_launcher.json5");
    let launcher_json5 = r#"{
            peppy_schema: "launcher/v1",
            deployments: [
                {
                    source: { name: "robot_arm:v1" },
                    instances: [
                        { instance_id: "arm_governed" },
                        {
                            instance_id: "arm_watched",
                            links: { controller: { vacant: "monitor rig: nothing commands this arm" } }
                        }
                    ]
                },
                {
                    source: { name: "arm_controller:v1" },
                    instances: [{
                        instance_id: "ctrl_1",
                        links: { arm: "arm_governed" }
                    }]
                }
            ]
        }"#;
    fs::write(&launcher_path, launcher_json5).expect("launcher config should be writable");
    register_repo_caches(
        serve.temp_dir(),
        &[
            ("robot_arm", "v1", &arm_path),
            ("arm_controller", "v1", &ctrl_path),
        ],
    );

    StackCommand {
        command: StackCommands::Launch {
            place: Vec::new(),
            local: false,
            launcher_config_path: launcher_path,
            node_add_idle_timeout_secs: 60,
            node_build_idle_timeout_secs: 60,
            node_run_idle_timeout_secs: 60,
            max_timeout_secs: Some(120),
        },
    }
    .execute(&ctx)
    .expect("one instance paired and one vacant is a valid launch");

    let governed = watches[0].borrow().clone();
    let pin = governed
        .pin
        .expect("the governed instance's slot should be pinned");
    assert_eq!(pin.producer.instance_id, "ctrl_1");
    assert_eq!(pin.peer_link_id, "arm");

    let watched = watches[1].borrow().clone();
    assert!(
        watched.pin.is_none(),
        "the vacant instance's slot must stay unpaired: {:?}",
        watched.pin
    );

    let controller = watches[2].borrow().clone();
    let pin = controller
        .pin
        .expect("the controller's slot should be pinned");
    assert_eq!(
        pin.producer.instance_id, "arm_governed",
        "the controller pairs the instance it named, not its vacant sibling"
    );

    drop(instances);
    for instance_id in ["ctrl_1", "arm_watched", "arm_governed"] {
        let _ = NodeCommand {
            command: NodeCommands::Stop {
                instance_id: instance_id.to_string(),
            },
        }
        .execute(&ctx);
    }
}

/// An observer slot is delivered its member set live, at every cardinality. A
/// recorder observing the `arm` role of `arm_link/v1` declares three slots: a
/// `one` slot linked to `arm_1`, a `one_or_more` slot linked to
/// `["arm_2", "arm_1"]`, and a `zero_or_more` slot the launcher omits entirely.
/// The sources are `robot_arm` instances whose own participant slots are
/// declared vacant, so they boot unpaired but still publish their role's
/// topics. When
/// the launch is up, the daemon's observation coordinator has pushed each slot's
/// complete member set (in launcher order, at live generations) to the
/// recorder's `observation_update` service, and the omitted slot holds the empty
/// set. This is the observer analogue of
/// `stack_launch_establishes_launcher_pairings`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stack_launch_delivers_observer_member_sets() {
    let serve = ServeCommandEmulation::with_zenoh()
        .await
        .expect("failed to create zenoh serve emulation");
    let core_node_name = serve.core_node_name().to_string();

    let nodes_dir = tempfile::tempdir().expect("failed to create temp nodes directory");
    let ctx = Arc::new(
        AppContext::with_messenger(nodes_dir.path(), Arc::clone(&serve.messenger()))
            .with_daemon_state_file(serve.daemon_state_path()),
    );

    let repo_dir = tempfile::tempdir().expect("temp repo dir");
    super::common::seed_pairing_repo(&serve, &ctx, repo_dir.path());

    let git_hash = read_daemon_git_hash(serve.daemon_state_path());
    // Keep-alive rather than `sleep 30`: both instances must be live for the
    // pin assertions, and the teardown below ends them explicitly instead of
    // leaving the daemon to wait out its force-kill deadline once per instance.
    let instances = peppy::test_support::InstanceLifetime::new();
    let run_cmd = instances.keep_alive_argv();
    // The source plays the `arm` role through its participant slot `controller`,
    // declared optional so a watched-only deployment may write it vacant.
    let arm_path = write_node_config_for_helper(
        nodes_dir.path(),
        "robot_arm",
        "v1",
        &git_hash,
        &run_cmd,
        Some(
            r#"{ pairings: [{ name: "arm_link", tag: "v1", role: "arm", link_id: "controller", optional: true }] }"#,
        ),
        None,
        Some(
            r#"{ topics: {
                emits: [{ link_id: "controller", name: "joint_states" }],
                consumes: [{ link_id: "controller", name: "joint_commands" }]
            } }"#,
        ),
    );
    // The recorder observes the `arm` role through observer slot `watch`,
    // consuming the topic that role emits (`joint_states`).
    let recorder_path = write_node_config_for_helper(
        nodes_dir.path(),
        "recorder",
        "v1",
        &git_hash,
        &run_cmd,
        Some(
            r#"{ pairing_observers: [
                { name: "arm_link", tag: "v1", role: "arm", link_id: "watch" },
                { name: "arm_link", tag: "v1", role: "arm", link_id: "watched", cardinality: "one_or_more" },
                { name: "arm_link", tag: "v1", role: "arm", link_id: "spare", cardinality: "zero_or_more" }
            ] }"#,
        ),
        None,
        Some(
            r#"{ topics: { consumes: [
                { link_id: "watch", name: "joint_states" },
                { link_id: "watched", name: "joint_states" },
                { link_id: "spare", name: "joint_states" }
            ] } }"#,
        ),
    );

    let node_messenger = MessengerHandle::from_shared(Arc::clone(&serve.messenger()));
    // Source instance services: ready/health/shutdown, no peer_update (its slot
    // is vacant, so it is never paired).
    for (node_name, instance_id) in [("robot_arm", "arm_1"), ("robot_arm", "arm_2")] {
        let _ready = listen_for_node_ready(
            &node_messenger,
            &core_node_name,
            instance_id,
            test_node_target(node_name),
        )
        .await
        .expect("ready service should start");
        let _health = listen_for_node_health(
            &node_messenger,
            &core_node_name,
            instance_id,
            test_node_target(node_name),
        )
        .await
        .expect("health service should start");
        let (_shutdown, _) = listen_for_shutdown(
            &node_messenger,
            &core_node_name,
            instance_id,
            test_node_target(node_name),
        )
        .await
        .expect("shutdown service should start");
    }
    // Recorder services, including the `observation_update` endpoint whose watch
    // observes the delivered source pin.
    let _rec_ready = listen_for_node_ready(
        &node_messenger,
        &core_node_name,
        "rec_1",
        test_node_target("recorder"),
    )
    .await
    .expect("ready service should start");
    let _rec_health = listen_for_node_health(
        &node_messenger,
        &core_node_name,
        "rec_1",
        test_node_target("recorder"),
    )
    .await
    .expect("health service should start");
    let (_rec_shutdown, _) = listen_for_shutdown(
        &node_messenger,
        &core_node_name,
        "rec_1",
        test_node_target("recorder"),
    )
    .await
    .expect("shutdown service should start");
    // One watch channel per declared observer slot, exactly as a real node's
    // processor seeds them.
    let mut obs_senders = std::collections::BTreeMap::new();
    let mut obs_receivers = std::collections::BTreeMap::new();
    for link_id in ["watch", "watched", "spare"] {
        let (tx, rx) =
            tokio::sync::watch::channel(peppylib::messaging::ObservationState::unregistered());
        obs_senders.insert(link_id.to_string(), tx);
        obs_receivers.insert(link_id, rx);
    }
    let obs_slots = Arc::new(obs_senders);
    peppylib::services::observation_update::listen_for_observation_update(
        &node_messenger,
        &core_node_name,
        "rec_1",
        test_node_target("recorder"),
        obs_slots,
    )
    .await
    .expect("observation_update service should start");

    let launcher_path = nodes_dir.path().join("peppy_launcher.json5");
    let launcher_json5 = r#"{
            peppy_schema: "launcher/v1",
            deployments: [
                {
                    source: { name: "robot_arm:v1" },
                    instances: [
                        {
                            instance_id: "arm_1",
                            links: { controller: { vacant: "watched only: nothing drives this arm" } }
                        },
                        {
                            instance_id: "arm_2",
                            links: { controller: { vacant: "watched only: nothing drives this arm" } }
                        }
                    ]
                },
                {
                    source: { name: "recorder:v1" },
                    instances: [{
                        instance_id: "rec_1",
                        links: { watch: "arm_1", watched: ["arm_2", "arm_1"] }
                    }]
                }
            ]
        }"#;
    fs::write(&launcher_path, launcher_json5).expect("launcher config should be writable");
    register_repo_caches(
        serve.temp_dir(),
        &[
            ("robot_arm", "v1", &arm_path),
            ("recorder", "v1", &recorder_path),
        ],
    );

    StackCommand {
        command: StackCommands::Launch {
            place: Vec::new(),
            local: false,
            launcher_config_path: launcher_path,
            node_add_idle_timeout_secs: 60,
            node_build_idle_timeout_secs: 60,
            node_run_idle_timeout_secs: 60,
            max_timeout_secs: Some(120),
        },
    }
    .execute(&ctx)
    .expect("launch with an observer should succeed");

    // By the time launch returns, every observer slot holds its complete member
    // set, each member pinned to its source's producer-side `controller` slot at
    // a live incarnation.
    let slot_members = |link_id: &str| -> Vec<peppylib::messaging::ObservedMemberState> {
        obs_receivers[link_id].borrow().members.clone()
    };

    let watch = slot_members("watch");
    assert_eq!(watch.len(), 1, "a `one` slot holds exactly its one member");
    assert_eq!(watch[0].source.producer.instance_id, "arm_1");
    assert_eq!(watch[0].source.source_link_id, "controller");
    assert!(watch[0].source_live, "the source is Running, so it is live");
    assert!(
        watch[0].source_incarnation >= 1,
        "a live source carries a bumped incarnation incarnation, got {}",
        watch[0].source_incarnation
    );

    let watched = slot_members("watched");
    assert_eq!(
        watched
            .iter()
            .map(|member| member.source.producer.instance_id.as_str())
            .collect::<Vec<_>>(),
        ["arm_2", "arm_1"],
        "a multi slot holds its members in launcher order, not sorted"
    );
    assert!(
        watched.iter().all(|member| member.source_live
            && member.source.source_link_id == "controller"
            && member.source_incarnation >= 1),
        "every member is pinned to its source's participant slot at a live incarnation: {watched:?}"
    );

    assert!(
        slot_members("spare").is_empty(),
        "a `zero_or_more` slot the launcher omits boots with an empty member set"
    );

    // Assertions are done; ending the keep-alive first lets each Stop observe
    // an already-exited process instead of waiting out the force-kill deadline.
    drop(instances);
    for instance_id in ["rec_1", "arm_1", "arm_2"] {
        let _ = NodeCommand {
            command: NodeCommands::Stop {
                instance_id: instance_id.to_string(),
            },
        }
        .execute(&ctx);
    }
}

/// A required pairing slot with neither a `links:` entry (on either
/// side) nor a `{ vacant: "<why>" }` opt-out fails the launch at validation,
/// before anything is added or spawned.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stack_launch_rejects_uncovered_pairing_slot() {
    let serve = ServeCommandEmulation::with_mock()
        .await
        .expect("failed to create serve emulation");

    let nodes_dir = tempfile::tempdir().expect("failed to create temp nodes directory");
    let ctx = Arc::new(
        AppContext::with_messenger(nodes_dir.path(), Arc::clone(&serve.messenger()))
            .with_daemon_state_file(serve.daemon_state_path()),
    );

    let git_hash = read_daemon_git_hash(serve.daemon_state_path());
    let run_cmd = vec!["sleep".to_string(), "5".to_string()];
    let arm_path = write_node_config_for_helper(
        nodes_dir.path(),
        "robot_arm",
        "v1",
        &git_hash,
        &run_cmd,
        Some(
            r#"{ pairings: [{ name: "arm_link", tag: "v1", role: "arm", link_id: "controller" }] }"#,
        ),
        None,
        Some(
            r#"{ topics: {
                emits: [{ link_id: "controller", name: "joint_states" }],
                consumes: [{ link_id: "controller", name: "joint_commands" }]
            } }"#,
        ),
    );

    let launcher_path = nodes_dir.path().join("peppy_launcher.json5");
    let launcher_json5 = r#"{
            peppy_schema: "launcher/v1",
            deployments: [
                {
                    source: { name: "robot_arm:v1" },
                    instances: [{ instance_id: "arm_1" }]
                }
            ]
        }"#;
    fs::write(&launcher_path, launcher_json5).expect("launcher config should be writable");
    register_repo_caches(serve.temp_dir(), &[("robot_arm", "v1", &arm_path)]);

    let err = StackCommand {
        command: StackCommands::Launch {
            place: Vec::new(),
            local: false,
            launcher_config_path: launcher_path,
            node_add_idle_timeout_secs: 60,
            node_build_idle_timeout_secs: 60,
            node_run_idle_timeout_secs: 60,
            max_timeout_secs: Some(60),
        },
    }
    .execute(&ctx)
    .expect_err("an uncovered required pairing slot must fail the launch");
    let msg = err.to_string();
    assert!(
        msg.contains("controller") && msg.contains("--link") && msg.contains("`optional: true`"),
        "the failure should name the uncovered slot, the pairing key and the manifest key: {msg}"
    );
    assert!(
        msg.contains("if it is meant to run empty, declare"),
        "the vacancy must be offered only behind the manifest change, never as a standalone \
         fix: {msg}"
    );
}

/// A launcher file may only reference nodes by `name:tag`: a path-shaped
/// `local:` source fails the launch with an error naming the offending key.
/// Local nodes become resolvable by registering their directory with
/// `peppy repo add <path>` instead.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stack_launch_rejects_a_path_shaped_deployment_source() {
    let serve = ServeCommandEmulation::with_mock()
        .await
        .expect("failed to create serve emulation");
    let shared_messenger = serve.messenger();

    let nodes_dir = tempfile::tempdir().expect("failed to create temp nodes directory");
    let ctx = Arc::new(
        AppContext::with_messenger(nodes_dir.path(), Arc::clone(&shared_messenger))
            .with_daemon_state_file(serve.daemon_state_path()),
    );

    let launcher_path = nodes_dir.path().join("peppy_launcher.json5");
    let launcher_json5 = r#"{
        peppy_schema: "launcher/v1",
        deployments: [
            {
                source: { local: "./uvc_camera" },
                instances: [{ instance_id: "camera_front" }]
            }
        ]
    }"#;
    fs::write(&launcher_path, launcher_json5).expect("launcher config should be writable");

    let err = StackCommand {
        command: StackCommands::Launch {
            place: Vec::new(),
            local: false,
            launcher_config_path: launcher_path,
            node_add_idle_timeout_secs: 60,
            node_build_idle_timeout_secs: 60,
            node_run_idle_timeout_secs: 60,
            max_timeout_secs: Some(60),
        },
    }
    .execute(&ctx)
    .expect_err("a path-shaped deployment source must fail the launch");
    let msg = err.to_string();
    assert!(
        msg.contains("local"),
        "the error should name the offending key: {msg}"
    );
}

/// The first real consumer of observer cardinality, end to end over the
/// checked-in hub fixtures: a teleoperation panel whose four observer slots are
/// all `zero_or_more`, so one launch exercises every shape the feature has.
///
/// The stack is a backbone, two paired arm limbs and one paired gripper limb.
/// The commander links `observed_joints` to both followers (an ordered
/// two-member set), `commanded_joints` to one leader (a one-member set),
/// `observed_grippers` to the explicit empty set, and omits `commanded_grippers`
/// entirely. Its `recorder` producer slot is `zero_or_more` and left unbound, so
/// the same launch also covers the producer-side empty set the panel hides its
/// recording controls behind.
///
/// What it proves that the narrower observer tests do not: the plan's order
/// survives all the way to the node (member N is the deployment's Nth entry, so
/// the panel can pair a readout card with its own Nth command slot), the two
/// ways of writing "observes nothing" both arrive as an empty set, and one
/// member going down moves only its own member.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stack_launch_serves_a_commander_panels_observer_slots() {
    let instances = peppy::test_support::InstanceLifetime::new();
    let serve = ServeCommandEmulation::with_zenoh()
        .await
        .expect("failed to create zenoh serve emulation");
    let core_node_name = serve.core_node_name().to_string();

    let nodes_dir = tempfile::tempdir().expect("failed to create temp nodes directory");
    let ctx = Arc::new(
        AppContext::with_messenger(nodes_dir.path(), Arc::clone(&serve.messenger()))
            .with_daemon_state_file(serve.daemon_state_path()),
    );

    // The pairing and contract documents the fixtures reference.
    let repo_dir = tempfile::tempdir().expect("temp repo dir");
    stage_hub_docs(
        repo_dir.path(),
        &["joint_link", "gripper_link"],
        &["openarm_governor_control"],
    );
    super::common::seed_docs_repo(&serve, &ctx, repo_dir.path());

    let git_hash = read_daemon_git_hash(serve.daemon_state_path());
    let run_cmd = instances.keep_alive_argv();
    // The commander snapshots its own boot config so the producer-side empty
    // set can be read back from what the daemon actually assembled.
    let dump_dir = tempfile::tempdir().expect("temp dump dir");
    let commander_dump = dump_dir.path().join("commander.json5");
    let commander_run_cmd = vec![
        "sh".to_string(),
        "-c".to_string(),
        format!(
            "cp \"$PEPPY_RUNTIME_CONFIG\" \"{}\" && {}",
            commander_dump.display(),
            instances.keep_alive_script(),
        ),
    ];

    // `lerobot_recorder` is staged but never instantiated: the commander's
    // `recorder` slot is `zero_or_more`, so the deployment binds nothing to it,
    // and the manifest still has to resolve.
    let fixtures = [
        "openarm_backbone",
        "lerobot_recorder",
        "openarm_joint_leader",
        "openarm_joint_follower",
        "openarm_gripper_leader",
        "openarm_gripper_follower",
    ];
    let mut staged: Vec<(&str, &str, PathBuf)> = fixtures
        .iter()
        .map(|name| {
            (
                *name,
                "v1",
                stage_hub_fixture_node(nodes_dir.path(), name, &git_hash, &run_cmd),
            )
        })
        .collect();
    staged.push((
        "openarm_commander",
        "v1",
        stage_hub_fixture_node(
            nodes_dir.path(),
            "openarm_commander",
            &git_hash,
            &commander_run_cmd,
        ),
    ));
    register_repo_caches(
        serve.temp_dir(),
        &staged
            .iter()
            .map(|(name, tag, dir)| (*name, *tag, dir.as_path()))
            .collect::<Vec<_>>(),
    );

    // Every instance's framework services, impersonated from the test process:
    // the fixtures' run_cmd is a keep-alive shell, not a peppylib node.
    let node_messenger = MessengerHandle::from_shared(Arc::clone(&serve.messenger()));
    let mut service_handles = Vec::new();
    for (node_name, instance_id) in [
        ("openarm_backbone", "backbone_inst"),
        ("lerobot_recorder", "recorder_inst"),
        ("openarm_joint_leader", "leader_1"),
        ("openarm_joint_leader", "leader_2"),
        ("openarm_joint_follower", "follower_1"),
        ("openarm_joint_follower", "follower_2"),
        ("openarm_gripper_leader", "grip_leader_1"),
        ("openarm_gripper_follower", "grip_follower_1"),
        ("openarm_commander", "commander_inst"),
    ] {
        let ready = listen_for_node_ready(
            &node_messenger,
            &core_node_name,
            instance_id,
            test_node_target(node_name),
        )
        .await
        .expect("ready service should start");
        let health = listen_for_node_health(
            &node_messenger,
            &core_node_name,
            instance_id,
            test_node_target(node_name),
        )
        .await
        .expect("health service should start");
        let (shutdown, _) = listen_for_shutdown(
            &node_messenger,
            &core_node_name,
            instance_id,
            test_node_target(node_name),
        )
        .await
        .expect("shutdown service should start");
        service_handles.push((ready, health, shutdown));
    }
    // The limbs pair with each other, so each carries a `peer_update` endpoint.
    let mut peer_handles = Vec::new();
    for (node_name, instance_id) in [
        ("openarm_joint_leader", "leader_1"),
        ("openarm_joint_leader", "leader_2"),
        ("openarm_joint_follower", "follower_1"),
        ("openarm_joint_follower", "follower_2"),
        ("openarm_gripper_leader", "grip_leader_1"),
        ("openarm_gripper_follower", "grip_follower_1"),
    ] {
        let (tx, _rx) = tokio::sync::watch::channel(peppylib::messaging::PeerPinState::unpaired());
        let slots = Arc::new(std::collections::BTreeMap::from([("limb".to_string(), tx)]));
        peer_handles.push(
            peppylib::services::peer_update::listen_for_peer_update(
                &node_messenger,
                &core_node_name,
                instance_id,
                test_node_target(node_name),
                slots,
            )
            .await
            .expect("peer_update service should start"),
        );
    }
    // One observation watch per declared observer slot, as a real node's
    // processor seeds them.
    let mut obs_senders = std::collections::BTreeMap::new();
    let mut obs_receivers = std::collections::BTreeMap::new();
    for link_id in [
        "observed_joints",
        "commanded_joints",
        "observed_grippers",
        "commanded_grippers",
    ] {
        let (tx, rx) =
            tokio::sync::watch::channel(peppylib::messaging::ObservationState::unregistered());
        obs_senders.insert(link_id.to_string(), tx);
        obs_receivers.insert(link_id, rx);
    }
    let _obs_handle = peppylib::services::observation_update::listen_for_observation_update(
        &node_messenger,
        &core_node_name,
        "commander_inst",
        test_node_target("openarm_commander"),
        Arc::new(obs_senders),
    )
    .await
    .expect("observation_update service should start");

    let launcher_path = nodes_dir.path().join("peppy_launcher.json5");
    let launcher_json5 = r#"{
            peppy_schema: "launcher/v1",
            deployments: [
                {
                    source: { name: "openarm_backbone:v1" },
                    instances: [{ instance_id: "backbone_inst" }]
                },
                {
                    // Present in the stack but bound to nothing: the
                    // commander's `recorder` slot is `zero_or_more`, and
                    // omitting it must mean the empty set rather than
                    // "bind whatever happens to be running".
                    source: { name: "lerobot_recorder:v1" },
                    instances: [{ instance_id: "recorder_inst" }]
                },
                {
                    source: { name: "openarm_joint_leader:v1" },
                    instances: [
                        { instance_id: "leader_1", links: { limb: "follower_1" } },
                        { instance_id: "leader_2", links: { limb: "follower_2" } }
                    ]
                },
                {
                    source: { name: "openarm_joint_follower:v1" },
                    instances: [
                        { instance_id: "follower_1" },
                        { instance_id: "follower_2" }
                    ]
                },
                {
                    source: { name: "openarm_gripper_leader:v1" },
                    instances: [{ instance_id: "grip_leader_1", links: { limb: "grip_follower_1" } }]
                },
                {
                    source: { name: "openarm_gripper_follower:v1" },
                    instances: [{ instance_id: "grip_follower_1" }]
                },
                {
                    source: { name: "openarm_commander:v1" },
                    instances: [{
                        instance_id: "commander_inst",
                        links: {
                            backbone: "backbone_inst",
                            // Two followers, in the order the panel reads them
                            // back and pairs with its own command slots.
                            observed_joints: ["follower_1", "follower_2"],
                            commanded_joints: ["leader_1"],
                            // The explicit empty set, next to the omitted
                            // `commanded_grippers` and the omitted `recorder`.
                            observed_grippers: []
                        }
                    }]
                }
            ]
        }"#;
    fs::write(&launcher_path, launcher_json5).expect("launcher config should be writable");

    StackCommand {
        command: StackCommands::Launch {
            place: Vec::new(),
            local: false,
            launcher_config_path: launcher_path,
            node_add_idle_timeout_secs: 60,
            node_build_idle_timeout_secs: 60,
            node_run_idle_timeout_secs: 60,
            max_timeout_secs: Some(180),
        },
    }
    .execute(&ctx)
    .expect("launching the commander panel stack should succeed");

    // The unbound `zero_or_more` producer slot reaches the node as an explicit
    // empty set, so the panel's "is there a recorder?" branch is a set size and
    // never a missing key.
    let runtime_config: config::runtime::RuntimeConfig = serde_json5::from_str(
        &fs::read_to_string(&commander_dump).expect("commander runtime config should be dumped"),
    )
    .expect("commander runtime config should parse");
    let recorder = runtime_config
        .node_instance
        .slot_bindings
        .get("recorder")
        .expect("an unbound zero_or_more slot still materializes");
    assert!(
        recorder.is_empty(),
        "the recorder slot binds nothing in this deployment: {recorder:?}"
    );
    assert_eq!(
        runtime_config.node_instance.slot_bindings["backbone"]
            .iter()
            .map(|producer| producer.instance_id.as_str())
            .collect::<Vec<_>>(),
        ["backbone_inst"]
    );

    let slot_members = |link_id: &str| -> Vec<peppylib::messaging::ObservedMemberState> {
        obs_receivers[link_id].borrow().members.clone()
    };
    let observed_instances = |link_id: &str| -> Vec<String> {
        slot_members(link_id)
            .iter()
            .map(|member| member.source.producer.instance_id.clone())
            .collect()
    };

    assert_eq!(
        observed_instances("observed_joints"),
        ["follower_1", "follower_2"],
        "the panel reads its followers in the order the launcher wrote them"
    );
    assert_eq!(observed_instances("commanded_joints"), ["leader_1"]);
    assert!(
        slot_members("observed_joints")
            .iter()
            .chain(slot_members("commanded_joints").iter())
            .all(|member| member.source_live && member.source.source_link_id == "limb"),
        "every observed member is pinned to its source's participant slot and live"
    );
    for empty_slot in ["observed_grippers", "commanded_grippers"] {
        assert!(
            slot_members(empty_slot).is_empty(),
            "`{empty_slot}` observes nothing, whether written as [] or omitted"
        );
    }

    // One followed limb going down moves only its own member: the panel greys
    // that readout card and keeps the rest live.
    NodeCommand {
        command: NodeCommands::Stop {
            instance_id: "follower_2".to_string(),
        },
    }
    .execute(&ctx)
    .expect("stopping one observed follower should succeed");

    let members = slot_members("observed_joints");
    assert_eq!(
        members
            .iter()
            .map(|member| member.source.producer.instance_id.as_str())
            .collect::<Vec<_>>(),
        ["follower_1", "follower_2"],
        "a stopped source keeps its position in the set"
    );
    assert!(
        members[0].source_live,
        "the untouched member stays live: {:?}",
        members[0]
    );
    assert!(
        !members[1].source_live,
        "the stopped member reports source_live=false: {:?}",
        members[1]
    );

    drop(instances);
    for instance_id in [
        "commander_inst",
        "recorder_inst",
        "grip_follower_1",
        "grip_leader_1",
        "follower_1",
        "leader_2",
        "leader_1",
        "backbone_inst",
    ] {
        let _ = NodeCommand {
            command: NodeCommands::Stop {
                instance_id: instance_id.to_string(),
            },
        }
        .execute(&ctx);
    }
}
