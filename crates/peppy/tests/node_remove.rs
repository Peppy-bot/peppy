mod helpers;

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use config::node::NodeConfigParser;
use helpers::TestServeHandle;
use master_node::encoding::NodeListRequest;
use node_stack::SerializedNodeGraph;
use peppy::commands::Command;
use peppy::commands::node::{NodeCommand, NodeCommands, NodeName};
use peppy::context::{AppContext, DaemonState};
use peppy::error::Error as PeppyError;
use peppylib::MessengerHandle;
use peppylib::services::health::listen_for_node_health;
use peppylib::services::ready::listen_for_node_ready;
use peppylib::services::shutdown::listen_for_shutdown;

const CALLER_INSTANCE_ID: &str = "peppy-test";

fn override_start_cmd(peppy_json5: &Path) {
    let mut cfg = NodeConfigParser::from_path(peppy_json5).expect("peppy.json5 should read");
    // Avoid spawning a real node binary in tests, but keep the process alive long enough for
    // `node_start` to complete its `node_ready` + health check phases.
    cfg.manifest.start_cmd = vec!["sleep".to_string(), "5".to_string()];

    // Write JSON (valid JSON5) back to disk.
    let updated_content = serde_json::to_string_pretty(&cfg).expect("peppy.json5 should serialize");
    std::fs::write(peppy_json5, updated_content).expect("peppy.json5 should update");
}

