mod common;

use common::{CALLER_INSTANCE_ID, setup_test_master_node};
use master_node::encoding::{NodeAddRequest, NodeStartRequest, NodeStopRequest};
use peppylib::messaging::MessengerHandle;
use peppylib::services::health::listen_for_node_health;
use peppylib::services::shutdown::listen_for_shutdown;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn node_stop_success() {
    let (client, server) = setup_test_master_node().await;

    // First, add a node to the stack with a launch_cmd
    let peppy_json5 = r#"{
        schema_version: 1,
        manifest: {
            name: "stoppable_node",
            tag: "1.0.0",
            launch_cmd: ["true"]
        },
        interfaces: {}
    }"#;

    let from_dir = PathBuf::from("/tmp/test");
    let instance_id = "stoppable_instance";

    let add_request = NodeAddRequest::new(peppy_json5, from_dir).with_instance_id(instance_id);
    let add_response = add_request
        .poll(
            &client.caller_handle,
            &client.master_node_name,
            CALLER_INSTANCE_ID,
            &client.master_node_name,
            Duration::from_secs(2),
        )
        .await
        .expect("add poll should succeed");

    assert!(add_response.success, "node add should succeed");

    // Verify the node is in the stack
    assert!(
        server.node_stack.contains("stoppable_node", "1.0.0"),
        "node should be in the stack"
    );

    let node_name = "stoppable_node";
    let bound_master_node = "test_master_node";

    // Set up a mock health service listener to simulate the spawned node being ready.
    // In real usage, the spawned process would expose this service, but in tests with
    // launch_cmd: ["true"], the process exits immediately.
    let health_handle = MessengerHandle::from_shared(Arc::clone(&server.shared_messenger));
    let _health_task =
        listen_for_node_health(&health_handle, bound_master_node, instance_id, node_name)
            .await
            .expect("failed to start mock health service");

    // Allow the health service to fully establish its listeners
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Now start the instance
    let runtime_config_json5 = r#"{
        messaging_host: "127.0.0.1",
        messaging_port: 7447,
        node_name: "stoppable_node",
        bound_master_node: "test_master_node",
        deployment_instance: {
            instance_id: "stoppable_instance"
        },
        codegen_peppy_config_md5: "d41d8cd98f00b204e9800998ecf8427e"
    }"#;

    let start_request = NodeStartRequest::new(runtime_config_json5);
    let start_response = start_request
        .poll(
            &client.caller_handle,
            &client.master_node_name,
            CALLER_INSTANCE_ID,
            &client.master_node_name,
            Duration::from_secs(2),
        )
        .await
        .expect("start poll should succeed");

    assert!(
        start_response.success,
        "node start should succeed, got error: {:?}",
        start_response.error_message
    );

    // Set up a shutdown service for the child node (simulating the node being "running")
    let child_messenger = MessengerHandle::from_shared(Arc::clone(&server.shared_messenger));
    let (_shutdown_task, _shutdown_rx) = listen_for_shutdown(
        &child_messenger,
        &client.master_node_name,
        instance_id,
        "stoppable_node",
    )
    .await
    .expect("failed to start shutdown service");

    // Allow the shutdown service to fully establish
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Now send a stop request
    let stop_request = NodeStopRequest::new(instance_id);
    let stop_response = stop_request
        .poll(
            &client.caller_handle,
            &client.master_node_name,
            CALLER_INSTANCE_ID,
            &client.master_node_name,
            &client.master_node_name,
            Duration::from_secs(5),
        )
        .await
        .expect("stop poll should succeed");

    assert!(
        stop_response.success,
        "node stop should succeed, got error: {:?}",
        stop_response.error_message
    );
    assert!(
        stop_response.error_message.is_none(),
        "error_message should be None on success"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn node_stop_instance_fails_when_not_started() {
    let (client, server) = setup_test_master_node().await;

    // First, add a node to the stack
    let peppy_json5 = r#"{
        schema_version: 1,
        manifest: {
            name: "stoppable_node",
            tag: "1.0.0"
        },
        interfaces: {}
    }"#;

    let from_dir = PathBuf::from("/tmp/test");
    let instance_id = "stoppable_instance";

    let add_request = NodeAddRequest::new(peppy_json5, from_dir).with_instance_id(instance_id);
    let add_response = add_request
        .poll(
            &client.caller_handle,
            &client.master_node_name,
            CALLER_INSTANCE_ID,
            &client.master_node_name,
            Duration::from_secs(2),
        )
        .await
        .expect("add poll should succeed");

    assert!(add_response.success, "node add should succeed");

    // Verify the node is in the stack
    assert!(
        server.node_stack.contains("stoppable_node", "1.0.0"),
        "node should be in the stack"
    );

    // The instance is added but NOT started - no shutdown service is listening

    // Send a stop request - should fail because the instance isn't running
    // Use a longer timeout (10s) since the stop service has a 5s shutdown timeout internally
    let stop_request = NodeStopRequest::new(instance_id);
    let stop_response = stop_request
        .poll(
            &client.caller_handle,
            &client.master_node_name,
            CALLER_INSTANCE_ID,
            &client.master_node_name,
            &client.master_node_name,
            Duration::from_secs(10),
        )
        .await
        .expect("stop poll should succeed");

    assert!(
        !stop_response.success,
        "node stop should fails, got: {:?}",
        stop_response.error_message
    );
    assert!(
        !stop_response.error_message.is_none(),
        "error_message should not be empty"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn node_stop_instance_id_not_found() {
    let (client, _server) = setup_test_master_node().await;

    // Try to stop with a valid instance_id format but one that doesn't exist in the node stack
    let stop_request = NodeStopRequest::new("nonexistent_instance");
    let stop_response = stop_request
        .poll(
            &client.caller_handle,
            &client.master_node_name,
            CALLER_INSTANCE_ID,
            &client.master_node_name,
            &client.master_node_name,
            Duration::from_secs(2),
        )
        .await
        .expect("stop poll should succeed");

    assert!(
        !stop_response.success,
        "node stop should fail for non-existent instance_id"
    );
    let error_message = stop_response
        .error_message
        .expect("error_message should be present on failure");
    assert!(
        error_message.contains("not found"),
        "error should indicate instance not found, got: {}",
        error_message
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn node_stop_invalid_instance_id() {
    let (client, _server) = setup_test_master_node().await;

    // Try to stop with an invalid instance_id (contains invalid characters)
    let stop_request = NodeStopRequest::new("invalid instance id with spaces!");
    let stop_response = stop_request
        .poll(
            &client.caller_handle,
            &client.master_node_name,
            CALLER_INSTANCE_ID,
            &client.master_node_name,
            &client.master_node_name,
            Duration::from_secs(2),
        )
        .await
        .expect("remove poll should succeed");

    assert!(
        !stop_response.success,
        "node stop should fail for invalid instance_id"
    );
    let error_message = stop_response
        .error_message
        .expect("error_message should be present on failure");
    assert!(
        error_message.contains("Invalid instance_id") || error_message.contains("invalid"),
        "error should indicate invalid instance_id, got: {}",
        error_message
    );
}
