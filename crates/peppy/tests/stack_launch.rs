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

fn write_node_config(
    nodes_directory: &Path,
    node_name: &str,
    node_tag: &str,
    git_hash: &str,
    run_cmd: &[&str],
) -> PathBuf {
    let node_dir = nodes_directory.join(node_name);
    fs::create_dir_all(&node_dir).expect("failed to create node directory");
    let node_config_path = node_dir.join(NODE_CONFIG_FILE);
    let run_cmd_json5 = run_cmd
        .iter()
        .map(|arg| serde_json::to_string(arg).expect("run_cmd arg should serialize"))
        .collect::<Vec<_>>()
        .join(", ");
    fs::write(
        &node_config_path,
        r#"{
                peppy_schema: "node/v1",
                manifest: {
                    name: "{node_name}",
                    tag: "{node_tag}",
                },
                execution: {
                    language: "rust",
                    run_cmd: [{run_cmd_json5}]
                }
            }"#
        .replace("{node_name}", node_name)
        .replace("{node_tag}", node_tag)
        .replace("{run_cmd_json5}", &run_cmd_json5),
    )
    .expect("failed to write node config");
    config::fingerprint::create_codegen_fingerprint(
        &node_config_path,
        Path::new(PEPPYGEN_OUTPUT_PATH),
    );

    let peppy_output_dir = node_dir.join(PEPPY_OUTPUT_DIR);
    fs::create_dir_all(&peppy_output_dir).expect("failed to create peppy output directory");
    fs::write(peppy_output_dir.join("git.hash"), git_hash).expect("failed to write node git hash");
    node_dir
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
            binds: Vec::new(),
            pairs: Vec::new(),
            defer_pairs: Vec::new(),
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
    peppy::test_support::override_run_cmd(&node_b_peppy_json5_path);

    let messenger_handle = ctx
        .messenger_handle()
        .expect("messenger handle should be available");

    let response = poll(
        &StackListRequest::new(false),
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
    let (_node_shutdown_handle, _node_shutdown_rx) = listen_for_shutdown(
        &node_messenger,
        &core_node_name,
        instance_id,
        test_node_target(node_b_name),
    )
    .await
    .expect("node shutdown service should start");

    let launcher_path = nodes_dir.path().join("peppy_launcher.json5");
    let node_b_path = nodes_dir.path().join(node_b_name);
    let launcher_json5 = format!(
        r#"{{
            peppy_schema: "launcher/v1",
            deployments: [
                {{
                    source: {{ local: "{}" }},
                    instances: [{{ instance_id: "{instance_id}" }}]
                }}
            ]
        }}"#,
        node_b_path.display()
    );
    fs::write(&launcher_path, launcher_json5).expect("launcher config should be writable");

    StackCommand {
        command: StackCommands::Launch {
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
        &StackListRequest::new(false),
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
        &StackListRequest::new(false),
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
            binds: Vec::new(),
            pairs: Vec::new(),
            defer_pairs: Vec::new(),
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
        &StackListRequest::new(false),
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
                    source: {{ local: "{}" }},
                    instances: [{{ instance_id: "node_b_instance" }}]
                }}
            ]
        }}"#,
        node_b_path.display()
    );
    fs::write(&launcher_path, launcher_json5).expect("launcher config should be writable");

    let launch_result = StackCommand {
        command: StackCommands::Launch {
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
        &StackListRequest::new(false),
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
                    source: {{ local: "{}" }},
                    instances: [{{ instance_id: "node_b_instance" }}]
                }}
            ]
        }}"#,
        node_b_path.display()
    );
    fs::write(&launcher_path, launcher_json5).expect("launcher config should be writable");

    TimeoutTestHarness {
        ctx,
        launcher_path,
        node_b_peppy_json5,
        _serve: serve,
        _nodes_dir: nodes_dir,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn node_launch_fails_when_node_build_idle_timeout_is_hit() {
    let harness = setup_timeout_test("build_idle_node_b").await;

    override_build_cmd(
        &harness.node_b_peppy_json5,
        vec!["sh".to_string(), "-c".to_string(), "sleep 30".to_string()],
    );

    let started = Instant::now();
    let result = StackCommand {
        command: StackCommands::Launch {
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

    let started = Instant::now();
    let result = StackCommand {
        command: StackCommands::Launch {
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

    let started = Instant::now();
    let result = StackCommand {
        command: StackCommands::Launch {
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
/// build phase is skipped), an optional `depends_on` block, and an optional
/// top-level `interfaces` block. Mirrors `write_node_config` but accepts
/// run_cmd as owned strings and manifest/interfaces extensions for tests
/// that need to exercise binding resolution against `conforms_to`.
fn write_node_config_for_helper(
    nodes_directory: &Path,
    node_name: &str,
    node_tag: &str,
    git_hash: &str,
    run_cmd: &[String],
    depends_on_json5: Option<&str>,
    interfaces_json5: Option<&str>,
) -> PathBuf {
    let node_dir = nodes_directory.join(node_name);
    fs::create_dir_all(&node_dir).expect("failed to create node directory");
    let node_config_path = node_dir.join(NODE_CONFIG_FILE);
    let run_cmd_json5 = run_cmd
        .iter()
        .map(|arg| serde_json::to_string(arg).expect("run_cmd arg should serialize"))
        .collect::<Vec<_>>()
        .join(", ");
    let manifest_extra = depends_on_json5
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
    fs::write(&node_config_path, body).expect("failed to write node config");
    config::fingerprint::create_codegen_fingerprint(
        &node_config_path,
        Path::new(PEPPYGEN_OUTPUT_PATH),
    );

    let peppy_output_dir = node_dir.join(PEPPY_OUTPUT_DIR);
    fs::create_dir_all(&peppy_output_dir).expect("failed to create peppy output directory");
    fs::write(peppy_output_dir.join("git.hash"), git_hash).expect("failed to write node git hash");
    node_dir
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
            "cp \"$PEPPY_RUNTIME_CONFIG\" \"{}\" && exec sleep 30",
            producer_dump.display()
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
    );

    let consumer_run_cmd = vec![
        "sh".to_string(),
        "-c".to_string(),
        format!(
            "cp \"$PEPPY_RUNTIME_CONFIG\" \"{}\" && exec sleep 30",
            consumer_dump.display()
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
                    source: {{ local: "{producer_path}" }},
                    instances: [{{ instance_id: "{producer_instance_id}" }}]
                }},
                {{
                    source: {{ local: "{consumer_path}" }},
                    instances: [{{
                        instance_id: "{consumer_instance_id}",
                        bindings: {{ {link_id}: "{producer_instance_id}" }}
                    }}]
                }}
            ]
        }}"#,
        producer_path = producer_path.display(),
        consumer_path = consumer_path.display(),
    );
    fs::write(&launcher_path, launcher_json5).expect("launcher config should be writable");

    StackCommand {
        command: StackCommands::Launch {
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

    // Stop both instances after both dumps are captured so a failure
    // doesn't leak `sleep 30` subprocesses past the test.
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
        Some(&config::runtime::ProducerRef::new(
            &core_node_name,
            producer_instance_id
        )),
        "the launcher's binding `{link_id} -> {producer_instance_id}` should be present on the \
         consumer's runtime config as a Pinned slot binding stamped with the daemon's core_node",
    );
}

/// Stack-wide `instance_id` uniqueness (spec rule 7): two instances
/// anywhere in the launcher, even across different `(node_name,
/// node_tag)` pairs, sharing an `instance_id` must fail at the parse
/// stage, before any node is added, built, or spawned. The binding
/// model addresses producers by raw `instance_id` so a duplicate
/// would make `--bind KEY@id` ambiguous.
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
                    source: {{ local: "{camera}" }},
                    instances: [{{ instance_id: "shared_inst" }}]
                }},
                {{
                    source: {{ local: "{lidar}" }},
                    instances: [{{ instance_id: "shared_inst" }}]
                }}
            ]
        }}"#,
        camera = camera_path.display(),
        lidar = lidar_path.display(),
    );
    fs::write(&launcher_path, launcher_json5).expect("launcher config should be writable");

    let result = StackCommand {
        command: StackCommands::Launch {
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
        &StackListRequest::new(false),
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
/// a single `video_stream` topic. Used by the conformance-binding
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
/// the producer's `interfaces.conforms_to` declaration. Exercises a real
/// `peppy_schema: "contract/v1"` document on disk alongside a `node/v1`
/// producer declaring conformance and a `node/v1` consumer declaring
/// the contract dep; pairs the unit-level binding validator tests
/// with the full launch pipeline (cache resolution + daemon node-add).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stack_launch_resolves_conforms_to_binding_with_real_contract_doc() {
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
    // validator only inspects `conforms_to` claims, but the daemon's
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
        "exec sleep 30".to_string(),
    ];
    let producer_interfaces = format!(
        r#"{{
            conforms_to: [{{ name: "{contract_name}", tag: "{contract_tag}" }}]
        }}"#
    );
    let producer_path = write_node_config_for_helper(
        nodes_dir.path(),
        producer_name,
        node_tag,
        &git_hash,
        &producer_run_cmd,
        None,
        Some(&producer_interfaces),
    );

    let consumer_run_cmd = vec![
        "sh".to_string(),
        "-c".to_string(),
        format!(
            "cp \"$PEPPY_RUNTIME_CONFIG\" \"{}\" && exec sleep 30",
            consumer_dump.display()
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
                    source: {{ local: "{producer_path}" }},
                    instances: [{{ instance_id: "{producer_instance_id}" }}]
                }},
                {{
                    source: {{ local: "{consumer_path}" }},
                    instances: [{{
                        instance_id: "{consumer_instance_id}",
                        bindings: {{ {link_id}: "{producer_instance_id}" }}
                    }}]
                }}
            ]
        }}"#,
        producer_path = producer_path.display(),
        consumer_path = consumer_path.display(),
    );
    fs::write(&launcher_path, launcher_json5).expect("launcher config should be writable");

    StackCommand {
        command: StackCommands::Launch {
            launcher_config_path: launcher_path,
            node_add_idle_timeout_secs: 60,
            node_build_idle_timeout_secs: 60,
            node_run_idle_timeout_secs: 60,
            max_timeout_secs: Some(120),
        },
    }
    .execute(&ctx)
    .expect("launch should succeed with conforming producer");

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
        Some(&config::runtime::ProducerRef::new(
            &core_node_name,
            producer_instance_id
        )),
        "contract dep `{link_id}` should resolve to the conforming producer's instance \
         stamped with the daemon's core_node",
    );
}

