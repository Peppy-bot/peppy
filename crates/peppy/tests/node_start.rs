use config::node::Toolchain;
use peppy::test_support::{LogCapture, ServeCommandEmulation};
use std::sync::Arc;
use std::time::Duration;

use core_node::encoding::NodeListRequest;
use node_stack::SerializedNodeGraph;
use peppy::commands::Command;
use peppy::commands::node::{NodeCommand, NodeCommands, NodeName};
use peppy::context::AppContext;
use peppylib::MessengerHandle;
use peppylib::services::health::listen_for_node_health;
use peppylib::services::ready::listen_for_node_ready;

const CALLER_INSTANCE_ID: &str = "peppy-test";

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn node_run_command_succeeds() {
    // Mock messaging is sufficient: we run in-process node services for health/ready.
    let serve = ServeCommandEmulation::with_mock()
        .await
        .expect("failed to create serve emulation");
    let shared_messenger = serve.messenger();
    let core_node_name = serve.core_node_name().to_string();
    assert!(
        !core_node_name.is_empty(),
        "core_node_name should not be empty"
    );

    // Create a temp directory for the node
    let node_dir = tempfile::tempdir().expect("failed to create temp dir for node");
    let node_name = "test_run_node";
    let instance_id = "test_run_instance";

    // Create AppContext pointing to the temp directory
    let node_ctx = Arc::new(
        AppContext::with_messenger(node_dir.path(), Arc::clone(&shared_messenger))
            .with_daemon_state_file(serve.daemon_state_path()),
    );

    // Set up logging
    let log_capture = LogCapture::new();
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
            toolchain: Toolchain::Cargo,
            with_container: false,
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

    peppy::test_support::override_start_cmd(&peppy_json5_path);

    // Add the node to the node stack (without running)
    NodeCommand {
        command: NodeCommands::Add {
            source: Some(node_path.display().to_string()),
            git_ref: None,
            variant: None,
            build: true,
            start: false,
            args: Vec::new(),
            instance_id: None,
            idle_timeout: 60,
            max_timeout: 3600,
            force: false,
        },
    }
    .execute(&node_ctx)
    .expect("node add command should succeed");

    // Verify the node was added with 0 instances
    let messenger_handle = node_ctx
        .messenger_handle()
        .expect("messenger handle should be available");

    let response = NodeListRequest::new(false)
        .poll(
            messenger_handle,
            &core_node_name,
            CALLER_INSTANCE_ID,
            &core_node_name,
            Duration::from_secs(5),
        )
        .await
        .expect("node_list request should complete");

    let graph: SerializedNodeGraph =
        serde_json::from_str(&response.graph_json).expect("graph_json should parse");
    let node = graph
        .nodes
        .iter()
        .find(|n| n.name == node_name && n.tag == "0.1.0")
        .unwrap_or_else(|| {
            panic!(
                "graph should contain the added node. Got: {:?}",
                graph.nodes.iter().map(|n| n.label()).collect::<Vec<_>>()
            )
        });
    assert_eq!(
        node.instance_count(),
        0,
        "graph should show 0 instances before run. Got: {:?}",
        graph
            .nodes
            .iter()
            .map(|n| (n.label(), n.instance_count()))
            .collect::<Vec<_>>()
    );

    // Start in-process node services for health/ready so node_start can succeed.
    let node_messenger = MessengerHandle::from_shared(Arc::clone(&shared_messenger));
    let _node_ready_handle =
        listen_for_node_ready(&node_messenger, &core_node_name, instance_id, node_name)
            .await
            .expect("node ready service should start");
    let _node_health_handle =
        listen_for_node_health(&node_messenger, &core_node_name, instance_id, node_name)
            .await
            .expect("node health service should start");

    // Now run the node using the run command
    NodeCommand {
        command: NodeCommands::Start {
            node_ref: None,
            node_name: Some(node_name.to_string()),
            tag: Some("0.1.0".to_string()),
            args: Vec::new(),
            instance_id: Some(instance_id.to_string()),
            idle_timeout: 60,
            max_timeout: 3600,
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
    let response = NodeListRequest::new(false)
        .poll(
            messenger_handle,
            &core_node_name,
            CALLER_INSTANCE_ID,
            &core_node_name,
            Duration::from_secs(5),
        )
        .await
        .expect("node_list request should complete");

    // Verify the node has 1 instance now
    let graph: SerializedNodeGraph =
        serde_json::from_str(&response.graph_json).expect("graph_json should parse");
    let node = graph
        .nodes
        .iter()
        .find(|n| n.name == node_name && n.tag == "0.1.0")
        .unwrap_or_else(|| {
            panic!(
                "graph should contain the added node. Got: {:?}",
                graph.nodes.iter().map(|n| n.label()).collect::<Vec<_>>()
            )
        });
    assert_eq!(
        node.instance_count(),
        1,
        "graph should show 1 instance after run. Got: {:?}",
        graph
            .nodes
            .iter()
            .map(|n| (n.label(), n.instance_count()))
            .collect::<Vec<_>>()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn node_run_command_with_args_succeeds() {
    // Mock messaging is sufficient: we run in-process node services for health/ready.
    let serve = ServeCommandEmulation::with_mock()
        .await
        .expect("failed to create serve emulation");
    let shared_messenger = serve.messenger();
    let core_node_name = serve.core_node_name().to_string();
    assert!(
        !core_node_name.is_empty(),
        "core_node_name should not be empty"
    );

    // Create a temp directory for the node
    let node_dir = tempfile::tempdir().expect("failed to create temp dir for node");
    let node_name = "test_run_args_node";
    let instance_id = "test_run_args_instance";

    // Create AppContext pointing to the temp directory
    let node_ctx = Arc::new(
        AppContext::with_messenger(node_dir.path(), Arc::clone(&shared_messenger))
            .with_daemon_state_file(serve.daemon_state_path()),
    );

    // Set up logging
    let log_capture = LogCapture::new();
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
            toolchain: Toolchain::Cargo,
            with_container: false,
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
  manifest: { name: "test_run_args_node",
    tag: "0.1.0" },
  interfaces: {
    topics: {
      emits: [
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
  execution: {
    language: "rust",
    parameters: {
      resolution: "string",
      frequency: "i64",
      enabled: "bool"
    },
    start_cmd: [
      "cargo",
      "run",
      "--release"
    ]
  },
}
"#;
    std::fs::write(&peppy_json5_path, peppy_config).expect("peppy.json5 should be writable");
    peppy::test_support::override_start_cmd(&peppy_json5_path);

    // Add the node to the node stack (without running)
    NodeCommand {
        command: NodeCommands::Add {
            source: Some(node_path.display().to_string()),
            git_ref: None,
            variant: None,
            build: true,
            start: false,
            args: Vec::new(),
            instance_id: None,
            idle_timeout: 60,
            max_timeout: 3600,
            force: false,
        },
    }
    .execute(&node_ctx)
    .expect("node add command should succeed");

    // Verify the node was added with 0 instances before run
    let messenger_handle = node_ctx
        .messenger_handle()
        .expect("messenger handle should be available");

    let response = NodeListRequest::new(false)
        .poll(
            messenger_handle,
            &core_node_name,
            CALLER_INSTANCE_ID,
            &core_node_name,
            Duration::from_secs(5),
        )
        .await
        .expect("node_list request should complete");

    let graph: SerializedNodeGraph =
        serde_json::from_str(&response.graph_json).expect("graph_json should parse");
    let node = graph
        .nodes
        .iter()
        .find(|n| n.name == node_name && n.tag == "0.1.0")
        .unwrap_or_else(|| {
            panic!(
                "graph should contain the added node. Got: {:?}",
                graph.nodes.iter().map(|n| n.label()).collect::<Vec<_>>()
            )
        });
    assert_eq!(
        node.instance_count(),
        0,
        "graph should show 0 instances before run. Got: {:?}",
        graph
            .nodes
            .iter()
            .map(|n| (n.label(), n.instance_count()))
            .collect::<Vec<_>>()
    );

    // Start in-process node services for health/ready so node_start can succeed.
    let node_messenger = MessengerHandle::from_shared(Arc::clone(&shared_messenger));
    let _node_ready_handle =
        listen_for_node_ready(&node_messenger, &core_node_name, instance_id, node_name)
            .await
            .expect("node ready service should start");
    let _node_health_handle =
        listen_for_node_health(&node_messenger, &core_node_name, instance_id, node_name)
            .await
            .expect("node health service should start");

    // Now run the node with arguments
    let args = vec![
        ("resolution".to_string(), "1280x720".to_string()),
        ("frequency".to_string(), "30".to_string()),
        ("enabled".to_string(), "true".to_string()),
    ];

    NodeCommand {
        command: NodeCommands::Start {
            node_ref: None,
            node_name: Some(node_name.to_string()),
            tag: Some("0.1.0".to_string()),
            args,
            instance_id: Some(instance_id.to_string()),
            idle_timeout: 60,
            max_timeout: 3600,
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
    let response = NodeListRequest::new(false)
        .poll(
            messenger_handle,
            &core_node_name,
            CALLER_INSTANCE_ID,
            &core_node_name,
            Duration::from_secs(5),
        )
        .await
        .expect("node_list request should complete");

    let graph: SerializedNodeGraph =
        serde_json::from_str(&response.graph_json).expect("graph_json should parse");
    let node = graph
        .nodes
        .iter()
        .find(|n| n.name == node_name && n.tag == "0.1.0")
        .unwrap_or_else(|| {
            panic!(
                "graph should contain the added node. Got: {:?}",
                graph.nodes.iter().map(|n| n.label()).collect::<Vec<_>>()
            )
        });
    assert_eq!(
        node.instance_count(),
        1,
        "graph should show 1 instance after run. Got: {:?}",
        graph
            .nodes
            .iter()
            .map(|n| (n.label(), n.instance_count()))
            .collect::<Vec<_>>()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn node_run_command_with_custom_instance_id_succeeds() {
    // Mock messaging is sufficient: we run in-process node services for health/ready.
    let serve = ServeCommandEmulation::with_mock()
        .await
        .expect("failed to create serve emulation");
    let shared_messenger = serve.messenger();
    let core_node_name = serve.core_node_name().to_string();
    assert!(
        !core_node_name.is_empty(),
        "core_node_name should not be empty"
    );

    // Create a temp directory for the node
    let node_dir = tempfile::tempdir().expect("failed to create temp dir for node");
    let node_name = "test_run_instance_id_node";
    let custom_instance_id = "my-custom-instance";

    // Create AppContext pointing to the temp directory
    let node_ctx = Arc::new(
        AppContext::with_messenger(node_dir.path(), Arc::clone(&shared_messenger))
            .with_daemon_state_file(serve.daemon_state_path()),
    );

    // Set up logging
    let log_capture = LogCapture::new();
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
            toolchain: Toolchain::Cargo,
            with_container: false,
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

    peppy::test_support::override_start_cmd(&peppy_json5_path);

    // Add the node to the node stack (without running)
    NodeCommand {
        command: NodeCommands::Add {
            source: Some(node_path.display().to_string()),
            git_ref: None,
            variant: None,
            build: true,
            start: false,
            args: Vec::new(),
            instance_id: None,
            idle_timeout: 60,
            max_timeout: 3600,
            force: false,
        },
    }
    .execute(&node_ctx)
    .expect("node add command should succeed");

    // Verify the node was added with 0 instances before run
    let messenger_handle = node_ctx
        .messenger_handle()
        .expect("messenger handle should be available");

    let response = NodeListRequest::new(false)
        .poll(
            messenger_handle,
            &core_node_name,
            CALLER_INSTANCE_ID,
            &core_node_name,
            Duration::from_secs(5),
        )
        .await
        .expect("node_list request should complete");

    let graph: SerializedNodeGraph =
        serde_json::from_str(&response.graph_json).expect("graph_json should parse");
    let node = graph
        .nodes
        .iter()
        .find(|n| n.name == node_name && n.tag == "0.1.0")
        .unwrap_or_else(|| {
            panic!(
                "graph should contain the added node. Got: {:?}",
                graph.nodes.iter().map(|n| n.label()).collect::<Vec<_>>()
            )
        });
    assert_eq!(
        node.instance_count(),
        0,
        "graph should show 0 instances before run. Got: {:?}",
        graph
            .nodes
            .iter()
            .map(|n| (n.label(), n.instance_count()))
            .collect::<Vec<_>>()
    );

    // Start in-process node services for health/ready so node_start can succeed.
    let node_messenger = MessengerHandle::from_shared(Arc::clone(&shared_messenger));
    let _node_ready_handle = listen_for_node_ready(
        &node_messenger,
        &core_node_name,
        custom_instance_id,
        node_name,
    )
    .await
    .expect("node ready service should start");
    let _node_health_handle = listen_for_node_health(
        &node_messenger,
        &core_node_name,
        custom_instance_id,
        node_name,
    )
    .await
    .expect("node health service should start");

    // Now run the node with a custom instance_id
    NodeCommand {
        command: NodeCommands::Start {
            node_ref: None,
            node_name: Some(node_name.to_string()),
            tag: Some("0.1.0".to_string()),
            args: Vec::new(),
            instance_id: Some(custom_instance_id.to_string()),
            idle_timeout: 60,
            max_timeout: 3600,
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
    let response = NodeListRequest::new(false)
        .poll(
            messenger_handle,
            &core_node_name,
            CALLER_INSTANCE_ID,
            &core_node_name,
            Duration::from_secs(5),
        )
        .await
        .expect("node_list request should complete");

    let graph: SerializedNodeGraph =
        serde_json::from_str(&response.graph_json).expect("graph_json should parse");
    let node = graph
        .nodes
        .iter()
        .find(|n| n.name == node_name && n.tag == "0.1.0")
        .unwrap_or_else(|| {
            panic!(
                "graph should contain the added node. Got: {:?}",
                graph.nodes.iter().map(|n| n.label()).collect::<Vec<_>>()
            )
        });
    assert_eq!(
        node.instance_count(),
        1,
        "graph should show 1 instance after run. Got: {:?}",
        graph
            .nodes
            .iter()
            .map(|n| (n.label(), n.instance_count()))
            .collect::<Vec<_>>()
    );
}
