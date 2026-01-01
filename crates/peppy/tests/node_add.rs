mod helpers;

use std::sync::Arc;
use std::time::Duration;

use helpers::TestServeHandle;
use master_node::encoding::NodeListRequest;
use peppy::node::{NodeCommand, NodeCommands, NodeName};
use peppy::serve::DaemonState;
use peppy::{AppContext, Command};

const CALLER_INSTANCE_ID: &str = "peppy-test";

#[test]
fn node_add_command_succeeds() {
    let _serial_guard = helpers::serve_test_lock().lock().unwrap();
    let serve = TestServeHandle::new();

    let daemon_state = DaemonState::read().expect("daemon state should be readable");
    let master_node_name = daemon_state.master_node_name;
    assert!(
        !master_node_name.is_empty(),
        "master_node_name should not be empty"
    );

    // Create a temp directory for the node
    let node_dir = tempfile::tempdir().expect("failed to create temp dir for node");
    let node_name = "test_add_node";

    // Create AppContext pointing to the temp directory
    let node_ctx = Arc::new(AppContext::with_messenger(
        node_dir.path(),
        serve.messenger(),
    ));

    // Set up logging
    let log_capture = serve.log_capture().clone();
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .without_time()
        .with_writer(log_capture.clone())
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);

    // First, create a node using the init command
    NodeCommand {
        command: NodeCommands::Init {
            node_name: NodeName::new(node_name).expect("valid node name"),
            to_dir: None,
            build_system: config::peppy_config::BuildSystem::Rust,
        },
    }
    .execute(&node_ctx)
    .expect("node init command should succeed");

    // Get the path to the peppy.json5 file
    let peppy_json5_path = node_dir.path().join(node_name).join("peppy.json5");
    assert!(
        peppy_json5_path.exists(),
        "peppy.json5 should exist at {}",
        peppy_json5_path.display()
    );

    // Now add the node to the node stack
    NodeCommand {
        command: NodeCommands::Add {
            peppy_json5: peppy_json5_path,
            run: false,
            args: Vec::new(),
            instance_id: None,
        },
    }
    .execute(&node_ctx)
    .expect("node add command should succeed");

    // Verify the logs contain success message
    let logs = log_capture.logs();
    assert!(
        logs.contains(&format!("Added node {}:", node_name)),
        "logs should contain success message for adding node. Logs:\n{}",
        logs
    );

    // Query the node stack to verify the node was added
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime should create");
    let messenger_handle = node_ctx
        .messenger_handle()
        .expect("messenger handle should be available");

    let response = rt
        .block_on(NodeListRequest::new().poll(
            messenger_handle,
            &master_node_name,
            CALLER_INSTANCE_ID,
            &master_node_name,
            Duration::from_secs(5),
        ))
        .expect("node_list request should complete");

    // Verify the node is in the DOT graph
    assert!(
        response.dot_graph.contains(&format!("{}:0.1.0", node_name)),
        "dot_graph should contain the added node. Got:\n{}",
        response.dot_graph
    );

    // Verify the node has 0 instances (since run=false)
    assert!(
        response.dot_graph.contains("(0 instances)"),
        "dot_graph should show 0 instances for the added node. Got:\n{}",
        response.dot_graph
    );
}

#[test]
fn node_add_command_with_run_arg_succeeds() {
    let _serial_guard = helpers::serve_test_lock().lock().unwrap();
    // Use zenoh messaging so the spawned node process can communicate with the master node
    let serve = TestServeHandle::with_zenoh();

    let daemon_state = DaemonState::read().expect("daemon state should be readable");
    let master_node_name = daemon_state.master_node_name;
    assert!(
        !master_node_name.is_empty(),
        "master_node_name should not be empty"
    );

    // Create a temp directory for the node
    let node_dir = tempfile::tempdir().expect("failed to create temp dir for node");
    let node_name = "test_add_run_node";

    // Create AppContext pointing to the temp directory
    let node_ctx = Arc::new(AppContext::with_messenger(
        node_dir.path(),
        serve.messenger(),
    ));

    // Set up logging
    let log_capture = serve.log_capture().clone();
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .without_time()
        .with_writer(log_capture.clone())
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);

    // First, create a node using the init command
    NodeCommand {
        command: NodeCommands::Init {
            node_name: NodeName::new(node_name).expect("valid node name"),
            to_dir: None,
            build_system: config::peppy_config::BuildSystem::Rust,
        },
    }
    .execute(&node_ctx)
    .expect("node init command should succeed");

    // Get the path to the peppy.json5 file
    let node_path = node_dir.path().join(node_name);
    let peppy_json5_path = node_path.join("peppy.json5");
    assert!(
        peppy_json5_path.exists(),
        "peppy.json5 should exist at {}",
        peppy_json5_path.display()
    );

    // Build the node before running it
    let build_output = std::process::Command::new("cargo")
        .args(["build", "--release"])
        .current_dir(&node_path)
        .output()
        .expect("failed to run cargo build");

    assert!(
        build_output.status.success(),
        "cargo build should succeed. stderr: {}",
        String::from_utf8_lossy(&build_output.stderr)
    );

    // Add the node to the node stack with run=true to also start an instance
    NodeCommand {
        command: NodeCommands::Add {
            peppy_json5: peppy_json5_path,
            run: true,
            args: Vec::new(),
            instance_id: None,
        },
    }
    .execute(&node_ctx)
    .expect("node add command with run should succeed");

    // Verify the logs contain success messages for both add and start
    let logs = log_capture.logs();
    assert!(
        logs.contains(&format!("Added node {}:", node_name)),
        "logs should contain success message for adding node. Logs:\n{}",
        logs
    );
    assert!(
        logs.contains("Started node instance"),
        "logs should contain success message for starting node instance. Logs:\n{}",
        logs
    );

    // Query the node stack to verify the node was added with an instance
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime should create");
    let messenger_handle = node_ctx
        .messenger_handle()
        .expect("messenger handle should be available");

    let response = rt
        .block_on(NodeListRequest::new().poll(
            messenger_handle,
            &master_node_name,
            CALLER_INSTANCE_ID,
            &master_node_name,
            Duration::from_secs(5),
        ))
        .expect("node_list request should complete");

    // Verify the node is in the DOT graph
    assert!(
        response.dot_graph.contains(&format!("{}:0.1.0", node_name)),
        "dot_graph should contain the added node. Got:\n{}",
        response.dot_graph
    );

    // Verify the node has 1 instance (since run=true)
    assert!(
        response
            .dot_graph
            .contains(&format!("{}:0.1.0\\n(1 instance)", node_name)),
        "dot_graph should show 1 instance for the added node. Got:\n{}",
        response.dot_graph
    );
}
