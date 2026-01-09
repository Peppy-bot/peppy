mod helpers;

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use config::consts::NODE_CONFIG_FILE;
use helpers::TestServeHandle;
use master_node::encoding::NodeListRequest;
use node_stack::SerializedNodeGraph;
use peppy::commands::Command;
use peppy::commands::node::{NodeCommand, NodeCommands};
use peppy::commands::stack::{StackCommand, StackCommands};
use peppy::context::{AppContext, DaemonState};

const CALLER_INSTANCE_ID: &str = "peppy-test";

fn write_node_config(
    nodes_directory: &Path,
    node_name: &str,
    node_tag: &str,
    start_cmd: &[&str],
) -> PathBuf {
    let node_dir = nodes_directory.join(node_name);
    fs::create_dir_all(&node_dir).expect("failed to create node directory");
    let node_config_path = node_dir.join(NODE_CONFIG_FILE);
    let start_cmd_json5 = start_cmd
        .iter()
        .map(|arg| serde_json::to_string(arg).expect("start_cmd arg should serialize"))
        .collect::<Vec<_>>()
        .join(", ");
    fs::write(
        &node_config_path,
        format!(
            r#"{{
                schema_version: 1,
                manifest: {{
                    name: "{node_name}",
                    tag: "{node_tag}",
                    start_cmd: [{start_cmd_json5}]
                }}
            }}"#
        ),
    )
    .expect("failed to write node config");
    node_dir
}

#[test]
fn node_launch_command_succeed() {
    let _serial_guard = helpers::serve_test_guard();
    let serve = TestServeHandle::with_zenoh();

    let daemon_state = DaemonState::read().expect("daemon state should be readable");
    let master_node_name = daemon_state.master_node_name;
    assert!(
        !master_node_name.is_empty(),
        "master_node_name should not be empty"
    );

    let nodes_dir = tempfile::tempdir().expect("failed to create temp nodes directory");
    let node_a_name = "launch_succeed_node_a";
    let node_b_name = "launch_succeed_node_b";
    let node_tag = "0.1.0";
    let node_a_path = write_node_config(
        nodes_dir.path(),
        node_a_name,
        node_tag,
        &["sh", "-c", "exit 0"],
    );

    let ctx = Arc::new(AppContext::with_messenger(
        nodes_dir.path(),
        serve.messenger(),
    ));

    let log_capture = serve.log_capture().clone();
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .without_time()
        .with_writer(log_capture.clone())
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);

    NodeCommand {
        command: NodeCommands::Add {
            node_dir: node_a_path,
            start: false,
            args: Vec::new(),
            instance_id: None,
        },
    }
    .execute(&ctx)
    .expect("node add command should succeed");

    NodeCommand {
        command: NodeCommands::Init {
            node_name: peppy::commands::node::NodeName::new(node_b_name).expect("valid node name"),
            to_dir: None,
            build_system: config::peppy_config::BuildSystem::Rust,
        },
    }
    .execute(&ctx)
    .expect("node init command should succeed");

    let node_b_path = nodes_dir.path().join(node_b_name);
    let build_output = std::process::Command::new("cargo")
        .args(["build", "--release"])
        .current_dir(&node_b_path)
        .output()
        .expect("cargo build should start");
    assert!(
        build_output.status.success(),
        "cargo build --release failed.\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&build_output.stdout),
        String::from_utf8_lossy(&build_output.stderr)
    );

    let rt = tokio::runtime::Runtime::new().expect("tokio runtime should create");
    let messenger_handle = ctx
        .messenger_handle()
        .expect("messenger handle should be available");

    let response = rt
        .block_on(NodeListRequest::new(false).poll(
            messenger_handle,
            &master_node_name,
            CALLER_INSTANCE_ID,
            &master_node_name,
            Duration::from_secs(5),
        ))
        .expect("node_list request should complete");

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

    let launcher_path = nodes_dir.path().join("peppy_launcher.json5");
    let launcher_json5 = format!(
        r#"{{
            deployments: [
                {{
                    name: "{node_b_name}",
                    tag: "{node_tag}",
                    instances: [{{ instance_id: "{instance_id}" }}]
                }}
            ],
            logging: {{ min_level: "info", format: "text" }}
        }}"#
    );
    fs::write(&launcher_path, launcher_json5).expect("launcher config should be writable");

    StackCommand {
        command: StackCommands::Launch {
            launcher_config_path: launcher_path,
        },
    }
    .execute(&ctx)
    .expect("launch command should succeed");

    let response = rt
        .block_on(NodeListRequest::new(false).poll(
            messenger_handle,
            &master_node_name,
            CALLER_INSTANCE_ID,
            &master_node_name,
            Duration::from_secs(5),
        ))
        .expect("node_list request should complete after launch");

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

    assert!(
        graph.nodes.iter().any(|n| {
            n.label().contains(&format!("{node_b_name}:{node_tag}"))
                && n.label().contains("(1 instance)")
        }),
        "graph should contain node_b with 1 instance after launch. Got: {:?}",
        graph.nodes.iter().map(|n| n.label()).collect::<Vec<_>>()
    );

    // TODO we shouldn't need to stop the instances manually. If the master node stops, all child instances pid should stop too
    NodeCommand {
        command: NodeCommands::Stop {
            instance_id: instance_id.to_string(),
        },
    }
    .execute(&ctx)
    .expect("node stop command should succeed");

    let response = rt
        .block_on(NodeListRequest::new(false).poll(
            messenger_handle,
            &master_node_name,
            CALLER_INSTANCE_ID,
            &master_node_name,
            Duration::from_secs(5),
        ))
        .expect("node_list request should complete after stop");

    let graph: SerializedNodeGraph =
        serde_json::from_str(&response.graph_json).expect("graph_json should parse after stop");

    assert!(
        graph.nodes.iter().any(|n| n
            .label()
            .contains(&format!("{node_b_name}:{node_tag} (0 instances)"))),
        "graph should contain node_b with 0 instances after stop. Got: {:?}",
        graph.nodes.iter().map(|n| n.label()).collect::<Vec<_>>()
    );
}

