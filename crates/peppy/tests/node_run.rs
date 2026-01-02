mod helpers;

use std::sync::Arc;
use std::time::Duration;

use helpers::TestServeHandle;
use master_node::encoding::NodeListRequest;
use node_stack::SerializedNodeGraph;
use peppy::commands::Command;
use peppy::commands::node::{NodeCommand, NodeCommands, NodeName};
use peppy::context::{AppContext, DaemonState};

const CALLER_INSTANCE_ID: &str = "peppy-test";

#[test]
fn node_run_command_succeeds() {
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
    let node_name = "test_run_node";

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

    // Add the node to the node stack (without running)
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

    // Verify the node was added with 0 instances
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime should create");
    let messenger_handle = node_ctx
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
            .any(|n| n.label().contains("(0 instances)")),
        "graph should show 0 instances before run. Got: {:?}",
        graph.nodes.iter().map(|n| n.label()).collect::<Vec<_>>()
    );

    // Now run the node using the run command
    NodeCommand {
        command: NodeCommands::Run {
            node_name: node_name.to_string(),
            tag: "0.1.0".to_string(),
            args: Vec::new(),
            instance_id: None,
        },
    }
    .execute(&node_ctx)
    .expect("node run command should succeed");

    // Verify the logs contain success message
    let logs = log_capture.logs();
    assert!(
        logs.contains("Started node instance"),
        "logs should contain success message for starting node instance. Logs:\n{}",
        logs
    );

    // Query the node stack to verify the node now has an instance
    let response = rt
        .block_on(NodeListRequest::new(false).poll(
            messenger_handle,
            &master_node_name,
            CALLER_INSTANCE_ID,
            &master_node_name,
            Duration::from_secs(5),
        ))
        .expect("node_list request should complete");

    // Verify the node has 1 instance now
    let graph: SerializedNodeGraph =
        serde_json::from_str(&response.graph_json).expect("graph_json should parse");
    assert!(
        graph
            .nodes
            .iter()
            .any(|n| n.label().contains("(1 instance)")),
        "graph should show 1 instance after run. Got: {:?}",
        graph.nodes.iter().map(|n| n.label()).collect::<Vec<_>>()
    );
}

#[test]
fn node_run_command_with_args_succeeds() {
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
    let node_name = "test_run_args_node";

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

    // Overwrite peppy.json5 with a config that includes parameters
    let peppy_config = r#"{
  schema_version: 1,
  manifest: {
    name: "test_run_args_node",
    tag: "0.1.0",
    launch_cmd: [
      "cargo",
      "run",
      "--release"
    ]
  },
  parameters: {
    resolution: "string",
    frequency: "i64",
    enabled: "bool"
  },
  interfaces: {
    exposes: {
      topics: [
        {
          name: "hello_world",
          qos_profile: "sensor_data",
          message_format: {
            timestamp: "time",
            message: "string"
          }
        }
      ],
    }
  },
  logging: {
    min_level: "info",
    format: "text"
  }
}
"#;
    std::fs::write(&peppy_json5_path, peppy_config).expect("peppy.json5 should be writable");

    // Update the fingerprint to match the new config
    let fingerprint =
        config::runtime::RuntimeConfig::generate_peppy_config_fingerprint(&peppy_json5_path)
            .expect("peppy.json5 fingerprint should generate");
    let fingerprint_path = node_path
        .join(config::consts::PEPPYGEN_OUTPUT_PATH)
        .join(config::consts::NODE_CONFIG_FINGERPRINT_FILE);
    std::fs::write(&fingerprint_path, fingerprint)
        .expect("peppygen fingerprint should be writable");

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

    // Add the node to the node stack (without running)
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

    // Verify the node was added with 0 instances before run
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime should create");
    let messenger_handle = node_ctx
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
        graph.nodes.iter().any(|n| {
            n.label().contains(&format!("{}:0.1.0", node_name))
                && n.label().contains("(0 instances)")
        }),
        "graph should show 0 instances before run. Got: {:?}",
        graph.nodes.iter().map(|n| n.label()).collect::<Vec<_>>()
    );

    // Now run the node with arguments
    let args = vec![
        ("resolution".to_string(), "1280x720".to_string()),
        ("frequency".to_string(), "30".to_string()),
        ("enabled".to_string(), "true".to_string()),
    ];

    NodeCommand {
        command: NodeCommands::Run {
            node_name: node_name.to_string(),
            tag: "0.1.0".to_string(),
            args,
            instance_id: None,
        },
    }
    .execute(&node_ctx)
    .expect("node run command with args should succeed");

    // Verify the logs contain success message with argument count
    let logs = log_capture.logs();
    assert!(
        logs.contains("3 argument(s)"),
        "logs should mention the number of arguments. Logs:\n{}",
        logs
    );
    assert!(
        logs.contains("Started node instance"),
        "logs should contain success message for starting node instance. Logs:\n{}",
        logs
    );

    // Query the node stack to verify the node now has an instance
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
        graph.nodes.iter().any(|n| {
            n.label().contains(&format!("{}:0.1.0", node_name))
                && n.label().contains("(1 instance)")
        }),
        "graph should show 1 instance after run. Got: {:?}",
        graph.nodes.iter().map(|n| n.label()).collect::<Vec<_>>()
    );
}

#[test]
fn node_run_command_with_custom_instance_id_succeeds() {
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
    let node_name = "test_run_instance_id_node";
    let custom_instance_id = "my-custom-instance";

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

    // Add the node to the node stack (without running)
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

    // Verify the node was added with 0 instances before run
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime should create");
    let messenger_handle = node_ctx
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
        graph.nodes.iter().any(|n| {
            n.label().contains(&format!("{}:0.1.0", node_name))
                && n.label().contains("(0 instances)")
        }),
        "graph should show 0 instances before run. Got: {:?}",
        graph.nodes.iter().map(|n| n.label()).collect::<Vec<_>>()
    );

    // Now run the node with a custom instance_id
    NodeCommand {
        command: NodeCommands::Run {
            node_name: node_name.to_string(),
            tag: "0.1.0".to_string(),
            args: Vec::new(),
            instance_id: Some(custom_instance_id.to_string()),
        },
    }
    .execute(&node_ctx)
    .expect("node run command with custom instance_id should succeed");

    // Verify the logs contain the custom instance_id
    let logs = log_capture.logs();
    assert!(
        logs.contains(custom_instance_id),
        "logs should contain the custom instance_id '{}'. Logs:\n{}",
        custom_instance_id,
        logs
    );
    assert!(
        logs.contains("Started node instance"),
        "logs should contain success message for starting node instance. Logs:\n{}",
        logs
    );

    // Query the node stack to verify the node now has an instance
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
        graph.nodes.iter().any(|n| {
            n.label().contains(&format!("{}:0.1.0", node_name))
                && n.label().contains("(1 instance)")
        }),
        "graph should show 1 instance after run. Got: {:?}",
        graph.nodes.iter().map(|n| n.label()).collect::<Vec<_>>()
    );
}