/// Contract satisfaction is determined solely by `interfaces.conforms_to`,
/// never by node identity: a producer whose node name coincidentally
/// matches a consumer's contract dep name+tag but who declares no
/// `conforms_to` must be rejected at binding validation.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stack_launch_rejects_binding_when_producer_omits_conforms_to() {
    let serve = ServeCommandEmulation::with_mock()
        .await
        .expect("failed to create serve emulation");

    let nodes_dir = tempfile::tempdir().expect("failed to create temp nodes directory");

    // Contract name and producer node name intentionally collide on
    // `depth_camera:v1` to confirm the validator ignores node-identity
    // coincidence and requires an explicit `conforms_to` declaration.
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
    // Producer omits `interfaces.conforms_to` entirely.
    let producer_path = write_node_config_for_helper(
        nodes_dir.path(),
        producer_name,
        node_tag,
        &git_hash,
        &run_cmd,
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
                    source: {{ local: "{producer_path}" }},
                    instances: [{{ instance_id: "{producer_instance_id}" }}]
                }},
                {{
                    source: {{ local: "{consumer_path}" }},
                    instances: [{{
                        instance_id: "{consumer_instance_id}",
                        bindings: {{ {link_id}: "{producer_instance_id}" }}
                    }}]
                }}
            ]
        }}"#,
        producer_path = producer_path.display(),
        consumer_path = consumer_path.display(),
    );
    fs::write(&launcher_path, launcher_json5).expect("launcher config should be writable");

    let result = StackCommand {
        command: StackCommands::Launch {
            launcher_config_path: launcher_path,
            node_add_idle_timeout_secs: 60,
            node_build_idle_timeout_secs: 60,
            node_run_idle_timeout_secs: 60,
            max_timeout_secs: Some(120),
        },
    }
    .execute(&ctx);

    let err_msg = result
        .expect_err("launch must fail when producer omits conforms_to")
        .to_string();
    assert!(
        err_msg.contains("conforms_to"),
        "error should mention conforms_to. Got:\n{err_msg}"
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

/// `conforms_to` matching is strict on `(name, tag)`: a producer
/// declaring conformance to the right contract name but a different
/// tag must be rejected at binding validation.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stack_launch_rejects_binding_with_wrong_tag_in_conforms_to() {
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
    let producer_interfaces = format!(
        r#"{{
            conforms_to: [{{ name: "{contract_name}", tag: "{producer_claims_tag}" }}]
        }}"#
    );
    let producer_path = write_node_config_for_helper(
        nodes_dir.path(),
        producer_name,
        node_tag,
        &git_hash,
        &run_cmd,
        None,
        Some(&producer_interfaces),
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
                    source: {{ local: "{producer_path}" }},
                    instances: [{{ instance_id: "{producer_instance_id}" }}]
                }},
                {{
                    source: {{ local: "{consumer_path}" }},
                    instances: [{{
                        instance_id: "{consumer_instance_id}",
                        bindings: {{ {link_id}: "{producer_instance_id}" }}
                    }}]
                }}
            ]
        }}"#,
        producer_path = producer_path.display(),
        consumer_path = consumer_path.display(),
    );
    fs::write(&launcher_path, launcher_json5).expect("launcher config should be writable");

    let result = StackCommand {
        command: StackCommands::Launch {
            launcher_config_path: launcher_path,
            node_add_idle_timeout_secs: 60,
            node_build_idle_timeout_secs: 60,
            node_run_idle_timeout_secs: 60,
            max_timeout_secs: Some(120),
        },
    }
    .execute(&ctx);

    let err_msg = result
        .expect_err("launch must fail when producer's conforms_to tag mismatches")
        .to_string();
    assert!(
        err_msg.contains("conforms_to"),
        "error should mention conforms_to. Got:\n{err_msg}"
    );
    assert!(
        err_msg.contains(&format!("{contract_name}:{consumer_wants_tag}")),
        "error should name the requested contract `{contract_name}:{consumer_wants_tag}`. Got:\n{err_msg}"
    );
}

