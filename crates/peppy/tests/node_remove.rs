use config::node::Toolchain;
use core_node_api::SerializedNodeGraph;
use core_node_api::encoding::StackListRequest;
use peppy::commands::Command;
use peppy::commands::node::{NodeCommand, NodeCommands, NodeName};
use peppy::context::AppContext;
use peppy::test_support::{LogCapture, ServeCommandEmulation};
use peppylib::MessengerHandle;
use peppylib::services::health::listen_for_node_health;
use peppylib::services::ready::listen_for_node_ready;
use peppylib::services::shutdown::listen_for_shutdown;
use std::sync::Arc;
use std::time::Duration;

use peppylib::core_node::transport::poll_stack_list;
const CALLER_INSTANCE_ID: &str = "peppy-test";

#[test]
fn node_remove_command_succeeds() {
    // Use a runtime for async setup; NodeCommand::execute creates its own runtime internally
    let rt = tokio::runtime::Runtime::new().expect("failed to create tokio runtime");
    let serve = rt
        .block_on(ServeCommandEmulation::with_mock())
        .expect("failed to create serve emulation");
    let shared_messenger = serve.messenger();
    let core_node_name = serve.core_node_name().to_string();
    assert!(
        !core_node_name.is_empty(),
        "core_node_name should not be empty"
    );

    let node_dir = tempfile::tempdir().expect("failed to create temp dir for node");
    let node_name = "test_remove_node";
    let node_tag = "0.1.0";

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
    assert!(
        peppy_json5_path.exists(),
        "peppy.json5 should exist at {}",
        peppy_json5_path.display()
    );

    peppy::test_support::disable_build_cmd(&peppy_json5_path);

    NodeCommand {
        command: NodeCommands::Add {
            source: Some(node_path.display().to_string()),
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
    .execute(&node_ctx)
    .expect("node add command should succeed");

    // Assert there is one node + the core node in the node stack after add
    let messenger_handle = node_ctx
        .messenger_handle()
        .expect("messenger handle should be available");

    let response = rt
        .block_on(poll_stack_list(
            &StackListRequest::new(false),
            messenger_handle,
            &core_node_name,
            CALLER_INSTANCE_ID,
            &core_node_name,
            Duration::from_secs(5),
        ))
        .expect("stack_list request should complete");

    let graph: SerializedNodeGraph =
        serde_json::from_str(&response.graph_json).expect("graph_json should parse");

    assert_eq!(
        graph.nodes.len(),
        2,
        "graph should only contain the core node + the added node. Got: {:?}",
        graph.nodes.iter().map(|n| n.label()).collect::<Vec<_>>()
    );

    NodeCommand {
        command: NodeCommands::Remove {
            node_ref: (
                node_name.to_string(),
                node_tag.to_string(),
                "default".to_string(),
            ),
            stop_instances: false,
            force: false,
        },
    }
    .execute(&node_ctx)
    .expect("node remove command should succeed");

    let logs = log_capture.logs();
    assert!(
        logs.contains(&format!("Removed node '{node_name}:{node_tag}'")),
        "logs should contain success message for removing node. Logs:\n{}",
        logs
    );

    let messenger_handle = node_ctx
        .messenger_handle()
        .expect("messenger handle should be available");

    let response = rt
        .block_on(poll_stack_list(
            &StackListRequest::new(false),
            messenger_handle,
            &core_node_name,
            CALLER_INSTANCE_ID,
            &core_node_name,
            Duration::from_secs(5),
        ))
        .expect("stack_list request should complete");

    let graph: SerializedNodeGraph =
        serde_json::from_str(&response.graph_json).expect("graph_json should parse");

    assert_eq!(
        graph.nodes.len(),
        1,
        "graph should only contain the core node. Got: {:?}",
        graph.nodes.iter().map(|n| n.label()).collect::<Vec<_>>()
    );
}

#[test]
fn node_remove_command_force_bypasses_prompt_and_stops_instances() {
    // Use a runtime for async setup; NodeCommand::execute creates its own runtime internally
    let rt = tokio::runtime::Runtime::new().expect("failed to create tokio runtime");
    let serve = rt
        .block_on(ServeCommandEmulation::with_mock())
        .expect("failed to create serve emulation");
    let shared_messenger = serve.messenger();
    let core_node_name = serve.core_node_name().to_string();
    assert!(
        !core_node_name.is_empty(),
        "core_node_name should not be empty"
    );

    let node_dir = tempfile::tempdir().expect("failed to create temp dir for node");
    let node_name = "test_remove_running_node";
    let node_tag = "0.1.0";
    let instance_id = "test_remove_running_instance";

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
    assert!(
        peppy_json5_path.exists(),
        "peppy.json5 should exist at {}",
        peppy_json5_path.display()
    );

    // Override the launch command to avoid spawning a real node process.
    // Health/shutdown services are provided in-process via the mock messenger.
    peppy::test_support::override_run_cmd(&peppy_json5_path);

    // Start in-process node services for health/shutdown so node_run can succeed.
    let node_messenger = MessengerHandle::from_shared(Arc::clone(&shared_messenger));
    let _node_ready_handle = rt
        .block_on(listen_for_node_ready(
            &node_messenger,
            &core_node_name,
            instance_id,
            node_name,
        ))
        .expect("node ready service should start");
    let _node_health_handle = rt
        .block_on(listen_for_node_health(
            &node_messenger,
            &core_node_name,
            instance_id,
            node_name,
        ))
        .expect("node health service should start");
    let (_node_shutdown_handle, node_shutdown_rx) = rt
        .block_on(listen_for_shutdown(
            &node_messenger,
            &core_node_name,
            instance_id,
            node_name,
        ))
        .expect("node shutdown service should start");

    // Use Add with start=true to add and start in a single command execution
    // (avoids cross-runtime issues when using separate Add and Start commands)
    NodeCommand {
        command: NodeCommands::Add {
            source: Some(node_path.display().to_string()),
            git_ref: None,
            variant: Vec::new(),
            sync: false,
            build: true,
            run: true,
            args: Vec::new(),
            instance_id: Some(instance_id.to_string()),
            idle_timeout: 60,
            max_timeout: 3600,
            force: false,
        },
    }
    .execute(&node_ctx)
    .expect("node add+start command should succeed");

    NodeCommand {
        command: NodeCommands::Remove {
            node_ref: (
                node_name.to_string(),
                node_tag.to_string(),
                "default".to_string(),
            ),
            stop_instances: false,
            force: true,
        },
    }
    .execute(&node_ctx)
    .expect("node remove command should succeed");

    rt.block_on(async { tokio::time::timeout(Duration::from_secs(2), node_shutdown_rx).await })
        .expect("shutdown request should arrive")
        .expect("shutdown signal should be delivered");

    // Node should be removed from the stack.
    let messenger_handle = node_ctx
        .messenger_handle()
        .expect("messenger handle should be available");

    let response = rt
        .block_on(poll_stack_list(
            &StackListRequest::new(false),
            messenger_handle,
            &core_node_name,
            CALLER_INSTANCE_ID,
            &core_node_name,
            Duration::from_secs(5),
        ))
        .expect("stack_list request should complete");

    let graph: SerializedNodeGraph =
        serde_json::from_str(&response.graph_json).expect("graph_json should parse");

    assert!(
        !graph
            .nodes
            .iter()
            .any(|n| n.label().contains(&format!("{node_name}:{node_tag}"))),
        "graph should not contain the removed node. Got: {:?}",
        graph.nodes.iter().map(|n| n.label()).collect::<Vec<_>>()
    );
}

#[test]
fn node_remove_command_with_stop_instances_succeeds_and_stops_instances() {
    // Use a runtime for async setup; NodeCommand::execute creates its own runtime internally
    let rt = tokio::runtime::Runtime::new().expect("failed to create tokio runtime");
    let serve = rt
        .block_on(ServeCommandEmulation::with_mock())
        .expect("failed to create serve emulation");
    let shared_messenger = serve.messenger();
    let core_node_name = serve.core_node_name().to_string();
    assert!(
        !core_node_name.is_empty(),
        "core_node_name should not be empty"
    );

    let node_dir = tempfile::tempdir().expect("failed to create temp dir for node");
    let node_name = "test_remove_stop_instances_node";
    let node_tag = "0.1.0";
    let instance_id = "test_remove_stop_instances_instance";

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
    assert!(
        peppy_json5_path.exists(),
        "peppy.json5 should exist at {}",
        peppy_json5_path.display()
    );

    peppy::test_support::override_run_cmd(&peppy_json5_path);

    let node_messenger = MessengerHandle::from_shared(Arc::clone(&shared_messenger));

    let _node_ready_handle = rt
        .block_on(listen_for_node_ready(
            &node_messenger,
            &core_node_name,
            instance_id,
            node_name,
        ))
        .expect("node ready service should start");

    let _node_health_handle = rt
        .block_on(listen_for_node_health(
            &node_messenger,
            &core_node_name,
            instance_id,
            node_name,
        ))
        .expect("node health service should start");

    let (_node_shutdown_handle, node_shutdown_rx) = rt
        .block_on(listen_for_shutdown(
            &node_messenger,
            &core_node_name,
            instance_id,
            node_name,
        ))
        .expect("node shutdown service should start");

    // Use Add with start=true to add and start in a single command execution
    // (avoids cross-runtime issues when using separate Add and Start commands)
    NodeCommand {
        command: NodeCommands::Add {
            source: Some(node_path.display().to_string()),
            git_ref: None,
            variant: Vec::new(),
            sync: false,
            build: true,
            run: true,
            args: Vec::new(),
            instance_id: Some(instance_id.to_string()),
            idle_timeout: 60,
            max_timeout: 3600,
            force: false,
        },
    }
    .execute(&node_ctx)
    .expect("node add+start command should succeed");

    NodeCommand {
        command: NodeCommands::Remove {
            node_ref: (
                node_name.to_string(),
                node_tag.to_string(),
                "default".to_string(),
            ),
            stop_instances: true,
            force: false,
        },
    }
    .execute(&node_ctx)
    .expect("node remove command should succeed");

    rt.block_on(async { tokio::time::timeout(Duration::from_secs(2), node_shutdown_rx).await })
        .expect("shutdown request should arrive")
        .expect("shutdown signal should be delivered");

    let logs = log_capture.logs();
    assert!(
        logs.contains(&format!("Removed node '{node_name}:{node_tag}'")),
        "logs should contain success message for removing node. Logs:\n{}",
        logs
    );

    let messenger_handle = node_ctx
        .messenger_handle()
        .expect("messenger handle should be available");

    let response = rt
        .block_on(poll_stack_list(
            &StackListRequest::new(false),
            messenger_handle,
            &core_node_name,
            CALLER_INSTANCE_ID,
            &core_node_name,
            Duration::from_secs(5),
        ))
        .expect("stack_list request should complete");

    let graph: SerializedNodeGraph =
        serde_json::from_str(&response.graph_json).expect("graph_json should parse");

    assert!(
        !graph
            .nodes
            .iter()
            .any(|n| n.label().contains(&format!("{node_name}:{node_tag}"))),
        "graph should not contain the removed node. Got: {:?}",
        graph.nodes.iter().map(|n| n.label()).collect::<Vec<_>>()
    );
}
