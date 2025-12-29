mod common;

use common::{CALLER_INSTANCE_ID, setup_test_master_node};
use master_node::encoding::{NodeAddRequest, NodeRemoveRequest};
use peppylib::messaging::MessengerHandle;
use peppylib::services::shutdown::listen_for_shutdown;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn node_remove_success() {
    let (client, server) = setup_test_master_node().await;

    // Add a node to remove
    let peppy_json5 = r#"{
        schema_version: 1,
        manifest: {
            name: "removable_node",
            tag: "1.0.0"
        },
        interfaces: {}
    }"#;

    let add_request = NodeAddRequest::new(peppy_json5, PathBuf::from("/tmp/test"));
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

    assert!(
        server.node_stack.contains("removable_node", "1.0.0"),
        "node should be in the stack before removal"
    );

    let remove_request = NodeRemoveRequest::new("removable_node");
    let remove_response = remove_request
        .poll(
            &client.caller_handle,
            &client.master_node_name,
            CALLER_INSTANCE_ID,
            &client.master_node_name,
            Duration::from_secs(2),
        )
        .await
        .expect("remove poll should succeed");

    assert!(
        remove_response.success,
        "node remove should succeed, got error: {:?}",
        remove_response.error_message
    );
    assert!(
        remove_response.error_message.is_none(),
        "error_message should be None on success"
    );
    assert!(
        !server.node_stack.contains("removable_node", "1.0.0"),
        "node should be removed from the stack"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn node_remove_node_name_not_found_fails() {
    let (client, _server) = setup_test_master_node().await;

    let remove_request = NodeRemoveRequest::new("missing_node");
    let remove_response = remove_request
        .poll(
            &client.caller_handle,
            &client.master_node_name,
            CALLER_INSTANCE_ID,
            &client.master_node_name,
            Duration::from_secs(2),
        )
        .await
        .expect("remove poll should succeed");

    assert!(
        !remove_response.success,
        "node remove should fail for non-existent node name"
    );
    let error_message = remove_response
        .error_message
        .expect("error_message should be present on failure");
    assert!(
        error_message.contains("not found"),
        "error should indicate node not found, got: {}",
        error_message
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn node_remove_stop_running_instances_first() {
    let (client, server) = setup_test_master_node().await;

    // Add a node with a known instance_id
    let peppy_json5 = r#"{
        schema_version: 1,
        manifest: {
            name: "removable_node",
            tag: "1.0.0"
        },
        interfaces: {}
    }"#;

    let instance_id = "removable_instance";
    let add_request =
        NodeAddRequest::new(peppy_json5, PathBuf::from("/tmp/test")).with_instance_id(instance_id);
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

    // Simulate the node being "running" by exposing the shutdown service for that instance
    let child_messenger = MessengerHandle::from_shared(Arc::clone(&server.shared_messenger));
    let (_shutdown_task, shutdown_rx) = listen_for_shutdown(
        &child_messenger,
        &client.master_node_name,
        instance_id,
        "removable_node",
    )
    .await
    .expect("failed to start shutdown service");

    tokio::time::sleep(Duration::from_millis(50)).await;

    let remove_request = NodeRemoveRequest::new("removable_node").with_stop_instances(true);
    let remove_response = remove_request
        .poll(
            &client.caller_handle,
            &client.master_node_name,
            CALLER_INSTANCE_ID,
            &client.master_node_name,
            Duration::from_secs(5),
        )
        .await
        .expect("remove poll should succeed");

    assert!(
        remove_response.success,
        "node remove should succeed, got error: {:?}",
        remove_response.error_message
    );

    // The remove service should have issued a shutdown request before removing the node.
    tokio::time::timeout(Duration::from_millis(200), shutdown_rx)
        .await
        .expect("shutdown signal should be received within timeout")
        .expect("shutdown channel should not be dropped");

    assert!(
        !server.node_stack.contains("removable_node", "1.0.0"),
        "node should be removed from the stack"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn node_remove_fails_on_stop_running_instances_if_stop_instances_parameter_not_set() {
    let (client, server) = setup_test_master_node().await;

    // Add a node with a known instance_id
    let peppy_json5 = r#"{
        schema_version: 1,
        manifest: {
            name: "removable_node",
            tag: "1.0.0"
        },
        interfaces: {}
    }"#;

    let instance_id = "removable_instance";
    let add_request =
        NodeAddRequest::new(peppy_json5, PathBuf::from("/tmp/test")).with_instance_id(instance_id);
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

    // Simulate the node being "running" by exposing the shutdown service for that instance
    let child_messenger = MessengerHandle::from_shared(Arc::clone(&server.shared_messenger));
    let (_shutdown_task, shutdown_rx) = listen_for_shutdown(
        &child_messenger,
        &client.master_node_name,
        instance_id,
        "removable_node",
    )
    .await
    .expect("failed to start shutdown service");

    tokio::time::sleep(Duration::from_millis(50)).await;

    let remove_request = NodeRemoveRequest::new("removable_node");
    let remove_response = remove_request
        .poll(
            &client.caller_handle,
            &client.master_node_name,
            CALLER_INSTANCE_ID,
            &client.master_node_name,
            Duration::from_secs(2),
        )
        .await
        .expect("remove poll should succeed");

    assert!(
        !remove_response.success,
        "node remove should fail when running instances exist and stop_instances is not set"
    );
    let error_message = remove_response
        .error_message
        .expect("error_message should be present on failure");
    assert!(
        error_message.contains("stop_instances"),
        "error should mention stop_instances, got: {}",
        error_message
    );

    assert!(
        server.node_stack.contains("removable_node", "1.0.0"),
        "node should remain in the stack on failure"
    );

    // stop_instances was not set, so no shutdown signal should be issued.
    assert!(
        tokio::time::timeout(Duration::from_millis(200), shutdown_rx)
            .await
            .is_err(),
        "shutdown signal should not be received"
    );
}
