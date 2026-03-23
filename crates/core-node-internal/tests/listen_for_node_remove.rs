mod common;

use common::{
    AbortOnDrop, CALLER_INSTANCE_ID, send_node_add_and_wait, start_core_node_with_mock_messenger,
    write_peppy_json5,
};
use config::node::Name;
use core_node::encoding::NodeRemoveRequest;
use peppylib::messaging::MessengerHandle;
use peppylib::services::shutdown::listen_for_shutdown;
use std::sync::Arc;
use std::time::Duration;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_remove_success() {
    const TARGET_NODE_NAME: &str = "removable_node";
    const TARGET_NODE_TAG: &str = "0.1.0";

    let started_core_node = start_core_node_with_mock_messenger().await;
    let node_stack = started_core_node.node_stack.clone();

    let source_dir = tempfile::tempdir().expect("failed to create temp source dir");

    let peppy_json5 = r#"{
            schema_version: 1,
            manifest: {
                name: "{TARGET_NODE_NAME}",
                tag: "{TARGET_NODE_TAG}",
            },
            codegen: {
                language: "rust",
            },
            process: {
                start_cmd: ["sleep", "10"]
            },
            parameters: {}
        }"#
    .replace("{TARGET_NODE_NAME}", TARGET_NODE_NAME)
    .replace("{TARGET_NODE_TAG}", TARGET_NODE_TAG);
    write_peppy_json5(source_dir.path(), &peppy_json5);

    let add_response = send_node_add_and_wait(
        &started_core_node.caller_handle,
        &started_core_node.core_node_name,
        source_dir.path(),
        Duration::from_secs(5),
        Duration::from_secs(5),
        None,
    )
    .await
    .expect("node_add should complete");

    assert!(
        add_response.success,
        "node_add should succeed, got error: {:?}",
        add_response.error_message
    );

    assert!(node_stack.contains(TARGET_NODE_NAME, TARGET_NODE_TAG));
    assert_eq!(node_stack.len(), 2, "root + added node");
    let entity = node_stack
        .find(TARGET_NODE_NAME, TARGET_NODE_TAG)
        .expect("node should exist in stack");
    assert_eq!(entity.instances().len(), 0, "node should have no instances");

    let response = NodeRemoveRequest::new(TARGET_NODE_NAME, TARGET_NODE_TAG)
        .poll(
            &started_core_node.caller_handle,
            &started_core_node.core_node_name,
            CALLER_INSTANCE_ID,
            &started_core_node.core_node_name,
            Duration::from_secs(5),
        )
        .await
        .expect("node_remove request should complete");

    assert!(
        response.success,
        "node_remove should succeed, got error: {:?}",
        response.error_message
    );
    assert!(
        response.error_message.is_none(),
        "success response should not include error_message, got: {:?}",
        response.error_message
    );

    assert!(
        !node_stack.contains(TARGET_NODE_NAME, TARGET_NODE_TAG),
        "node should be removed from node stack"
    );
    assert_eq!(node_stack.len(), 1, "only root should remain");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_remove_node_name_not_found_fails() {
    const MISSING_NODE_NAME: &str = "missing_node";
    const MISSING_NODE_TAG: &str = "0.1.0";

    let started_core_node = start_core_node_with_mock_messenger().await;
    let node_stack = started_core_node.node_stack.clone();
    let before_len = node_stack.len();

    let response = NodeRemoveRequest::new(MISSING_NODE_NAME, MISSING_NODE_TAG)
        .poll(
            &started_core_node.caller_handle,
            &started_core_node.core_node_name,
            CALLER_INSTANCE_ID,
            &started_core_node.core_node_name,
            Duration::from_secs(5),
        )
        .await
        .expect("node_remove request should complete");

    assert!(!response.success, "node_remove should fail");
    let error_message = response
        .error_message
        .as_ref()
        .expect("failure response should include error_message");
    assert!(
        error_message.contains("not found in node stack"),
        "error should mention node not found, got: {}",
        error_message
    );
    assert!(
        error_message.contains(MISSING_NODE_NAME),
        "error should include missing node name, got: {}",
        error_message
    );

    assert_eq!(node_stack.len(), before_len, "stack should be unchanged");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_remove_stop_running_instances_first() {
    const TARGET_NODE_NAME: &str = "running_node";
    const TARGET_NODE_TAG: &str = "0.1.0";
    const TARGET_INSTANCE_ID: &str = "running_instance";

    let started_core_node = start_core_node_with_mock_messenger().await;
    let node_stack = started_core_node.node_stack.clone();

    let source_dir = tempfile::tempdir().expect("failed to create temp source dir");

    let peppy_json5 = r#"{
            schema_version: 1,
            manifest: {
                name: "{TARGET_NODE_NAME}",
                tag: "{TARGET_NODE_TAG}",
            },
            codegen: {
                language: "rust",
            },
            process: {
                start_cmd: ["sleep", "10"]
            },
            parameters: {}
        }"#
    .replace("{TARGET_NODE_NAME}", TARGET_NODE_NAME)
    .replace("{TARGET_NODE_TAG}", TARGET_NODE_TAG);
    write_peppy_json5(source_dir.path(), &peppy_json5);

    let add_response = send_node_add_and_wait(
        &started_core_node.caller_handle,
        &started_core_node.core_node_name,
        source_dir.path(),
        Duration::from_secs(5),
        Duration::from_secs(5),
        None,
    )
    .await
    .expect("node_add should complete");

    assert!(
        add_response.success,
        "node_add should succeed, got error: {:?}",
        add_response.error_message
    );

    let instance_id = Name::new(TARGET_INSTANCE_ID).expect("valid instance id");
    node_stack
        .add_instance(TARGET_NODE_NAME, TARGET_NODE_TAG, Some(&instance_id), None)
        .expect("add_instance should succeed");

    // Simulate the node exposing the shutdown service, so node_remove detects it as running.
    let shutdown_handle =
        MessengerHandle::from_shared(Arc::clone(&started_core_node.shared_messenger));
    let (shutdown_task, shutdown_rx) = listen_for_shutdown(
        &shutdown_handle,
        &started_core_node.core_node_name,
        TARGET_INSTANCE_ID,
        TARGET_NODE_NAME,
    )
    .await
    .expect("failed to start shutdown service");
    let _shutdown_task = AbortOnDrop(shutdown_task);

    // Allow the shutdown service to fully establish its listener
    tokio::time::sleep(Duration::from_millis(50)).await;

    let response = NodeRemoveRequest::new(TARGET_NODE_NAME, TARGET_NODE_TAG)
        .with_stop_instances(true)
        .poll(
            &started_core_node.caller_handle,
            &started_core_node.core_node_name,
            CALLER_INSTANCE_ID,
            &started_core_node.core_node_name,
            Duration::from_secs(10),
        )
        .await
        .expect("node_remove request should complete");

    assert!(
        response.success,
        "node_remove should succeed, got error: {:?}",
        response.error_message
    );

    tokio::time::timeout(Duration::from_millis(100), shutdown_rx)
        .await
        .expect("shutdown signal should be received within timeout")
        .expect("shutdown channel should not be dropped");

    assert!(
        !node_stack.contains(TARGET_NODE_NAME, TARGET_NODE_TAG),
        "node should be removed from node stack"
    );
    assert_eq!(node_stack.len(), 1, "only root should remain");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_fails_when_stop_instances_parameter_not_set_and_instances_exist() {
    const TARGET_NODE_NAME: &str = "running_node";
    const TARGET_NODE_TAG: &str = "0.1.0";
    const TARGET_INSTANCE_ID: &str = "running_instance";

    let started_core_node = start_core_node_with_mock_messenger().await;
    let node_stack = started_core_node.node_stack.clone();

    let source_dir = tempfile::tempdir().expect("failed to create temp source dir");

    let peppy_json5 = r#"{
            schema_version: 1,
            manifest: {
                name: "{TARGET_NODE_NAME}",
                tag: "{TARGET_NODE_TAG}",
            },
            codegen: {
                language: "rust",
            },
            process: {
                start_cmd: ["sleep", "10"]
            },
            parameters: {}
        }"#
    .replace("{TARGET_NODE_NAME}", TARGET_NODE_NAME)
    .replace("{TARGET_NODE_TAG}", TARGET_NODE_TAG);
    write_peppy_json5(source_dir.path(), &peppy_json5);

    let add_response = send_node_add_and_wait(
        &started_core_node.caller_handle,
        &started_core_node.core_node_name,
        source_dir.path(),
        Duration::from_secs(5),
        Duration::from_secs(5),
        None,
    )
    .await
    .expect("node_add should complete");

    assert!(
        add_response.success,
        "node_add should succeed, got error: {:?}",
        add_response.error_message
    );

    let instance_id = Name::new(TARGET_INSTANCE_ID).expect("valid instance id");
    node_stack
        .add_instance(TARGET_NODE_NAME, TARGET_NODE_TAG, Some(&instance_id), None)
        .expect("add_instance should succeed");

    // Simulate the node exposing the shutdown service, so node_remove detects it as running.
    let shutdown_handle =
        MessengerHandle::from_shared(Arc::clone(&started_core_node.shared_messenger));
    let (shutdown_task, shutdown_rx) = listen_for_shutdown(
        &shutdown_handle,
        &started_core_node.core_node_name,
        TARGET_INSTANCE_ID,
        TARGET_NODE_NAME,
    )
    .await
    .expect("failed to start shutdown service");
    let _shutdown_task = AbortOnDrop(shutdown_task);

    // Allow the shutdown service to fully establish its listener
    tokio::time::sleep(Duration::from_millis(50)).await;

    let before_len = node_stack.len();

    let response = NodeRemoveRequest::new(TARGET_NODE_NAME, TARGET_NODE_TAG)
        .poll(
            &started_core_node.caller_handle,
            &started_core_node.core_node_name,
            CALLER_INSTANCE_ID,
            &started_core_node.core_node_name,
            Duration::from_secs(5),
        )
        .await
        .expect("node_remove request should complete");

    assert!(
        !response.success,
        "node_remove should fail when stop_instances is not set and instances are running"
    );
    let error_message = response
        .error_message
        .as_ref()
        .expect("failure response should include error_message");
    assert!(
        error_message.contains("has running instances"),
        "error should mention running instances, got: {}",
        error_message
    );
    assert!(
        error_message.contains(TARGET_NODE_NAME),
        "error should include node name, got: {}",
        error_message
    );
    assert!(
        error_message.contains(TARGET_INSTANCE_ID),
        "error should include an example running instance id, got: {}",
        error_message
    );

    assert_eq!(node_stack.len(), before_len, "stack should be unchanged");
    assert!(
        node_stack.contains(TARGET_NODE_NAME, TARGET_NODE_TAG),
        "node should remain in node stack"
    );
    let entity = node_stack
        .find(TARGET_NODE_NAME, TARGET_NODE_TAG)
        .expect("node entity should still exist in stack");
    assert_eq!(
        entity.instances().len(),
        1,
        "instance should not be removed"
    );
    assert_eq!(
        entity.instances()[0].instance_id().as_str(),
        TARGET_INSTANCE_ID
    );

    assert!(
        tokio::time::timeout(Duration::from_millis(100), shutdown_rx)
            .await
            .is_err(),
        "shutdown service should not be invoked when stop_instances=false"
    );
}
