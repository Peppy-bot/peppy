use config::node::Toolchain;
use peppy::test_support::{LogCapture, ServeCommandEmulation};
use std::sync::Arc;
use std::time::Duration;

use core_node_api::encoding::{NodeInfoRequest, NodeInfoResponse, StackListRequest};
use core_node_api::{NodeStage, SerializedNodeGraph};
use peppy::commands::Command;
use peppy::commands::node::{NodeCommand, NodeCommands, NodeName};
use peppy::context::AppContext;
use peppylib::MessengerHandle;
use peppylib::services::health::listen_for_node_health;
use peppylib::services::ready::listen_for_node_ready;

use peppylib::core_node::transport::{poll_node_info, poll_stack_list};

use super::common::test_node_target;
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

    peppy::test_support::override_run_cmd(&peppy_json5_path);

    // Add the node to the node stack (without running)
    NodeCommand {
        command: NodeCommands::Add {
            source: Some(node_path.display().to_string()),
            git_ref: None,
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
    .execute(&node_ctx)
    .expect("node add command should succeed");

    // Verify the node was added with 0 instances
    let messenger_handle = node_ctx
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
    let node = graph
        .nodes
        .iter()
        .find(|n| n.name == node_name && n.tag == "v1")
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

    // Start in-process node services for health/ready so node_run can succeed.
    let node_messenger = MessengerHandle::from_shared(Arc::clone(&shared_messenger));
    let _node_ready_handle = listen_for_node_ready(
        &node_messenger,
        &core_node_name,
        instance_id,
        test_node_target(node_name),
        &[],
    )
    .await
    .expect("node ready service should start");
    let _node_health_handle = listen_for_node_health(
        &node_messenger,
        &core_node_name,
        instance_id,
        test_node_target(node_name),
        &[],
    )
    .await
    .expect("node health service should start");

    // Now run the node using the run command
    NodeCommand {
        command: NodeCommands::Run {
            node_ref: None,
            node_name: Some(node_name.to_string()),
            tag: Some("v1".to_string()),
            args: Vec::new(),
            instance_id: Some(instance_id.to_string()),
            link_ids: Vec::new(),
            idle_timeout: 60,
            max_timeout: 3600,
            build: false,
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

    // Verify the node has 1 instance now
    let graph: SerializedNodeGraph =
        serde_json::from_str(&response.graph_json).expect("graph_json should parse");
    let node = graph
        .nodes
        .iter()
        .find(|n| n.name == node_name && n.tag == "v1")
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
  peppy_schema: "node_v1",
  manifest: { name: "test_run_args_node",
    tag: "v1" },
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
    run_cmd: [
      "cargo",
      "run",
      "--release"
    ]
  },
}
"#;
    std::fs::write(&peppy_json5_path, peppy_config).expect("peppy.json5 should be writable");
    peppy::test_support::override_run_cmd(&peppy_json5_path);

    // Add the node to the node stack (without running)
    NodeCommand {
        command: NodeCommands::Add {
            source: Some(node_path.display().to_string()),
            git_ref: None,
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
    .execute(&node_ctx)
    .expect("node add command should succeed");

    // Verify the node was added with 0 instances before run
    let messenger_handle = node_ctx
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
    let node = graph
        .nodes
        .iter()
        .find(|n| n.name == node_name && n.tag == "v1")
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

    // Start in-process node services for health/ready so node_run can succeed.
    let node_messenger = MessengerHandle::from_shared(Arc::clone(&shared_messenger));
    let _node_ready_handle = listen_for_node_ready(
        &node_messenger,
        &core_node_name,
        instance_id,
        test_node_target(node_name),
        &[],
    )
    .await
    .expect("node ready service should start");
    let _node_health_handle = listen_for_node_health(
        &node_messenger,
        &core_node_name,
        instance_id,
        test_node_target(node_name),
        &[],
    )
    .await
    .expect("node health service should start");

    // Now run the node with arguments
    let args = vec![
        ("resolution".to_string(), "1280x720".to_string()),
        ("frequency".to_string(), "30".to_string()),
        ("enabled".to_string(), "true".to_string()),
    ];

    NodeCommand {
        command: NodeCommands::Run {
            node_ref: None,
            node_name: Some(node_name.to_string()),
            tag: Some("v1".to_string()),
            args,
            instance_id: Some(instance_id.to_string()),
            link_ids: Vec::new(),
            idle_timeout: 60,
            max_timeout: 3600,
            build: false,
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
    let node = graph
        .nodes
        .iter()
        .find(|n| n.name == node_name && n.tag == "v1")
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

    peppy::test_support::override_run_cmd(&peppy_json5_path);

    // Add the node to the node stack (without running)
    NodeCommand {
        command: NodeCommands::Add {
            source: Some(node_path.display().to_string()),
            git_ref: None,
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
    .execute(&node_ctx)
    .expect("node add command should succeed");

    // Verify the node was added with 0 instances before run
    let messenger_handle = node_ctx
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
    let node = graph
        .nodes
        .iter()
        .find(|n| n.name == node_name && n.tag == "v1")
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

    // Start in-process node services for health/ready so node_run can succeed.
    let node_messenger = MessengerHandle::from_shared(Arc::clone(&shared_messenger));
    let _node_ready_handle = listen_for_node_ready(
        &node_messenger,
        &core_node_name,
        custom_instance_id,
        test_node_target(node_name),
        &[],
    )
    .await
    .expect("node ready service should start");
    let _node_health_handle = listen_for_node_health(
        &node_messenger,
        &core_node_name,
        custom_instance_id,
        test_node_target(node_name),
        &[],
    )
    .await
    .expect("node health service should start");

    // Now run the node with a custom instance_id
    NodeCommand {
        command: NodeCommands::Run {
            node_ref: None,
            node_name: Some(node_name.to_string()),
            tag: Some("v1".to_string()),
            args: Vec::new(),
            instance_id: Some(custom_instance_id.to_string()),
            link_ids: Vec::new(),
            idle_timeout: 60,
            max_timeout: 3600,
            build: false,
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
    let node = graph
        .nodes
        .iter()
        .find(|n| n.name == node_name && n.tag == "v1")
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
async fn node_run_with_build_flag_on_unbuilt_node_builds_then_runs() {
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

    let node_dir = tempfile::tempdir().expect("failed to create temp dir for node");
    let node_name = "test_run_b_unbuilt_node";
    let instance_id = "test_run_b_unbuilt_instance";

    let node_ctx = Arc::new(
        AppContext::with_messenger(node_dir.path(), Arc::clone(&shared_messenger))
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
        command: NodeCommands::Init {
            node_name: NodeName::new(node_name).expect("valid node name"),
            to_dir: None,
            toolchain: Toolchain::Cargo,
            with_container: false,
        },
    }
    .execute(&node_ctx)
    .expect("node init command should succeed");

    let node_path = node_dir.path().join(node_name);
    let peppy_json5_path = node_path.join("peppy.json5");
    assert!(peppy_json5_path.exists());
    peppy::test_support::override_run_cmd(&peppy_json5_path);

    // Add the node WITHOUT building it. The node should land in the `Added`
    // stage so that `node run -b` has something to build.
    NodeCommand {
        command: NodeCommands::Add {
            source: Some(node_path.display().to_string()),
            git_ref: None,
            sync: false,
            build: false,
            run: false,
            args: Vec::new(),
            instance_id: None,
            idle_timeout: 60,
            max_timeout: 3600,
            force: false,
        },
    }
    .execute(&node_ctx)
    .expect("node add command should succeed");

    let messenger_handle = node_ctx
        .messenger_handle()
        .expect("messenger handle should be available");

    // Pre-state: the node is in the stack and is in the `Added` stage
    // (not yet built).
    let info_response = poll_node_info(
        &NodeInfoRequest::new(node_name, "v1"),
        messenger_handle,
        &core_node_name,
        CALLER_INSTANCE_ID,
        &core_node_name,
        Duration::from_secs(5),
    )
    .await
    .expect("node_info request should complete");
    match info_response {
        NodeInfoResponse::Found(info) => {
            assert_eq!(
                info.stage,
                NodeStage::Added,
                "node should be in Added stage before `run -b`, got {:?}",
                info.stage
            );
        }
        NodeInfoResponse::NotInStack => {
            panic!("node should be in the stack before `run -b`")
        }
    }

    // Start in-process node services for health/ready so node_run can succeed.
    let node_messenger = MessengerHandle::from_shared(Arc::clone(&shared_messenger));
    let _node_ready_handle = listen_for_node_ready(
        &node_messenger,
        &core_node_name,
        instance_id,
        test_node_target(node_name),
        &[],
    )
    .await
    .expect("node ready service should start");
    let _node_health_handle = listen_for_node_health(
        &node_messenger,
        &core_node_name,
        instance_id,
        test_node_target(node_name),
        &[],
    )
    .await
    .expect("node health service should start");

    // Run with `-b` set: should build first, then start the instance.
    NodeCommand {
        command: NodeCommands::Run {
            node_ref: None,
            node_name: Some(node_name.to_string()),
            tag: Some("v1".to_string()),
            args: Vec::new(),
            instance_id: Some(instance_id.to_string()),
            link_ids: Vec::new(),
            idle_timeout: 60,
            max_timeout: 3600,
            build: true,
        },
    }
    .execute(&node_ctx)
    .expect("node run -b should succeed on an unbuilt node");

    let logs = log_capture.logs();
    assert!(
        logs.contains("Building node"),
        "logs should mention that the node is being built. Logs:\n{}",
        logs
    );
    assert!(
        !logs.contains("has already been built"),
        "logs should NOT contain the already-built skip message. Logs:\n{}",
        logs
    );
    assert!(
        logs.contains("Started node instance"),
        "logs should mention that the instance was started. Logs:\n{}",
        logs
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
    .expect("stack_list request should complete");
    let graph: SerializedNodeGraph =
        serde_json::from_str(&response.graph_json).expect("graph_json should parse");
    let node = graph
        .nodes
        .iter()
        .find(|n| n.name == node_name && n.tag == "v1")
        .expect("graph should contain the added node");
    assert_eq!(
        node.instance_count(),
        1,
        "graph should show 1 instance after `run -b`"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn node_run_with_build_flag_on_already_built_node_skips_build() {
    let serve = ServeCommandEmulation::with_mock()
        .await
        .expect("failed to create serve emulation");
    let shared_messenger = serve.messenger();
    let core_node_name = serve.core_node_name().to_string();
    assert!(
        !core_node_name.is_empty(),
        "core_node_name should not be empty"
    );

    let node_dir = tempfile::tempdir().expect("failed to create temp dir for node");
    let node_name = "test_run_b_built_node";
    let instance_id = "test_run_b_built_instance";

    let node_ctx = Arc::new(
        AppContext::with_messenger(node_dir.path(), Arc::clone(&shared_messenger))
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
        command: NodeCommands::Init {
            node_name: NodeName::new(node_name).expect("valid node name"),
            to_dir: None,
            toolchain: Toolchain::Cargo,
            with_container: false,
        },
    }
    .execute(&node_ctx)
    .expect("node init command should succeed");

    let node_path = node_dir.path().join(node_name);
    let peppy_json5_path = node_path.join("peppy.json5");
    assert!(peppy_json5_path.exists());
    peppy::test_support::override_run_cmd(&peppy_json5_path);

    // Add the node WITH building it. The node should land in the `Ready`
    // stage so that `node run -b` finds it already built.
    NodeCommand {
        command: NodeCommands::Add {
            source: Some(node_path.display().to_string()),
            git_ref: None,
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
    .execute(&node_ctx)
    .expect("node add command should succeed");

    let messenger_handle = node_ctx
        .messenger_handle()
        .expect("messenger handle should be available");

    // Sanity check: node is in `Ready` stage before we run `-b`.
    let info_response = poll_node_info(
        &NodeInfoRequest::new(node_name, "v1"),
        messenger_handle,
        &core_node_name,
        CALLER_INSTANCE_ID,
        &core_node_name,
        Duration::from_secs(5),
    )
    .await
    .expect("node_info request should complete");
    match info_response {
        NodeInfoResponse::Found(info) => {
            assert_eq!(
                info.stage,
                NodeStage::Ready,
                "node should be in Ready stage before `run -b`, got {:?}",
                info.stage
            );
        }
        NodeInfoResponse::NotInStack => {
            panic!("node should be in the stack before `run -b`")
        }
    }

    let node_messenger = MessengerHandle::from_shared(Arc::clone(&shared_messenger));
    let _node_ready_handle = listen_for_node_ready(
        &node_messenger,
        &core_node_name,
        instance_id,
        test_node_target(node_name),
        &[],
    )
    .await
    .expect("node ready service should start");
    let _node_health_handle = listen_for_node_health(
        &node_messenger,
        &core_node_name,
        instance_id,
        test_node_target(node_name),
        &[],
    )
    .await
    .expect("node health service should start");

    // Run with `-b` set: should detect the node is already built, skip the
    // build, and run.
    NodeCommand {
        command: NodeCommands::Run {
            node_ref: None,
            node_name: Some(node_name.to_string()),
            tag: Some("v1".to_string()),
            args: Vec::new(),
            instance_id: Some(instance_id.to_string()),
            link_ids: Vec::new(),
            idle_timeout: 60,
            max_timeout: 3600,
            build: true,
        },
    }
    .execute(&node_ctx)
    .expect("node run -b should succeed on an already-built node");

    let logs = log_capture.logs();
    assert!(
        logs.contains("has already been built"),
        "logs should contain the already-built skip message. Logs:\n{}",
        logs
    );
    assert!(
        logs.contains("Started node instance"),
        "logs should mention that the instance was started. Logs:\n{}",
        logs
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
    .expect("stack_list request should complete");
    let graph: SerializedNodeGraph =
        serde_json::from_str(&response.graph_json).expect("graph_json should parse");
    let node = graph
        .nodes
        .iter()
        .find(|n| n.name == node_name && n.tag == "v1")
        .expect("graph should contain the added node");
    assert_eq!(
        node.instance_count(),
        1,
        "graph should show 1 instance after `run -b`"
    );
}

/// Hand-crafts a peppy.json5 declaring `depends_on.nodes` against
/// `(producer_name, "v1")` with one entry per supplied `link_id`. The
/// consumer carries no interfaces and a no-op `run_cmd`, so the
/// daemon's dependency-spec validator is happy with the dep being
/// declared but never actually consumed. Returns the path to the
/// consumer directory ready to feed into `NodeCommands::Add`.
fn write_consumer_with_depends_on(
    work_dir: &std::path::Path,
    consumer_name: &str,
    producer_name: &str,
    link_ids: &[&str],
) -> std::path::PathBuf {
    let consumer_dir = work_dir.join(consumer_name);
    std::fs::create_dir_all(&consumer_dir).expect("create consumer dir");
    let entries = link_ids
        .iter()
        .map(|lid| {
            format!(r#"            {{ name: "{producer_name}", tag: "v1", link_id: "{lid}" }}"#)
        })
        .collect::<Vec<_>>()
        .join(",\n");
    let body = format!(
        r#"{{
    peppy_schema: "node_v1",
    manifest: {{
        name: "{consumer_name}",
        tag: "v1",
        depends_on: {{
            nodes: [
{entries}
            ]
        }}
    }},
    execution: {{
        language: "rust",
        run_cmd: ["sleep", "30"]
    }}
}}
"#
    );
    std::fs::write(consumer_dir.join("peppy.json5"), body).expect("write consumer peppy.json5");
    consumer_dir
}

/// Adds a scaffolded producer node to the stack and builds it.
/// Mirrors the boilerplate that `node_run_command_succeeds` runs
/// through; tests below treat it as the "set up a runnable target"
/// step.
async fn add_built_producer(
    node_ctx: &Arc<AppContext>,
    work_dir: &std::path::Path,
    producer_name: &str,
) {
    NodeCommand {
        command: NodeCommands::Init {
            node_name: NodeName::new(producer_name).expect("valid node name"),
            to_dir: None,
            toolchain: Toolchain::Cargo,
            with_container: false,
        },
    }
    .execute(node_ctx)
    .expect("producer node init should succeed");
    let producer_path = work_dir.join(producer_name);
    let producer_json5 = producer_path.join("peppy.json5");
    peppy::test_support::override_run_cmd(&producer_json5);
    NodeCommand {
        command: NodeCommands::Add {
            source: Some(producer_path.display().to_string()),
            git_ref: None,
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
    .execute(node_ctx)
    .expect("producer node add should succeed");
}

async fn add_consumer_with_pins(
    node_ctx: &Arc<AppContext>,
    work_dir: &std::path::Path,
    consumer_name: &str,
    producer_name: &str,
    link_ids: &[&str],
) {
    let consumer_dir =
        write_consumer_with_depends_on(work_dir, consumer_name, producer_name, link_ids);
    NodeCommand {
        command: NodeCommands::Add {
            source: Some(consumer_dir.display().to_string()),
            git_ref: None,
            sync: false,
            build: false,
            run: false,
            args: Vec::new(),
            instance_id: None,
            idle_timeout: 60,
            max_timeout: 3600,
            force: false,
        },
    }
    .execute(node_ctx)
    .expect("consumer node add should succeed");
}

/// Installs in-process node_ready and node_health services for the
/// given producer + instance_id so the daemon's start handshake can
/// complete. Returns the join handles so callers can hold them past
/// the `node run` call. The two services must outlive the run.
async fn install_node_services(
    node_messenger: &MessengerHandle,
    core_node_name: &str,
    producer_name: &str,
    instance_id: &str,
) -> (
    impl std::any::Any + Send + Sync,
    impl std::any::Any + Send + Sync,
) {
    let ready = listen_for_node_ready(
        node_messenger,
        core_node_name,
        instance_id,
        test_node_target(producer_name),
        &[],
    )
    .await
    .expect("node ready service should start");
    let health = listen_for_node_health(
        node_messenger,
        core_node_name,
        instance_id,
        test_node_target(producer_name),
        &[],
    )
    .await
    .expect("node health service should start");
    (ready, health)
}

/// Test A — running a producer with `--link-id main` when a stack
/// consumer pins it with `front_left` and `front_right` emits a
/// warning listing both link_ids and the consumer that asked.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn node_run_warns_when_stack_consumers_have_unsatisfied_link_ids() {
    let serve = ServeCommandEmulation::with_mock()
        .await
        .expect("failed to create serve emulation");
    let shared_messenger = serve.messenger();
    let core_node_name = serve.core_node_name().to_string();

    let work_dir = tempfile::tempdir().expect("failed to create work dir");
    let producer_name = "test_warn_producer";
    let consumer_name = "test_warn_consumer";
    let instance_id = "main_instance";

    let node_ctx = Arc::new(
        AppContext::with_messenger(work_dir.path(), Arc::clone(&shared_messenger))
            .with_daemon_state_file(serve.daemon_state_path()),
    );

    let log_capture = LogCapture::new();
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .without_time()
        .with_writer(log_capture.clone())
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);

    add_built_producer(&node_ctx, work_dir.path(), producer_name).await;
    add_consumer_with_pins(
        &node_ctx,
        work_dir.path(),
        consumer_name,
        producer_name,
        &["front_left", "front_right"],
    )
    .await;

    let node_messenger = MessengerHandle::from_shared(Arc::clone(&shared_messenger));
    let _services =
        install_node_services(&node_messenger, &core_node_name, producer_name, instance_id).await;

    NodeCommand {
        command: NodeCommands::Run {
            node_ref: None,
            node_name: Some(producer_name.to_string()),
            tag: Some("v1".to_string()),
            args: Vec::new(),
            instance_id: Some(instance_id.to_string()),
            link_ids: vec!["main".to_string()],
            idle_timeout: 60,
            max_timeout: 3600,
            build: false,
        },
    }
    .execute(&node_ctx)
    .expect("node run should succeed despite the warning");

    let logs = log_capture.logs();
    assert!(
        logs.contains("stack consumers that expect link_ids no instance is publishing"),
        "warning preamble should be present. Logs:\n{logs}"
    );
    assert!(
        logs.contains("front_left"),
        "warning should name missing link_id 'front_left'. Logs:\n{logs}"
    );
    assert!(
        logs.contains("front_right"),
        "warning should name missing link_id 'front_right'. Logs:\n{logs}"
    );
    assert!(
        logs.contains(&format!("{consumer_name}:v1")),
        "warning should name the consumer ({consumer_name}:v1). Logs:\n{logs}"
    );
    assert!(
        logs.contains("This instance will publish under: [main]"),
        "warning should report the new instance's link_ids. Logs:\n{logs}"
    );
    assert!(
        logs.contains("Started node instance"),
        "run should still complete successfully. Logs:\n{logs}"
    );
}

/// Test B — a second launch sees the first instance's `link_ids` as
/// already-running and only warns about pins that are still
/// uncovered.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn node_run_warning_accounts_for_existing_running_instances() {
    let serve = ServeCommandEmulation::with_mock()
        .await
        .expect("failed to create serve emulation");
    let shared_messenger = serve.messenger();
    let core_node_name = serve.core_node_name().to_string();

    let work_dir = tempfile::tempdir().expect("failed to create work dir");
    let producer_name = "test_warn_shrink_producer";
    let consumer_name = "test_warn_shrink_consumer";
    let first_instance = "inst_left";
    let second_instance = "inst_main";

    let node_ctx = Arc::new(
        AppContext::with_messenger(work_dir.path(), Arc::clone(&shared_messenger))
            .with_daemon_state_file(serve.daemon_state_path()),
    );

    let log_capture = LogCapture::new();
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .without_time()
        .with_writer(log_capture.clone())
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);

    add_built_producer(&node_ctx, work_dir.path(), producer_name).await;
    add_consumer_with_pins(
        &node_ctx,
        work_dir.path(),
        consumer_name,
        producer_name,
        &["front_left", "front_right"],
    )
    .await;

    // First instance — supplies front_left only.
    let node_messenger = MessengerHandle::from_shared(Arc::clone(&shared_messenger));
    let _services_first = install_node_services(
        &node_messenger,
        &core_node_name,
        producer_name,
        first_instance,
    )
    .await;
    let marker_before_first = log_capture.logs().len();
    NodeCommand {
        command: NodeCommands::Run {
            node_ref: None,
            node_name: Some(producer_name.to_string()),
            tag: Some("v1".to_string()),
            args: Vec::new(),
            instance_id: Some(first_instance.to_string()),
            link_ids: vec!["front_left".to_string()],
            idle_timeout: 60,
            max_timeout: 3600,
            build: false,
        },
    }
    .execute(&node_ctx)
    .expect("first node run should succeed");

    let first_logs = log_capture.logs()[marker_before_first..].to_string();
    assert!(
        first_logs.contains("front_right"),
        "first run should warn about front_right. New logs:\n{first_logs}"
    );
    assert!(
        !first_logs.contains("link_id `front_left`"),
        "first run should NOT list front_left as missing — this instance covers it. New logs:\n{first_logs}"
    );

    // Second instance — supplies an unrelated link_id `main`, so
    // front_right is still uncovered. front_left should now be
    // accounted for by the *existing* running instance.
    let _services_second = install_node_services(
        &node_messenger,
        &core_node_name,
        producer_name,
        second_instance,
    )
    .await;
    let marker_before_second = log_capture.logs().len();
    NodeCommand {
        command: NodeCommands::Run {
            node_ref: None,
            node_name: Some(producer_name.to_string()),
            tag: Some("v1".to_string()),
            args: Vec::new(),
            instance_id: Some(second_instance.to_string()),
            link_ids: vec!["main".to_string()],
            idle_timeout: 60,
            max_timeout: 3600,
            build: false,
        },
    }
    .execute(&node_ctx)
    .expect("second node run should succeed");

    let second_logs = log_capture.logs()[marker_before_second..].to_string();
    assert!(
        second_logs.contains("front_right"),
        "second run should still warn about front_right. New logs:\n{second_logs}"
    );
    assert!(
        !second_logs.contains("link_id `front_left`"),
        "second run should NOT list front_left as missing — the first instance covers it. New logs:\n{second_logs}"
    );
    assert!(
        second_logs.contains("Existing running instances publish under:"),
        "second run should advertise the existing instance's link_ids. New logs:\n{second_logs}"
    );
    assert!(
        second_logs.contains("front_left"),
        "second run should report front_left under the 'existing' line. New logs:\n{second_logs}"
    );
}

/// Test C — when the new instance fully covers every consumer-pin,
/// no warning is emitted.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn node_run_emits_no_warning_when_all_link_ids_are_covered() {
    let serve = ServeCommandEmulation::with_mock()
        .await
        .expect("failed to create serve emulation");
    let shared_messenger = serve.messenger();
    let core_node_name = serve.core_node_name().to_string();

    let work_dir = tempfile::tempdir().expect("failed to create work dir");
    let producer_name = "test_warn_covered_producer";
    let consumer_name = "test_warn_covered_consumer";
    let instance_id = "covered_instance";

    let node_ctx = Arc::new(
        AppContext::with_messenger(work_dir.path(), Arc::clone(&shared_messenger))
            .with_daemon_state_file(serve.daemon_state_path()),
    );

    let log_capture = LogCapture::new();
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .without_time()
        .with_writer(log_capture.clone())
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);

    add_built_producer(&node_ctx, work_dir.path(), producer_name).await;
    add_consumer_with_pins(
        &node_ctx,
        work_dir.path(),
        consumer_name,
        producer_name,
        &["front_left", "front_right"],
    )
    .await;

    let node_messenger = MessengerHandle::from_shared(Arc::clone(&shared_messenger));
    let _services =
        install_node_services(&node_messenger, &core_node_name, producer_name, instance_id).await;

    NodeCommand {
        command: NodeCommands::Run {
            node_ref: None,
            node_name: Some(producer_name.to_string()),
            tag: Some("v1".to_string()),
            args: Vec::new(),
            instance_id: Some(instance_id.to_string()),
            link_ids: vec!["front_left".to_string(), "front_right".to_string()],
            idle_timeout: 60,
            max_timeout: 3600,
            build: false,
        },
    }
    .execute(&node_ctx)
    .expect("node run should succeed");

    let logs = log_capture.logs();
    assert!(
        !logs.contains("stack consumers that expect link_ids no instance is publishing"),
        "no warning should fire when every consumer pin is covered. Logs:\n{logs}"
    );
    assert!(
        logs.contains("Started node instance"),
        "run should complete successfully. Logs:\n{logs}"
    );
}

/// Test D — when no consumer in the stack pins the target node, no
/// warning fires.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn node_run_emits_no_warning_when_stack_has_no_consumer_pin() {
    let serve = ServeCommandEmulation::with_mock()
        .await
        .expect("failed to create serve emulation");
    let shared_messenger = serve.messenger();
    let core_node_name = serve.core_node_name().to_string();

    let work_dir = tempfile::tempdir().expect("failed to create work dir");
    let producer_name = "test_warn_no_consumer_producer";
    let instance_id = "no_consumer_instance";

    let node_ctx = Arc::new(
        AppContext::with_messenger(work_dir.path(), Arc::clone(&shared_messenger))
            .with_daemon_state_file(serve.daemon_state_path()),
    );

    let log_capture = LogCapture::new();
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .without_time()
        .with_writer(log_capture.clone())
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);

    add_built_producer(&node_ctx, work_dir.path(), producer_name).await;
    // Intentionally no consumer in the stack.

    let node_messenger = MessengerHandle::from_shared(Arc::clone(&shared_messenger));
    let _services =
        install_node_services(&node_messenger, &core_node_name, producer_name, instance_id).await;

    NodeCommand {
        command: NodeCommands::Run {
            node_ref: None,
            node_name: Some(producer_name.to_string()),
            tag: Some("v1".to_string()),
            args: Vec::new(),
            instance_id: Some(instance_id.to_string()),
            link_ids: vec!["any".to_string()],
            idle_timeout: 60,
            max_timeout: 3600,
            build: false,
        },
    }
    .execute(&node_ctx)
    .expect("node run should succeed");

    let logs = log_capture.logs();
    assert!(
        !logs.contains("stack consumers that expect link_ids no instance is publishing"),
        "no warning should fire when no consumer pins us. Logs:\n{logs}"
    );
    assert!(
        logs.contains("Started node instance"),
        "run should complete successfully. Logs:\n{logs}"
    );
}