/// Bidirectional contract communication under explicit bindings: two
/// nodes each emit one contract (`conforms_to`) and consume the other
/// through a contract dep, each slot bound to the other instance in the
/// launcher (conformance-matched, not node-identity-matched). The
/// launcher must materialize both slots with the producer's full wire
/// address, and the stack must come up regardless of deployment order.
/// This is the end-to-end counterpart to the unit-level binding
/// validator tests and the wire-level flow test in
/// `peppylib/tests/topics.rs`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stack_launch_binds_contract_slots_in_both_directions() {
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
    let conf_dir = serve.temp_dir().join("conf");
    fs::create_dir_all(&conf_dir).expect("create conf dir");
    let repos_content = serde_json::to_string_pretty(&serde_json::json!([
        { "id": 1, "type": "fs", "path": contract_repo_dir.path().to_string_lossy() }
    ]))
    .expect("serialize repos");
    fs::write(conf_dir.join("repositories.json5"), repos_content).expect("write repos");

    // arm_controller: emits joint_commands (conforms_to joint_command_source),
    // consumes joint_states through its `arm` slot (bound in the launcher).
    let controller_run_cmd = vec![
        "sh".to_string(),
        "-c".to_string(),
        format!(
            "cp \"$PEPPY_RUNTIME_CONFIG\" \"{}\" && exec sleep 30",
            controller_dump.display()
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
    let controller_interfaces = format!(
        r#"{{
            conforms_to: [{{ name: "{command_contract}", tag: "{contract_tag}" }}]
        }}"#
    );
    let controller_path = write_node_config_for_helper(
        nodes_dir.path(),
        controller_name,
        node_tag,
        &git_hash,
        &controller_run_cmd,
        Some(&controller_depends_on),
        Some(&controller_interfaces),
    );

    // robot_arm: emits joint_states (conforms_to joint_state_source),
    // consumes joint_commands through its `controller` slot, bound back
    // to the controller in the launcher (every declared slot must be
    // bound).
    let arm_run_cmd = vec![
        "sh".to_string(),
        "-c".to_string(),
        format!(
            "cp \"$PEPPY_RUNTIME_CONFIG\" \"{}\" && exec sleep 30",
            arm_dump.display()
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
    let arm_interfaces = format!(
        r#"{{
            conforms_to: [{{ name: "{state_contract}", tag: "{contract_tag}" }}]
        }}"#
    );
    let arm_path = write_node_config_for_helper(
        nodes_dir.path(),
        arm_name,
        node_tag,
        &git_hash,
        &arm_run_cmd,
        Some(&arm_depends_on),
        Some(&arm_interfaces),
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
                    source: {{ local: "{controller_path}" }},
                    instances: [{{
                        instance_id: "{controller_instance_id}",
                        bindings: {{ {controller_link_id}: "{arm_instance_id}" }}
                    }}]
                }},
                {{
                    source: {{ local: "{arm_path}" }},
                    instances: [{{
                        instance_id: "{arm_instance_id}",
                        bindings: {{ {arm_link_id}: "{controller_instance_id}" }}
                    }}]
                }}
            ]
        }}"#,
        controller_path = controller_path.display(),
        arm_path = arm_path.display(),
    );
    fs::write(&launcher_path, launcher_json5).expect("launcher config should be writable");

    StackCommand {
        command: StackCommands::Launch {
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
        Some(&config::runtime::ProducerRef::new(
            &core_node_name,
            arm_instance_id,
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
        Some(&config::runtime::ProducerRef::new(
            &core_node_name,
            controller_instance_id,
        )),
        "robot_arm's `{arm_link_id}` interface slot should materialize with the bound \
         producer's full wire address",
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
    );

    let ctx = Arc::new(
        AppContext::with_messenger(nodes_dir.path(), Arc::clone(&shared_messenger))
            .with_daemon_state_file(serve.daemon_state_path()),
    );

    // The consumer instance carries no `bindings:` map at all, so its
    // declared `main_cam` slot is unfulfilled.
    let launcher_path = nodes_dir.path().join("peppy_launcher.json5");
    let launcher_json5 = format!(
        r#"{{
            peppy_schema: "launcher/v1",
            deployments: [
                {{
                    source: {{ local: "{producer}" }},
                    instances: [{{ instance_id: "cam_1" }}]
                }},
                {{
                    source: {{ local: "{consumer}" }},
                    instances: [{{ instance_id: "cons_1" }}]
                }}
            ]
        }}"#,
        producer = producer_path.display(),
        consumer = consumer_path.display(),
    );
    fs::write(&launcher_path, launcher_json5).expect("launcher config should be writable");

    let result = StackCommand {
        command: StackCommands::Launch {
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
        err_msg.contains("--bind main_cam@"),
        "error should show the exact bind syntax that fixes it. Got:\n{err_msg}"
    );

    // No spawn side-effect: neither node should appear in the stack.
    let messenger_handle = ctx
        .messenger_handle()
        .expect("messenger handle should be available");
    let response = poll(
        &StackListRequest::new(false),
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
/// endpoint boots deferred, the later one carries the request, and both
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
    let run_cmd = vec!["sleep".to_string(), "30".to_string()];
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
    let launcher_json5 = format!(
        r#"{{
            peppy_schema: "launcher/v1",
            deployments: [
                {{
                    source: {{ local: "{arm_path}" }},
                    instances: [{{ instance_id: "arm_1" }}]
                }},
                {{
                    source: {{ local: "{ctrl_path}" }},
                    instances: [{{
                        instance_id: "ctrl_1",
                        pairings: {{ arm: "arm_1" }}
                    }}]
                }}
            ]
        }}"#,
        arm_path = arm_path.display(),
        ctrl_path = ctrl_path.display(),
    );
    fs::write(&launcher_path, launcher_json5).expect("launcher config should be writable");

    StackCommand {
        command: StackCommands::Launch {
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

    for instance_id in ["ctrl_1", "arm_1"] {
        let _ = NodeCommand {
            command: NodeCommands::Stop {
                instance_id: instance_id.to_string(),
            },
        }
        .execute(&ctx);
    }
}

/// A required pairing slot with neither a `pairings:` entry (on either
/// side) nor a `defer_pairings:` opt-out fails the launch at validation,
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
    );

    let launcher_path = nodes_dir.path().join("peppy_launcher.json5");
    let launcher_json5 = format!(
        r#"{{
            peppy_schema: "launcher/v1",
            deployments: [
                {{
                    source: {{ local: "{arm_path}" }},
                    instances: [{{ instance_id: "arm_1" }}]
                }}
            ]
        }}"#,
        arm_path = arm_path.display(),
    );
    fs::write(&launcher_path, launcher_json5).expect("launcher config should be writable");

    let err = StackCommand {
        command: StackCommands::Launch {
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
        msg.contains("controller") && (msg.contains("pairings") || msg.contains("defer_pairings")),
        "the failure should name the uncovered slot and the launcher keys: {msg}"
    );
}
