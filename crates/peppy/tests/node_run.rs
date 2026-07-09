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

use peppylib::core_node::transport::poll;

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
            binds: Vec::new(),
            pairs: Vec::new(),
            defer_pairs: Vec::new(),
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
    let node = graph.find_node(node_name, "v1").unwrap_or_else(|| {
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
    )
    .await
    .expect("node ready service should start");
    let _node_health_handle = listen_for_node_health(
        &node_messenger,
        &core_node_name,
        instance_id,
        test_node_target(node_name),
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
            binds: Vec::new(),
            pairs: Vec::new(),
            defer_pairs: Vec::new(),

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

    // Verify the node has 1 instance now
    let graph: SerializedNodeGraph =
        serde_json::from_str(&response.graph_json).expect("graph_json should parse");
    let node = graph.find_node(node_name, "v1").unwrap_or_else(|| {
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
  peppy_schema: "node/v1",
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
            binds: Vec::new(),
            pairs: Vec::new(),
            defer_pairs: Vec::new(),
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
    let node = graph.find_node(node_name, "v1").unwrap_or_else(|| {
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
    )
    .await
    .expect("node ready service should start");
    let _node_health_handle = listen_for_node_health(
        &node_messenger,
        &core_node_name,
        instance_id,
        test_node_target(node_name),
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
            binds: Vec::new(),
            pairs: Vec::new(),
            defer_pairs: Vec::new(),

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
    let node = graph.find_node(node_name, "v1").unwrap_or_else(|| {
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
            binds: Vec::new(),
            pairs: Vec::new(),
            defer_pairs: Vec::new(),
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
    let node = graph.find_node(node_name, "v1").unwrap_or_else(|| {
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
    )
    .await
    .expect("node ready service should start");
    let _node_health_handle = listen_for_node_health(
        &node_messenger,
        &core_node_name,
        custom_instance_id,
        test_node_target(node_name),
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
            binds: Vec::new(),
            pairs: Vec::new(),
            defer_pairs: Vec::new(),

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
    let node = graph.find_node(node_name, "v1").unwrap_or_else(|| {
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
            binds: Vec::new(),
            pairs: Vec::new(),
            defer_pairs: Vec::new(),
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
    let info_response = poll(
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
    )
    .await
    .expect("node ready service should start");
    let _node_health_handle = listen_for_node_health(
        &node_messenger,
        &core_node_name,
        instance_id,
        test_node_target(node_name),
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
            binds: Vec::new(),
            pairs: Vec::new(),
            defer_pairs: Vec::new(),

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
    let node = graph
        .find_node(node_name, "v1")
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
            binds: Vec::new(),
            pairs: Vec::new(),
            defer_pairs: Vec::new(),
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
    let info_response = poll(
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
    )
    .await
    .expect("node ready service should start");
    let _node_health_handle = listen_for_node_health(
        &node_messenger,
        &core_node_name,
        instance_id,
        test_node_target(node_name),
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
            binds: Vec::new(),
            pairs: Vec::new(),
            defer_pairs: Vec::new(),

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
    let node = graph
        .find_node(node_name, "v1")
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
    peppy_schema: "node/v1",
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
            binds: Vec::new(),
            pairs: Vec::new(),
            defer_pairs: Vec::new(),
            idle_timeout: 60,
            max_timeout: 3600,
            force: false,
        },
    }
    .execute(node_ctx)
    .expect("producer node add should succeed");
}

async fn add_built_consumer_with_pins(
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
    )
    .await
    .expect("node ready service should start");
    let health = listen_for_node_health(
        node_messenger,
        core_node_name,
        instance_id,
        test_node_target(producer_name),
    )
    .await
    .expect("node health service should start");
    (ready, health)
}

// ─── --bind binding-driven-routing integration tests ───────────────────────

/// Consumer manifest declares two pinned `link_id`s. Running the
/// consumer without `--bind` for either of them is a hard error
/// (rule 1: pinned-unbound rejection). The error names every missing
/// link_id and the spawn must not happen.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn node_run_bind_rejects_pinned_unbound() {
    let serve = ServeCommandEmulation::with_mock()
        .await
        .expect("failed to create serve emulation");
    let shared_messenger = serve.messenger();
    let core_node_name = serve.core_node_name().to_string();

    let work_dir = tempfile::tempdir().expect("failed to create work dir");
    let producer_name = "test_bind_reject_producer";
    let consumer_name = "test_bind_reject_consumer";
    let instance_id = "consumer_inst";

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
    add_built_consumer_with_pins(
        &node_ctx,
        work_dir.path(),
        consumer_name,
        producer_name,
        &["wrist_left", "wrist_right"],
    )
    .await;

    let node_messenger = MessengerHandle::from_shared(Arc::clone(&shared_messenger));
    let _services =
        install_node_services(&node_messenger, &core_node_name, consumer_name, instance_id).await;

    let result = NodeCommand {
        command: NodeCommands::Run {
            node_ref: None,
            node_name: Some(consumer_name.to_string()),
            tag: Some("v1".to_string()),
            args: Vec::new(),
            instance_id: Some(instance_id.to_string()),
            binds: Vec::new(),
            pairs: Vec::new(),
            defer_pairs: Vec::new(),
            idle_timeout: 60,
            max_timeout: 3600,
            build: false,
        },
    }
    .execute(&node_ctx);

    let err = result.expect_err("node run must fail when pinned deps are unbound");
    let err_msg = err.to_string();
    assert!(
        err_msg.contains("is unbound"),
        "error should report each slot as unbound. Got: {err_msg}"
    );
    assert!(
        err_msg.contains("wrist_left"),
        "error should name missing link_id 'wrist_left'. Got: {err_msg}"
    );
    assert!(
        err_msg.contains("wrist_right"),
        "error should name missing link_id 'wrist_right'. Got: {err_msg}"
    );

    let logs = log_capture.logs();
    assert!(
        !logs.contains("Started node instance"),
        "run must NOT complete when a pinned dep is unbound. Logs:\n{logs}"
    );
}

/// `--bind` with a KEY that isn't declared in the consumer's
/// `depends_on` is a hard error (dead-binding). The launcher's
/// `validate_bindings` already raises it as `BindingDeadKey`; the CLI
/// surfaces the message and aborts the run before any spawn side-effect.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn node_run_bind_rejects_dead_key() {
    let serve = ServeCommandEmulation::with_mock()
        .await
        .expect("failed to create serve emulation");
    let shared_messenger = serve.messenger();
    let core_node_name = serve.core_node_name().to_string();

    let work_dir = tempfile::tempdir().expect("failed to create work dir");
    let producer_name = "test_dead_key_producer";
    let consumer_name = "test_dead_key_consumer";
    let producer_instance_id = "cam_a";
    let consumer_instance_id = "consumer_inst";

    let node_ctx = Arc::new(
        AppContext::with_messenger(work_dir.path(), Arc::clone(&shared_messenger))
            .with_daemon_state_file(serve.daemon_state_path()),
    );

    add_built_producer(&node_ctx, work_dir.path(), producer_name).await;
    add_built_consumer_with_pins(
        &node_ctx,
        work_dir.path(),
        consumer_name,
        producer_name,
        &["wrist_left"],
    )
    .await;

    let node_messenger = MessengerHandle::from_shared(Arc::clone(&shared_messenger));
    let _producer_svcs = install_node_services(
        &node_messenger,
        &core_node_name,
        producer_name,
        producer_instance_id,
    )
    .await;
    NodeCommand {
        command: NodeCommands::Run {
            node_ref: None,
            node_name: Some(producer_name.to_string()),
            tag: Some("v1".to_string()),
            args: Vec::new(),
            instance_id: Some(producer_instance_id.to_string()),
            binds: Vec::new(),
            pairs: Vec::new(),
            defer_pairs: Vec::new(),
            idle_timeout: 60,
            max_timeout: 3600,
            build: false,
        },
    }
    .execute(&node_ctx)
    .expect("producer run should succeed");

    let result = NodeCommand {
        command: NodeCommands::Run {
            node_ref: None,
            node_name: Some(consumer_name.to_string()),
            tag: Some("v1".to_string()),
            args: Vec::new(),
            instance_id: Some(consumer_instance_id.to_string()),
            binds: vec![("ghost".to_string(), producer_instance_id.to_string())],
            pairs: Vec::new(),
            defer_pairs: Vec::new(),
            idle_timeout: 60,
            max_timeout: 3600,
            build: false,
        },
    }
    .execute(&node_ctx);
    let err = match result {
        Err(e) => e,
        Ok(()) => panic!("--bind ghost@<id> should have been rejected as a dead-key"),
    };
    let msg = err.to_string();
    assert!(
        msg.contains("ghost"),
        "dead-key error should name the unknown key. Got: {msg}"
    );
}

/// Positive control: a consumer with two pinned `depends_on` entries, run
/// with `--bind` satisfying both, produces no warning and the run
/// completes normally. Guards against the warning regressing from
/// "missing pin" into "always fires" or "fires when satisfied".
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn node_run_bind_emits_no_warning_when_all_pinned_deps_have_binds() {
    let serve = ServeCommandEmulation::with_mock()
        .await
        .expect("failed to create serve emulation");
    let shared_messenger = serve.messenger();
    let core_node_name = serve.core_node_name().to_string();

    let work_dir = tempfile::tempdir().expect("failed to create work dir");
    let producer_name = "test_no_warn_producer";
    let consumer_name = "test_no_warn_consumer";
    let producer_left_id = "cam_left";
    let producer_right_id = "cam_right";
    let consumer_instance_id = "consumer_inst";

    let node_ctx = Arc::new(
        AppContext::with_messenger(work_dir.path(), Arc::clone(&shared_messenger))
            .with_daemon_state_file(serve.daemon_state_path()),
    );

    add_built_producer(&node_ctx, work_dir.path(), producer_name).await;
    add_built_consumer_with_pins(
        &node_ctx,
        work_dir.path(),
        consumer_name,
        producer_name,
        &["wrist_left", "wrist_right"],
    )
    .await;

    let node_messenger = MessengerHandle::from_shared(Arc::clone(&shared_messenger));
    let _left_svcs = install_node_services(
        &node_messenger,
        &core_node_name,
        producer_name,
        producer_left_id,
    )
    .await;
    let _right_svcs = install_node_services(
        &node_messenger,
        &core_node_name,
        producer_name,
        producer_right_id,
    )
    .await;
    for instance_id in [producer_left_id, producer_right_id] {
        NodeCommand {
            command: NodeCommands::Run {
                node_ref: None,
                node_name: Some(producer_name.to_string()),
                tag: Some("v1".to_string()),
                args: Vec::new(),
                instance_id: Some(instance_id.to_string()),
                binds: Vec::new(),
                pairs: Vec::new(),
                defer_pairs: Vec::new(),
                idle_timeout: 60,
                max_timeout: 3600,
                build: false,
            },
        }
        .execute(&node_ctx)
        .expect("producer run should succeed");
    }

    let _consumer_svcs = install_node_services(
        &node_messenger,
        &core_node_name,
        consumer_name,
        consumer_instance_id,
    )
    .await;

    // Capture only the consumer-run logs so we can assert the missing-pin
    // warning string is absent without false-positive matches from producer
    // bootstrap output.
    let log_capture = LogCapture::new();
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .without_time()
        .with_writer(log_capture.clone())
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);

    NodeCommand {
        command: NodeCommands::Run {
            pairs: Vec::new(),
            defer_pairs: Vec::new(),
            node_ref: None,
            node_name: Some(consumer_name.to_string()),
            tag: Some("v1".to_string()),
            args: Vec::new(),
            instance_id: Some(consumer_instance_id.to_string()),
            binds: vec![
                ("wrist_left".to_string(), producer_left_id.to_string()),
                ("wrist_right".to_string(), producer_right_id.to_string()),
            ],
            idle_timeout: 60,
            max_timeout: 3600,
            build: false,
        },
    }
    .execute(&node_ctx)
    .expect("consumer run should succeed when all pinned deps have binds");

    let logs = log_capture.logs();
    assert!(
        !logs.contains("pinned dependencies with no"),
        "no missing-pin warning should fire when every pinned dep is bound. Logs:\n{logs}"
    );
}

/// `--bind KEY@VALUE` where VALUE is an `instance_id` that belongs to a
/// node of a different `(name, tag)` than the consumer's `depends_on`
/// declared is a hard error (target mismatch). Catches a misrouting class
/// the dead-key check doesn't cover: KEY *is* declared, the target
/// *exists*, but it deploys the wrong node identity.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn node_run_bind_rejects_target_mismatch() {
    let serve = ServeCommandEmulation::with_mock()
        .await
        .expect("failed to create serve emulation");
    let shared_messenger = serve.messenger();
    let core_node_name = serve.core_node_name().to_string();

    let work_dir = tempfile::tempdir().expect("failed to create work dir");
    let expected_producer = "test_mismatch_expected_producer";
    let wrong_producer = "test_mismatch_wrong_producer";
    let consumer_name = "test_mismatch_consumer";
    let wrong_instance_id = "wrong_prod_inst";
    let consumer_instance_id = "consumer_inst";

    let node_ctx = Arc::new(
        AppContext::with_messenger(work_dir.path(), Arc::clone(&shared_messenger))
            .with_daemon_state_file(serve.daemon_state_path()),
    );

    add_built_producer(&node_ctx, work_dir.path(), expected_producer).await;
    add_built_producer(&node_ctx, work_dir.path(), wrong_producer).await;
    add_built_consumer_with_pins(
        &node_ctx,
        work_dir.path(),
        consumer_name,
        expected_producer,
        &["wrist_left"],
    )
    .await;

    let node_messenger = MessengerHandle::from_shared(Arc::clone(&shared_messenger));
    let _wrong_svcs = install_node_services(
        &node_messenger,
        &core_node_name,
        wrong_producer,
        wrong_instance_id,
    )
    .await;
    NodeCommand {
        command: NodeCommands::Run {
            node_ref: None,
            node_name: Some(wrong_producer.to_string()),
            tag: Some("v1".to_string()),
            args: Vec::new(),
            instance_id: Some(wrong_instance_id.to_string()),
            binds: Vec::new(),
            pairs: Vec::new(),
            defer_pairs: Vec::new(),
            idle_timeout: 60,
            max_timeout: 3600,
            build: false,
        },
    }
    .execute(&node_ctx)
    .expect("wrong-node producer run should succeed");

    let result = NodeCommand {
        command: NodeCommands::Run {
            node_ref: None,
            node_name: Some(consumer_name.to_string()),
            tag: Some("v1".to_string()),
            args: Vec::new(),
            instance_id: Some(consumer_instance_id.to_string()),
            binds: vec![("wrist_left".to_string(), wrong_instance_id.to_string())],
            pairs: Vec::new(),
            defer_pairs: Vec::new(),
            idle_timeout: 60,
            max_timeout: 3600,
            build: false,
        },
    }
    .execute(&node_ctx);
    let err = match result {
        Err(e) => e,
        Ok(()) => panic!("--bind to wrong-node instance should have been rejected"),
    };
    let msg = err.to_string();
    assert!(
        msg.contains(wrong_instance_id),
        "target-mismatch error should name the offending instance. Got: {msg}"
    );
    assert!(
        msg.contains(expected_producer) || msg.contains("camera") || msg.contains("expected"),
        "target-mismatch error should hint at the expected node identity. Got: {msg}"
    );
}

/// Launching a new instance must not surface unbound-slot errors for
/// pins that belong to a *different* consumer already running in the
/// stack with all its pins satisfied. The pre-flight only validates
/// the new invocation's bindings; bindings of running consumers were
/// resolved when those consumers were started and are not the new
/// invocation's concern.
///
/// Setup: producer `cam` is built, plus a built consumer `cons_a` with
/// two pinned `link_id`s on `cam`. Spawn two `cam` instances and
/// `cons_a` with valid `--bind`s satisfying both pins. Then spawn a
/// THIRD `cam` instance: `cons_a` is running clean, the new
/// invocation has no binds at all, and Rule 1 must NOT fire against
/// `cons_a`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn node_run_does_not_false_flag_existing_consumer_pinned_slots() {
    let serve = ServeCommandEmulation::with_mock()
        .await
        .expect("failed to create serve emulation");
    let shared_messenger = serve.messenger();
    let core_node_name = serve.core_node_name().to_string();

    let work_dir = tempfile::tempdir().expect("failed to create work dir");
    let producer_name = "test_no_falseflag_producer";
    let consumer_name = "test_no_falseflag_consumer";
    let producer_left_id = "cam_left";
    let producer_right_id = "cam_right";
    let producer_extra_id = "cam_extra";
    let consumer_instance_id = "cons_a_inst";

    let node_ctx = Arc::new(
        AppContext::with_messenger(work_dir.path(), Arc::clone(&shared_messenger))
            .with_daemon_state_file(serve.daemon_state_path()),
    );

    add_built_producer(&node_ctx, work_dir.path(), producer_name).await;
    add_built_consumer_with_pins(
        &node_ctx,
        work_dir.path(),
        consumer_name,
        producer_name,
        &["wrist_left", "wrist_right"],
    )
    .await;

    let node_messenger = MessengerHandle::from_shared(Arc::clone(&shared_messenger));
    let _left_svcs = install_node_services(
        &node_messenger,
        &core_node_name,
        producer_name,
        producer_left_id,
    )
    .await;
    let _right_svcs = install_node_services(
        &node_messenger,
        &core_node_name,
        producer_name,
        producer_right_id,
    )
    .await;
    for instance_id in [producer_left_id, producer_right_id] {
        NodeCommand {
            command: NodeCommands::Run {
                node_ref: None,
                node_name: Some(producer_name.to_string()),
                tag: Some("v1".to_string()),
                args: Vec::new(),
                instance_id: Some(instance_id.to_string()),
                binds: Vec::new(),
                pairs: Vec::new(),
                defer_pairs: Vec::new(),
                idle_timeout: 60,
                max_timeout: 3600,
                build: false,
            },
        }
        .execute(&node_ctx)
        .expect("producer run should succeed");
    }

    let _consumer_svcs = install_node_services(
        &node_messenger,
        &core_node_name,
        consumer_name,
        consumer_instance_id,
    )
    .await;
    NodeCommand {
        command: NodeCommands::Run {
            pairs: Vec::new(),
            defer_pairs: Vec::new(),
            node_ref: None,
            node_name: Some(consumer_name.to_string()),
            tag: Some("v1".to_string()),
            args: Vec::new(),
            instance_id: Some(consumer_instance_id.to_string()),
            binds: vec![
                ("wrist_left".to_string(), producer_left_id.to_string()),
                ("wrist_right".to_string(), producer_right_id.to_string()),
            ],
            idle_timeout: 60,
            max_timeout: 3600,
            build: false,
        },
    }
    .execute(&node_ctx)
    .expect("consumer run with both pins bound should succeed");

    // Third producer instance: `cons_a` is already running with all
    // its pinned slots satisfied, and this invocation has nothing to
    // do with `cons_a`. The pre-flight must validate only the new
    // synthesized instance; `cons_a`'s pinned slots are out of scope.
    let _extra_svcs = install_node_services(
        &node_messenger,
        &core_node_name,
        producer_name,
        producer_extra_id,
    )
    .await;
    let result = NodeCommand {
        command: NodeCommands::Run {
            node_ref: None,
            node_name: Some(producer_name.to_string()),
            tag: Some("v1".to_string()),
            args: Vec::new(),
            instance_id: Some(producer_extra_id.to_string()),
            binds: Vec::new(),
            pairs: Vec::new(),
            defer_pairs: Vec::new(),
            idle_timeout: 60,
            max_timeout: 3600,
            build: false,
        },
    }
    .execute(&node_ctx);

    if let Err(err) = &result {
        let msg = err.to_string();
        assert!(
            !msg.contains("wrist_left") && !msg.contains("wrist_right"),
            "running an unrelated node must not report the existing consumer's pinned slots \
             as unbound. Got: {msg}"
        );
    }
    result.expect(
        "running a third producer instance must not fail validation because of an existing \
         consumer's already-satisfied pinned slots",
    );
}

/// Rule 1 (`BindingMissingForPinnedDep`) fires for the new instance's
/// missing pinned binds, scoped to that instance only. Inert items for
/// already-running consumers participate in producer lookup and
/// stack-wide `instance_id` uniqueness but never contribute slots to
/// Rule 1.
///
/// Setup: producer `cam` is built, plus consumer `cons_a` (running
/// with both pins bound) and consumer `cons_b` (a second consumer
/// with one pinned `link_id`). Launching `cons_b` with no `--bind`
/// must be rejected with an error naming `cons_b`'s missing link_id
/// only, never `cons_a`'s.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn node_run_still_rejects_pinned_unbound_on_new_instance_when_others_run_clean() {
    let serve = ServeCommandEmulation::with_mock()
        .await
        .expect("failed to create serve emulation");
    let shared_messenger = serve.messenger();
    let core_node_name = serve.core_node_name().to_string();

    let work_dir = tempfile::tempdir().expect("failed to create work dir");
    let producer_name = "test_rule1_still_fires_producer";
    let consumer_a_name = "test_rule1_still_fires_cons_a";
    let consumer_b_name = "test_rule1_still_fires_cons_b";
    let producer_left_id = "cam_left";
    let producer_right_id = "cam_right";
    let consumer_a_instance_id = "cons_a_inst";
    let consumer_b_instance_id = "cons_b_inst";

    let node_ctx = Arc::new(
        AppContext::with_messenger(work_dir.path(), Arc::clone(&shared_messenger))
            .with_daemon_state_file(serve.daemon_state_path()),
    );

    add_built_producer(&node_ctx, work_dir.path(), producer_name).await;
    add_built_consumer_with_pins(
        &node_ctx,
        work_dir.path(),
        consumer_a_name,
        producer_name,
        &["wrist_left", "wrist_right"],
    )
    .await;
    add_built_consumer_with_pins(
        &node_ctx,
        work_dir.path(),
        consumer_b_name,
        producer_name,
        &["second_consumer_pin"],
    )
    .await;

    let node_messenger = MessengerHandle::from_shared(Arc::clone(&shared_messenger));
    let _left_svcs = install_node_services(
        &node_messenger,
        &core_node_name,
        producer_name,
        producer_left_id,
    )
    .await;
    let _right_svcs = install_node_services(
        &node_messenger,
        &core_node_name,
        producer_name,
        producer_right_id,
    )
    .await;
    for instance_id in [producer_left_id, producer_right_id] {
        NodeCommand {
            command: NodeCommands::Run {
                node_ref: None,
                node_name: Some(producer_name.to_string()),
                tag: Some("v1".to_string()),
                args: Vec::new(),
                instance_id: Some(instance_id.to_string()),
                binds: Vec::new(),
                pairs: Vec::new(),
                defer_pairs: Vec::new(),
                idle_timeout: 60,
                max_timeout: 3600,
                build: false,
            },
        }
        .execute(&node_ctx)
        .expect("producer run should succeed");
    }

    let _cons_a_svcs = install_node_services(
        &node_messenger,
        &core_node_name,
        consumer_a_name,
        consumer_a_instance_id,
    )
    .await;
    NodeCommand {
        command: NodeCommands::Run {
            pairs: Vec::new(),
            defer_pairs: Vec::new(),
            node_ref: None,
            node_name: Some(consumer_a_name.to_string()),
            tag: Some("v1".to_string()),
            args: Vec::new(),
            instance_id: Some(consumer_a_instance_id.to_string()),
            binds: vec![
                ("wrist_left".to_string(), producer_left_id.to_string()),
                ("wrist_right".to_string(), producer_right_id.to_string()),
            ],
            idle_timeout: 60,
            max_timeout: 3600,
            build: false,
        },
    }
    .execute(&node_ctx)
    .expect("consumer_a run with both pins bound should succeed");

    // cons_b has a pinned dep but we deliberately omit --bind. Rule 1
    // must fire for cons_b's slot, NOT for cons_a's already-satisfied
    // slots.
    let result = NodeCommand {
        command: NodeCommands::Run {
            node_ref: None,
            node_name: Some(consumer_b_name.to_string()),
            tag: Some("v1".to_string()),
            args: Vec::new(),
            instance_id: Some(consumer_b_instance_id.to_string()),
            binds: Vec::new(),
            pairs: Vec::new(),
            defer_pairs: Vec::new(),
            idle_timeout: 60,
            max_timeout: 3600,
            build: false,
        },
    }
    .execute(&node_ctx);

    let err = result.expect_err("cons_b with no --bind must be rejected by Rule 1");
    let msg = err.to_string();
    assert!(
        msg.contains("second_consumer_pin"),
        "Rule 1 must still name cons_b's missing link_id. Got: {msg}"
    );
    assert!(
        msg.contains(consumer_b_instance_id),
        "Rule 1 error should name the new instance ('{consumer_b_instance_id}'). Got: {msg}"
    );
    assert!(
        !msg.contains("wrist_left") && !msg.contains("wrist_right"),
        "Rule 1 must NOT report cons_a's already-satisfied pins as unbound. Got: {msg}"
    );
    assert!(
        !msg.contains(consumer_a_instance_id),
        "Rule 1 error must not implicate cons_a's instance_id. Got: {msg}"
    );
}

/// When the target node already has running instances, the pre-flight
/// splits the synthesized new instance into its own validator group:
/// existing instances of the same node are inert under per-instance
/// rules, and only the new instance's bindings are checked. A second
/// instance launched with valid `--bind`s succeeds; one launched with
/// missing `--bind`s fails naming only itself.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn node_run_target_already_in_stack_validates_only_new_instance() {
    let serve = ServeCommandEmulation::with_mock()
        .await
        .expect("failed to create serve emulation");
    let shared_messenger = serve.messenger();
    let core_node_name = serve.core_node_name().to_string();

    let work_dir = tempfile::tempdir().expect("failed to create work dir");
    let producer_name = "test_target_in_stack_producer";
    let consumer_name = "test_target_in_stack_consumer";
    let producer_left_id = "cam_left";
    let producer_right_id = "cam_right";
    let consumer_inst_1 = "cons_inst_1";
    let consumer_inst_2 = "cons_inst_2";
    let consumer_inst_3_bad = "cons_inst_3";

    let node_ctx = Arc::new(
        AppContext::with_messenger(work_dir.path(), Arc::clone(&shared_messenger))
            .with_daemon_state_file(serve.daemon_state_path()),
    );

    add_built_producer(&node_ctx, work_dir.path(), producer_name).await;
    add_built_consumer_with_pins(
        &node_ctx,
        work_dir.path(),
        consumer_name,
        producer_name,
        &["wrist_left", "wrist_right"],
    )
    .await;

    let node_messenger = MessengerHandle::from_shared(Arc::clone(&shared_messenger));
    let _left_svcs = install_node_services(
        &node_messenger,
        &core_node_name,
        producer_name,
        producer_left_id,
    )
    .await;
    let _right_svcs = install_node_services(
        &node_messenger,
        &core_node_name,
        producer_name,
        producer_right_id,
    )
    .await;
    for instance_id in [producer_left_id, producer_right_id] {
        NodeCommand {
            command: NodeCommands::Run {
                node_ref: None,
                node_name: Some(producer_name.to_string()),
                tag: Some("v1".to_string()),
                args: Vec::new(),
                instance_id: Some(instance_id.to_string()),
                binds: Vec::new(),
                pairs: Vec::new(),
                defer_pairs: Vec::new(),
                idle_timeout: 60,
                max_timeout: 3600,
                build: false,
            },
        }
        .execute(&node_ctx)
        .expect("producer run should succeed");
    }

    // First consumer instance: both pins bound.
    let _cons_1_svcs = install_node_services(
        &node_messenger,
        &core_node_name,
        consumer_name,
        consumer_inst_1,
    )
    .await;
    NodeCommand {
        command: NodeCommands::Run {
            pairs: Vec::new(),
            defer_pairs: Vec::new(),
            node_ref: None,
            node_name: Some(consumer_name.to_string()),
            tag: Some("v1".to_string()),
            args: Vec::new(),
            instance_id: Some(consumer_inst_1.to_string()),
            binds: vec![
                ("wrist_left".to_string(), producer_left_id.to_string()),
                ("wrist_right".to_string(), producer_right_id.to_string()),
            ],
            idle_timeout: 60,
            max_timeout: 3600,
            build: false,
        },
    }
    .execute(&node_ctx)
    .expect("first consumer instance with both pins bound should succeed");

    // Second consumer instance: also both pins bound. cons_inst_1
    // enters the validator snapshot as an inert entry (no depends_on)
    // and cons_inst_2 as a live entry whose bindings are checked
    // against the consumer's depends_on.
    let _cons_2_svcs = install_node_services(
        &node_messenger,
        &core_node_name,
        consumer_name,
        consumer_inst_2,
    )
    .await;
    NodeCommand {
        command: NodeCommands::Run {
            pairs: Vec::new(),
            defer_pairs: Vec::new(),
            node_ref: None,
            node_name: Some(consumer_name.to_string()),
            tag: Some("v1".to_string()),
            args: Vec::new(),
            instance_id: Some(consumer_inst_2.to_string()),
            binds: vec![
                ("wrist_left".to_string(), producer_left_id.to_string()),
                ("wrist_right".to_string(), producer_right_id.to_string()),
            ],
            idle_timeout: 60,
            max_timeout: 3600,
            build: false,
        },
    }
    .execute(&node_ctx)
    .expect("second consumer instance must succeed when its own binds are valid");

    // Third consumer instance with deliberately missing binds. The
    // error must name only cons_inst_3, never cons_inst_1 or
    // cons_inst_2 (whose binds were already validated at their own
    // spawn time).
    let result = NodeCommand {
        command: NodeCommands::Run {
            node_ref: None,
            node_name: Some(consumer_name.to_string()),
            tag: Some("v1".to_string()),
            args: Vec::new(),
            instance_id: Some(consumer_inst_3_bad.to_string()),
            binds: Vec::new(),
            pairs: Vec::new(),
            defer_pairs: Vec::new(),
            idle_timeout: 60,
            max_timeout: 3600,
            build: false,
        },
    }
    .execute(&node_ctx);

    let err = result.expect_err("third consumer instance with no --bind must be rejected");
    let msg = err.to_string();
    assert!(
        msg.contains(consumer_inst_3_bad),
        "error should name the new instance ('{consumer_inst_3_bad}'). Got: {msg}"
    );
    assert!(
        !msg.contains(consumer_inst_1) && !msg.contains(consumer_inst_2),
        "error must not implicate the already-running consumer instances. Got: {msg}"
    );
}
