use config::node::Toolchain;
use peppy::test_support::{
    LogCapture, ServeCommandEmulation, override_build_cmd, override_run_cmd_silent,
};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use config::consts::{NODE_CONFIG_FILE, PEPPY_OUTPUT_DIR, PEPPYGEN_OUTPUT_PATH};
use core_node_api::encoding::StackListRequest;
use node_stack::SerializedNodeGraph;
use peppy::commands::Command;
use peppy::commands::node::{NodeCommand, NodeCommands};
use peppy::commands::stack::{StackCommand, StackCommands};
use peppy::context::AppContext;
use peppylib::MessengerHandle;
use peppylib::services::health::listen_for_node_health;
use peppylib::services::ready::listen_for_node_ready;
use peppylib::services::shutdown::listen_for_shutdown;

use core_node::transport::StackListRequestPollExt;
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
                schema_version: 1,
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
        serde_json::from_str(&contents).expect("daemon state should parse as JSON");
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
    let node_tag = "0.1.0";
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
            variant: Vec::new(),
            sync: false,
            build: true,
            run: false,
            args: Vec::new(),
            instance_id: None,
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

    let response = StackListRequest::new(false)
        .poll(
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
    let _node_ready_handle =
        listen_for_node_ready(&node_messenger, &core_node_name, instance_id, node_b_name)
            .await
            .expect("node ready service should start");
    let _node_health_handle =
        listen_for_node_health(&node_messenger, &core_node_name, instance_id, node_b_name)
            .await
            .expect("node health service should start");
    let (_node_shutdown_handle, _node_shutdown_rx) =
        listen_for_shutdown(&node_messenger, &core_node_name, instance_id, node_b_name)
            .await
            .expect("node shutdown service should start");

    let launcher_path = nodes_dir.path().join("peppy_launcher.json5");
    let node_b_path = nodes_dir.path().join(node_b_name);
    let launcher_json5 = format!(
        r#"{{
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

    let response = StackListRequest::new(false)
        .poll(
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

    let node_b = graph
        .nodes
        .iter()
        .find(|n| n.name == node_b_name && n.tag == node_tag)
        .unwrap_or_else(|| {
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

    let response = StackListRequest::new(false)
        .poll(
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

    let node_b = graph
        .nodes
        .iter()
        .find(|n| n.name == node_b_name && n.tag == node_tag)
        .unwrap_or_else(|| {
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
    let node_tag = "0.1.0";
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
            variant: Vec::new(),
            sync: false,
            build: true,
            run: false,
            args: Vec::new(),
            instance_id: None,
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

    let response = StackListRequest::new(false)
        .poll(
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

    let response = StackListRequest::new(false)
        .poll(
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
