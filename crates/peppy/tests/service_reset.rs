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
use peppy::commands::service::{ServiceCommand, ServiceCommands};
use peppy::context::{AppContext, DaemonState};

const CALLER_INSTANCE_ID: &str = "peppy-test";

fn write_node_config(
    nodes_directory: &Path,
    node_name: &str,
    node_tag: &str,
    launch_cmd: &[&str],
) -> PathBuf {
    let node_dir = nodes_directory.join(node_name);
    fs::create_dir_all(&node_dir).expect("failed to create node directory");
    let node_config_path = node_dir.join(NODE_CONFIG_FILE);
    let launch_cmd_json5 = launch_cmd
        .iter()
        .map(|arg| serde_json::to_string(arg).expect("launch_cmd arg should serialize"))
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
                    launch_cmd: [{launch_cmd_json5}]
                }}
            }}"#
        ),
    )
    .expect("failed to write node config");
    node_config_path
}

#[test]
fn service_reset_command_resets_node_stack() {
    let _serial_guard = helpers::serve_test_lock().lock().unwrap();
    let serve = TestServeHandle::with_mock_messenger();

    let daemon_state = DaemonState::read().expect("daemon state should be readable");
    let master_node_name = daemon_state.master_node_name;
    assert!(
        !master_node_name.is_empty(),
        "master_node_name should not be empty"
    );

    let nodes_dir = tempfile::tempdir().expect("failed to create temp nodes directory");
    let node_name = "reset_test_node";
    let node_tag = "0.1.0";
    let node_config_path = write_node_config(
        nodes_dir.path(),
        node_name,
        node_tag,
        &["sh", "-c", "exit 0"],
    );

    let ctx = Arc::new(AppContext::with_messenger(
        nodes_dir.path(),
        serve.messenger(),
    ));

    NodeCommand {
        command: NodeCommands::Add {
            peppy_json5: node_config_path,
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
            .any(|n| n.label().contains(&format!("{node_name}:{node_tag}"))),
        "graph should contain added node before reset. Got: {:?}",
        graph.nodes.iter().map(|n| n.label()).collect::<Vec<_>>()
    );

    ServiceCommand {
        command: ServiceCommands::Reset {},
    }
    .execute(&ctx)
    .expect("service reset command should succeed");

    let response = rt
        .block_on(NodeListRequest::new(false).poll(
            messenger_handle,
            &master_node_name,
            CALLER_INSTANCE_ID,
            &master_node_name,
            Duration::from_secs(5),
        ))
        .expect("node_list request should complete after reset");

    let graph: SerializedNodeGraph =
        serde_json::from_str(&response.graph_json).expect("graph_json should parse after reset");

    assert_eq!(
        graph.nodes.len(),
        1,
        "graph should contain only the root node after reset. Got: {:?}",
        graph.nodes.iter().map(|n| n.label()).collect::<Vec<_>>()
    );

    assert_eq!(
        graph.nodes[0].name,
        master_node_name,
        "root node name should match master node name. Got: {:?}",
        graph.nodes.iter().map(|n| n.label()).collect::<Vec<_>>()
    );
}
