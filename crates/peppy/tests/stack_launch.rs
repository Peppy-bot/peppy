use config::node::Toolchain;
use peppy::commands::repo::{RepoCommand, RepoCommands};
use peppy::test_support::{
    LogCapture, ServeCommandEmulation, override_build_cmd, override_run_cmd_silent,
};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use config::consts::{NODE_CONFIG_FILE, PEPPY_OUTPUT_DIR, PEPPYGEN_OUTPUT_PATH};
use core_node_api::SerializedNodeGraph;
use core_node_api::encoding::StackListRequest;
use peppy::commands::Command;
use peppy::commands::node::{NodeCommand, NodeCommands};
use peppy::commands::stack::{StackCommand, StackCommands};
use peppy::context::AppContext;
use peppylib::MessengerHandle;
use peppylib::services::health::listen_for_node_health;
use peppylib::services::ready::listen_for_node_ready;
use peppylib::services::shutdown::listen_for_shutdown;

use super::common::test_node_target;
use peppylib::core_node::transport::poll_stack_list;
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
                peppy_schema: "node_v1",
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

    let response = poll_stack_list(
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
            peppy_schema: "launcher_v1",
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

    let response = poll_stack_list(
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

    let response = poll_stack_list(
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
async fn node_launch_command_fails_when_node_never_becomes_healthy() {
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

    let response = poll_stack_list(
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
            peppy_schema: "launcher_v1",
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

    let response = poll_stack_list(
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
        graph
            .nodes
            .iter()
            .any(|n| n.label().contains(&format!("{node_a_name}:{node_tag}"))),
        "graph should still contain node_a after failed launch. Got: {:?}",
        graph.nodes.iter().map(|n| n.label()).collect::<Vec<_>>()
    );

    assert!(
        !graph
            .nodes
            .iter()
            .any(|n| n.label().contains(&format!("{node_b_name}:{node_tag}"))),
        "graph should not contain node_b after failed launch. Got: {:?}",
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
}

async fn setup_timeout_test(node_b_name: &'static str) -> TimeoutTestHarness {
    let serve = ServeCommandEmulation::with_mock()
        .await
        .expect("failed to create serve emulation");
    let shared_messenger = serve.messenger();
    let nodes_dir = tempfile::tempdir().expect("failed to create temp nodes directory");
    // Leak the tempdir so it survives for the duration of the test (the harness holds the
    // serve emulation, which already owns its own tempdir; we keep this one alive via the
    // launcher path it contains).
    let nodes_dir_path = nodes_dir.keep();

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
            peppy_schema: "launcher_v1",
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
            peppy_schema: "node_v1",
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
            peppy_schema: "launcher_v1",
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
        Some(&config::runtime::SlotBinding::Pinned {
            producer_instance_id: producer_instance_id.to_string(),
        }),
        "the launcher's binding `{link_id} -> {producer_instance_id}` should be present on the \
         consumer's runtime config as a Pinned slot binding",
    );
}

/// Stack-wide `instance_id` uniqueness (spec rule 7): two instances
/// anywhere in the launcher — even across different `(node_name,
/// node_tag)` pairs — sharing an `instance_id` must fail at the parse
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
    // the launcher level — instance_ids must be unique across the
    // entire stack, not merely within a `(node_name, node_tag)` group.
    let launcher_path = nodes_dir.path().join("peppy_launcher.json5");
    let launcher_json5 = format!(
        r#"{{
            peppy_schema: "launcher_v1",
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
    let response = poll_stack_list(
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

/// Writes a minimal `peppy_schema: "interface_v1"` document at `path`.
/// Used by the conformance-binding integration tests to materialize the
/// interface contract on disk alongside the producer/consumer node
/// configs that reference it.
fn write_interface_v1_doc(path: &Path, name: &str, tag: &str) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("failed to create interface parent dir");
    }
    let body = format!(
        r#"{{
            peppy_schema: "interface_v1",
            manifest: {{ name: "{name}", tag: "{tag}" }},
            interfaces: {{
                topics: [
                    {{
                        name: "video_stream",
                        qos_profile: "sensor_data",
                        message_format: {{
                            width: "u32",
                            height: "u32",
                            encoding: "string"
                        }}
                    }}
                ]
            }}
        }}"#
    );
    fs::write(path, body).expect("failed to write interface_v1 doc");
}

/// End-to-end check that a pinned interface binding resolves against
/// the producer's `interfaces.conforms_to` declaration. Exercises a real
/// `peppy_schema: "interface_v1"` document on disk alongside a `node_v1`
/// producer declaring conformance and a `node_v1` consumer declaring
/// the interface dep — pairs the unit-level binding validator tests
/// with the full launch pipeline (cache resolution + daemon node-add).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stack_launch_resolves_conforms_to_binding_with_real_interface_doc() {
    let serve = ServeCommandEmulation::with_zenoh()
        .await
        .expect("failed to create zenoh serve emulation");
    let core_node_name = serve.core_node_name().to_string();

    let nodes_dir = tempfile::tempdir().expect("failed to create temp nodes directory");
    let dump_dir = tempfile::tempdir().expect("failed to create temp dump directory");
    let interface_repo_dir = tempfile::tempdir().expect("failed to create temp interface repo");
    let consumer_dump = dump_dir.path().join("consumer.json5");

    let interface_name = "depth_camera_iface";
    let interface_tag = "v1";
    let producer_name = "realsense_d405";
    let consumer_name = "video_reconstruction";
    let node_tag = "v1";
    let producer_instance_id = "depth_cam_inst1";
    let consumer_instance_id = "video_rec_1";
    let link_id = "rear_camera";
    let git_hash = read_daemon_git_hash(serve.daemon_state_path());

    // Materialize the interface contract document on disk and register
    // the containing directory as an fs-type repo. The launcher binding
    // validator only inspects `conforms_to` claims, but the daemon's
    // node-add path also resolves the interface document from cache
    // for consumers declaring `depends_on.interfaces` — without the
    // repo refresh the consumer node-add would fail before
    // `validate_bindings` ever runs.
    write_interface_v1_doc(
        &interface_repo_dir
            .path()
            .join("depth_camera_iface/peppy.json5"),
        interface_name,
        interface_tag,
    );
    let conf_dir = serve.temp_dir().join("conf");
    fs::create_dir_all(&conf_dir).expect("create conf dir");
    let repos_content = serde_json::to_string_pretty(&serde_json::json!([
        { "id": 1, "type": "fs", "path": interface_repo_dir.path().to_string_lossy() }
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
            conforms_to: [{{ name: "{interface_name}", tag: "{interface_tag}" }}]
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
            interfaces: [{{
                name: "{interface_name}",
                tag: "{interface_tag}",
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
    .expect("repo refresh should populate interface cache");

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
            peppy_schema: "launcher_v1",
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
        Some(&config::runtime::SlotBinding::Pinned {
            producer_instance_id: producer_instance_id.to_string(),
        }),
        "interface dep `{link_id}` should resolve to the conforming producer's instance",
    );
}

/// Interface satisfaction is determined solely by `interfaces.conforms_to`,
/// never by node identity: a producer whose node name coincidentally
/// matches a consumer's interface dep name+tag but who declares no
/// `conforms_to` must be rejected at binding validation.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stack_launch_rejects_binding_when_producer_omits_conforms_to() {
    let serve = ServeCommandEmulation::with_mock()
        .await
        .expect("failed to create serve emulation");

    let nodes_dir = tempfile::tempdir().expect("failed to create temp nodes directory");

    // Interface name and producer node name intentionally collide on
    // `depth_camera:v1` to confirm the validator ignores node-identity
    // coincidence and requires an explicit `conforms_to` declaration.
    let interface_name = "depth_camera";
    let interface_tag = "v1";
    let producer_name = "depth_camera"; // intentional coincidence
    let consumer_name = "video_reconstruction";
    let node_tag = "v1";
    let producer_instance_id = "depth_cam_inst1";
    let consumer_instance_id = "video_rec_1";
    let link_id = "rear_camera";
    let git_hash = read_daemon_git_hash(serve.daemon_state_path());

    let interface_doc_path = nodes_dir.path().join("interfaces/depth_camera.peppy.json5");
    write_interface_v1_doc(&interface_doc_path, interface_name, interface_tag);

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
            interfaces: [{{
                name: "{interface_name}",
                tag: "{interface_tag}",
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
            peppy_schema: "launcher_v1",
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
/// declaring conformance to the right interface name but a different
/// tag must be rejected at binding validation.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stack_launch_rejects_binding_with_wrong_tag_in_conforms_to() {
    let serve = ServeCommandEmulation::with_mock()
        .await
        .expect("failed to create serve emulation");

    let nodes_dir = tempfile::tempdir().expect("failed to create temp nodes directory");

    let interface_name = "depth_camera";
    let consumer_wants_tag = "v1";
    let producer_claims_tag = "v2"; // mismatch
    let producer_name = "realsense_d405";
    let consumer_name = "video_reconstruction";
    let node_tag = "v1";
    let producer_instance_id = "depth_cam_inst1";
    let consumer_instance_id = "video_rec_1";
    let link_id = "rear_camera";
    let git_hash = read_daemon_git_hash(serve.daemon_state_path());

    // Both interface tags exist on disk so the test isn't ambiguous
    // about whether the doc is missing vs. the wrong one. Only the v1
    // contract is what the consumer asks for.
    write_interface_v1_doc(
        &nodes_dir
            .path()
            .join("interfaces/depth_camera_v1.peppy.json5"),
        interface_name,
        consumer_wants_tag,
    );
    write_interface_v1_doc(
        &nodes_dir
            .path()
            .join("interfaces/depth_camera_v2.peppy.json5"),
        interface_name,
        producer_claims_tag,
    );

    let run_cmd = vec!["sh".to_string(), "-c".to_string(), "sleep 30".to_string()];
    let producer_interfaces = format!(
        r#"{{
            conforms_to: [{{ name: "{interface_name}", tag: "{producer_claims_tag}" }}]
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
            interfaces: [{{
                name: "{interface_name}",
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
            peppy_schema: "launcher_v1",
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
        err_msg.contains(&format!("{interface_name}:{consumer_wants_tag}")),
        "error should name the requested interface `{interface_name}:{consumer_wants_tag}`. Got:\n{err_msg}"
    );
}