#[test]
fn node_launch_command_fails_when_node_never_becomes_healthy() {
    let _serial_guard = helpers::serve_test_guard();
    let serve = TestServeHandle::with_mock_messenger();

    let daemon_state = DaemonState::read().expect("daemon state should be readable");
    let master_node_name = daemon_state.master_node_name;
    assert!(
        !master_node_name.is_empty(),
        "master_node_name should not be empty"
    );

    let nodes_dir = tempfile::tempdir().expect("failed to create temp nodes directory");
    let node_a_name = "launch_node_a";
    let node_b_name = "launch_node_b";
    let node_tag = "0.1.0";
    let node_a_path = write_node_config(
        nodes_dir.path(),
        node_a_name,
        node_tag,
        &["sh", "-c", "exit 0"],
    );
    let _node_b_path = write_node_config(
        nodes_dir.path(),
        node_b_name,
        node_tag,
        &["sh", "-c", "exit 0"],
    );

    let ctx = Arc::new(AppContext::with_messenger(
        nodes_dir.path(),
        serve.messenger(),
    ));

    let log_capture = serve.log_capture().clone();
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .without_time()
        .with_writer(log_capture.clone())
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);

    NodeCommand {
        command: NodeCommands::Add {
            node_dir: node_a_path,
            start: false,
            args: Vec::new(),
            instance_id: None,
        },
    }
    .execute(&ctx)
    .expect("node add command should succeed");

    let rt = tokio::runtime::Runtime::new().expect("tokio runtime should create");
    let messenger_handle = ctx
        .messenger_handle()
        .expect("messenger handle should be available");

    let response = rt
        .block_on(NodeListRequest::new(false).poll(
            messenger_handle,
            &master_node_name,
            CALLER_INSTANCE_ID,
            &master_node_name,
            Duration::from_secs(5),
        ))
        .expect("node_list request should complete");

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
                    name: "{node_b_name}",
                    tag: "{node_tag}",
                    instances: [{{ instance_id: "node_b_instance" }}]
                }}
            ],
            logging: {{ min_level: "info", format: "text" }}
        }}"#
    );
    fs::write(&launcher_path, launcher_json5).expect("launcher config should be writable");

    let launch_result = StackCommand {
        command: StackCommands::Launch {
            launcher_config_path: launcher_path,
        },
    }
    .execute(&ctx);

    assert!(
        launch_result.is_err(),
        "launch command should fail because the launched node never becomes healthy"
    );

    let response = rt
        .block_on(NodeListRequest::new(false).poll(
            messenger_handle,
            &master_node_name,
            CALLER_INSTANCE_ID,
            &master_node_name,
            Duration::from_secs(5),
        ))
        .expect("node_list request should complete after launch");

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
