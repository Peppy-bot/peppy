mod common;

use common::{CALLER_INSTANCE_ID, start_master_node};
use config::consts::NODE_CONFIG_FILE;
use config::node::Name;
use master_node::encoding::{NodeAddRequest, NodeStopRequest};
use peppylib::messaging::MessengerHandle;
use peppylib::services::shutdown::listen_for_shutdown;
use std::sync::Arc;
use std::time::Duration;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_stop_success() {
    const TARGET_NODE_NAME: &str = "stoppable_node";
    const TARGET_NODE_TAG: &str = "0.1.0";
    const TARGET_INSTANCE_ID: &str = "stoppable_instance";

    let started_master = start_master_node().await;
    let node_stack = started_master.node_stack.clone();

    let source_dir = tempfile::tempdir().expect("failed to create temp source dir");

    // Add the node to the stack so it can be discovered by instance_id
    let peppy_json5 = format!(
        r#"{{
            schema_version: 1,
            manifest: {{
                name: "{TARGET_NODE_NAME}",
                tag: "{TARGET_NODE_TAG}",
                launch_cmd: ["sleep", "10"]
            }},
            parameters: {{}}
        }}"#
    );
    std::fs::write(source_dir.path().join(NODE_CONFIG_FILE), &peppy_json5)
        .expect("failed to write peppy.json5");

    let add_response = NodeAddRequest::new(source_dir.path())
        .with_instance_id(TARGET_INSTANCE_ID)
        .poll(
            &started_master.caller_handle,
            &started_master.master_node_name,
            CALLER_INSTANCE_ID,
            &started_master.master_node_name,
            Duration::from_secs(5),
        )
        .await
        .expect("node_add request should complete");

    assert!(
        add_response.success,
        "node_add should succeed, got error: {:?}",
        add_response.error_message
    );
    assert!(node_stack.contains(TARGET_NODE_NAME, TARGET_NODE_TAG));

    let instance_id = Name::new(TARGET_INSTANCE_ID).expect("valid instance id");
    node_stack
        .add_instance(TARGET_NODE_NAME, TARGET_NODE_TAG, Some(&instance_id))
        .expect("add_instance should succeed");

    // Simulate the target node exposing the shutdown service.
    let shutdown_handle =
        MessengerHandle::from_shared(Arc::clone(&started_master.shared_messenger));
    let (shutdown_task, shutdown_rx) = listen_for_shutdown(
        &shutdown_handle,
        &started_master.master_node_name,
        TARGET_INSTANCE_ID,
        TARGET_NODE_NAME,
    )
    .await
    .expect("failed to start shutdown service");

    // Allow the shutdown service to fully establish its listener
    tokio::time::sleep(Duration::from_millis(50)).await;

    let response = NodeStopRequest::new(TARGET_INSTANCE_ID)
        .poll(
            &started_master.caller_handle,
            &started_master.master_node_name,
            CALLER_INSTANCE_ID,
            &started_master.master_node_name,
            &started_master.master_node_name,
            Duration::from_secs(10),
        )
        .await
        .expect("node_stop request should complete");

    assert!(response.success, "node_stop should succeed");
    assert!(
        response.error_message.is_none(),
        "success response should not include error_message, got: {:?}",
        response.error_message
    );

    tokio::time::timeout(Duration::from_millis(100), shutdown_rx)
        .await
        .expect("shutdown signal should be received within timeout")
        .expect("shutdown channel should not be dropped");

    shutdown_task.abort();
    started_master.task.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_stop_fails_when_instance_id_not_found() {
    const MISSING_INSTANCE_ID: &str = "missing_instance";

    let started_master = start_master_node().await;

    let response = NodeStopRequest::new(MISSING_INSTANCE_ID)
        .poll(
            &started_master.caller_handle,
            &started_master.master_node_name,
            CALLER_INSTANCE_ID,
            &started_master.master_node_name,
            &started_master.master_node_name,
            Duration::from_secs(5),
        )
        .await
        .expect("node_stop request should complete");

    assert!(!response.success, "node_stop should fail");
    let error_message = response
        .error_message
        .as_ref()
        .expect("node_stop failure should include error_message");
    assert!(
        error_message.contains("not found in node stack"),
        "error should mention missing instance, got: {}",
        error_message
    );
    assert!(
        error_message.contains(MISSING_INSTANCE_ID),
        "error should include missing instance id, got: {}",
        error_message
    );

    started_master.task.abort();
}