#[test]
fn node_remove_command_succeeds() {
    let _serial_guard = helpers::serve_test_guard();
    let serve = TestServeHandle::with_mock_messenger();

    let daemon_state = DaemonState::read().expect("daemon state should be readable");
    let master_node_name = daemon_state.master_node_name;
    assert!(
        !master_node_name.is_empty(),
        "master_node_name should not be empty"
    );

    let node_dir = tempfile::tempdir().expect("failed to create temp dir for node");
    let node_name = "test_remove_node";

    let node_ctx = Arc::new(AppContext::with_messenger(
        node_dir.path(),
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
        command: NodeCommands::Init {
            node_name: NodeName::new(node_name).expect("valid node name"),
            to_dir: None,
            build_system: config::peppy_config::BuildSystem::Rust,
        },
    }
    .execute(&node_ctx)
    .expect("node init command should succeed");

    let peppy_json5_path = node_dir.path().join(node_name).join("peppy.json5");
    assert!(
        peppy_json5_path.exists(),
        "peppy.json5 should exist at {}",
        peppy_json5_path.display()
    );

    NodeCommand {
        command: NodeCommands::Add {
            peppy_json5: peppy_json5_path,
            start: false,
            args: Vec::new(),
            instance_id: None,
        },
    }
    .execute(&node_ctx)
    .expect("node add command should succeed");

    // Assert there is one node + the master node in the node stack after add
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

    assert_eq!(
        graph.nodes.len(),
        2,
        "graph should only contain the master node + the added node. Got: {:?}",
        graph.nodes.iter().map(|n| n.label()).collect::<Vec<_>>()
    );

    NodeCommand {
        command: NodeCommands::Remove {
            node_name: NodeName::new(node_name).expect("valid node name"),
            stop_instances: false,
        },
    }
    .execute(&node_ctx)
    .expect("node remove command should succeed");

    let logs = log_capture.logs();
    assert!(
        logs.contains(&format!("Removed node '{node_name}'")),
        "logs should contain success message for removing node. Logs:\n{}",
        logs
    );

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

    assert_eq!(
        graph.nodes.len(),
        1,
        "graph should only contain the master node. Got: {:?}",
        graph.nodes.iter().map(|n| n.label()).collect::<Vec<_>>()
    );
}

#[test]
fn node_remove_command_fails_without_stop_instances_when_running_instance_exists() {
    let _serial_guard = helpers::serve_test_guard();
    let serve = TestServeHandle::with_mock_messenger();

    let daemon_state = DaemonState::read().expect("daemon state should be readable");
    let master_node_name = daemon_state.master_node_name;
    assert!(
        !master_node_name.is_empty(),
        "master_node_name should not be empty"
    );

    let node_dir = tempfile::tempdir().expect("failed to create temp dir for node");
    let node_name = "test_remove_running_node";
    let instance_id = "test_remove_running_instance";

    let node_ctx = Arc::new(AppContext::with_messenger(
        node_dir.path(),
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
        command: NodeCommands::Init {
            node_name: NodeName::new(node_name).expect("valid node name"),
            to_dir: None,
            build_system: config::peppy_config::BuildSystem::Rust,
        },
    }
    .execute(&node_ctx)
    .expect("node init command should succeed");

    let node_path = node_dir.path().join(node_name);
    let peppy_json5_path = node_path.join("peppy.json5");
    assert!(
        peppy_json5_path.exists(),
        "peppy.json5 should exist at {}",
        peppy_json5_path.display()
    );

    // Override the launch command to avoid spawning a real node process.
    // Health/shutdown services are provided in-process via the mock messenger.
    override_start_cmd(&peppy_json5_path);

    NodeCommand {
        command: NodeCommands::Add {
            peppy_json5: peppy_json5_path,
            start: false,
            args: Vec::new(),
            instance_id: None,
        },
    }
    .execute(&node_ctx)
    .expect("node add command should succeed");

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("tokio runtime should create");
    let messenger_handle = node_ctx
        .messenger_handle()
        .expect("messenger handle should be available");

    // Start in-process node services for health/shutdown so node_run can succeed.
    let node_messenger = MessengerHandle::from_shared(serve.messenger());
    let _node_ready_handle = rt
        .block_on(listen_for_node_ready(
            &node_messenger,
            &master_node_name,
            instance_id,
            node_name,
        ))
        .expect("node ready service should start");
    let _node_health_handle = rt
        .block_on(listen_for_node_health(
            &node_messenger,
            &master_node_name,
            instance_id,
            node_name,
        ))
        .expect("node health service should start");
    let (_node_shutdown_handle, _node_shutdown_rx) = rt
        .block_on(listen_for_shutdown(
            &node_messenger,
            &master_node_name,
            instance_id,
            node_name,
        ))
        .expect("node shutdown service should start");

    NodeCommand {
        command: NodeCommands::Start {
            node_ref: None,
            node_name: Some(node_name.to_string()),
            tag: Some("0.1.0".to_string()),
            args: Vec::new(),
            instance_id: Some(instance_id.to_string()),
        },
    }
    .execute(&node_ctx)
    .expect("node run command should succeed");

    let err = NodeCommand {
        command: NodeCommands::Remove {
            node_name: NodeName::new(node_name).expect("valid node name"),
            stop_instances: false,
        },
    }
    .execute(&node_ctx)
    .expect_err("node remove command should fail");

    match err {
        PeppyError::ExecutionFailed(message) => {
            assert!(
                message.contains("has running instances"),
                "error should mention running instances. Got: {}",
                message
            );
            assert!(
                message.contains(instance_id),
                "error should include an example running instance id. Got: {}",
                message
            );
        }
        other => panic!("expected ExecutionFailed error, got: {other:?}"),
    }

    // Node should remain present in the stack.
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
        graph.nodes.iter().any(|n| n
            .label()
            .contains(&format!("{node_name}:0.1.0 (1 instance)"))),
        "graph should contain the node with 1 instance. Got: {:?}",
        graph.nodes.iter().map(|n| n.label()).collect::<Vec<_>>()
    );
}

#[test]
fn node_remove_command_with_stop_instances_succeeds_and_stops_instances() {
    let _serial_guard = helpers::serve_test_guard();
    let serve = TestServeHandle::with_mock_messenger();

    let daemon_state = DaemonState::read().expect("daemon state should be readable");
    let master_node_name = daemon_state.master_node_name;
    assert!(
        !master_node_name.is_empty(),
        "master_node_name should not be empty"
    );

    let node_dir = tempfile::tempdir().expect("failed to create temp dir for node");
    let node_name = "test_remove_stop_instances_node";
    let instance_id = "test_remove_stop_instances_instance";

    let node_ctx = Arc::new(AppContext::with_messenger(
        node_dir.path(),
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
        command: NodeCommands::Init {
            node_name: NodeName::new(node_name).expect("valid node name"),
            to_dir: None,
            build_system: config::peppy_config::BuildSystem::Rust,
        },
    }
    .execute(&node_ctx)
    .expect("node init command should succeed");

    let node_path = node_dir.path().join(node_name);
    let peppy_json5_path = node_path.join("peppy.json5");
    assert!(
        peppy_json5_path.exists(),
        "peppy.json5 should exist at {}",
        peppy_json5_path.display()
    );

    override_start_cmd(&peppy_json5_path);

    NodeCommand {
        command: NodeCommands::Add {
            peppy_json5: peppy_json5_path,
            start: false,
            args: Vec::new(),
            instance_id: None,
        },
    }
    .execute(&node_ctx)
    .expect("node add command should succeed");

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("tokio runtime should create");
    let node_messenger = MessengerHandle::from_shared(serve.messenger());

    let _node_ready_handle = rt
        .block_on(listen_for_node_ready(
            &node_messenger,
            &master_node_name,
            instance_id,
            node_name,
        ))
        .expect("node ready service should start");

    let _node_health_handle = rt
        .block_on(listen_for_node_health(
            &node_messenger,
            &master_node_name,
            instance_id,
            node_name,
        ))
        .expect("node health service should start");

    let (_node_shutdown_handle, node_shutdown_rx) = rt
        .block_on(listen_for_shutdown(
            &node_messenger,
            &master_node_name,
            instance_id,
            node_name,
        ))
        .expect("node shutdown service should start");

    NodeCommand {
        command: NodeCommands::Start {
            node_ref: None,
            node_name: Some(node_name.to_string()),
            tag: Some("0.1.0".to_string()),
            args: Vec::new(),
            instance_id: Some(instance_id.to_string()),
        },
    }
    .execute(&node_ctx)
    .expect("node run command should succeed");

    NodeCommand {
        command: NodeCommands::Remove {
            node_name: NodeName::new(node_name).expect("valid node name"),
            stop_instances: true,
        },
    }
    .execute(&node_ctx)
    .expect("node remove command should succeed");

    rt.block_on(async move {
        tokio::time::timeout(Duration::from_secs(2), node_shutdown_rx)
            .await
            .expect("shutdown request should arrive")
            .expect("shutdown signal should be delivered");
    });

    let logs = log_capture.logs();
    assert!(
        logs.contains(&format!("Removed node '{node_name}'")),
        "logs should contain success message for removing node. Logs:\n{}",
        logs
    );

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
        !graph
            .nodes
            .iter()
            .any(|n| n.label().contains(&format!("{node_name}:0.1.0"))),
        "graph should not contain the removed node. Got: {:?}",
        graph.nodes.iter().map(|n| n.label()).collect::<Vec<_>>()
    );
}
