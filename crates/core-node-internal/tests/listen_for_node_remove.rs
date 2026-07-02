mod common;

use common::{
    AbortOnDrop, CALLER_INSTANCE_ID, build_staged_node, send_node_add_and_wait,
    spawn_real_running_instance, start_core_node_with_mock_messenger, write_peppy_json5,
};
use config::runtime::Name;
use core_node_api::encoding::NodeRemoveRequest;
use peppylib::core_node::transport::poll_node_remove;
use peppylib::messaging::MessengerHandle;
use peppylib::services::shutdown::listen_for_shutdown;
use std::sync::Arc;
use std::time::Duration;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_remove_success() {
    const TARGET_NODE_NAME: &str = "removable_node";
    const TARGET_NODE_TAG: &str = "v1";

    let started_core_node = start_core_node_with_mock_messenger().await;
    let node_stack = started_core_node.node_stack.clone();

    let source_dir = tempfile::tempdir().expect("failed to create temp source dir");

    let peppy_json5 = r#"{
            peppy_schema: "node/v1",
            manifest: {
                name: "{TARGET_NODE_NAME}",
                tag: "{TARGET_NODE_TAG}",
            },
            execution: {
                language: "rust",
                run_cmd: ["sleep", "10"]
            }
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
    assert_eq!(
        entity.read().instances().len(),
        0,
        "node should have no instances"
    );

    let response = poll_node_remove(
        &NodeRemoveRequest::new(TARGET_NODE_NAME, TARGET_NODE_TAG),
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
    const MISSING_NODE_TAG: &str = "v1";

    let started_core_node = start_core_node_with_mock_messenger().await;
    let node_stack = started_core_node.node_stack.clone();
    let before_len = node_stack.len();

    let response = poll_node_remove(
        &NodeRemoveRequest::new(MISSING_NODE_NAME, MISSING_NODE_TAG),
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
    const TARGET_NODE_TAG: &str = "v1";
    const TARGET_INSTANCE_ID: &str = "running_instance";

    let started_core_node = start_core_node_with_mock_messenger().await;
    let node_stack = started_core_node.node_stack.clone();

    let source_dir = tempfile::tempdir().expect("failed to create temp source dir");

    let peppy_json5 = r#"{
            peppy_schema: "node/v1",
            manifest: {
                name: "{TARGET_NODE_NAME}",
                tag: "{TARGET_NODE_TAG}",
            },
            execution: {
                language: "rust",
                run_cmd: ["sleep", "10"]
            }
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
    build_staged_node(&started_core_node, TARGET_NODE_NAME, TARGET_NODE_TAG).await;

    let instance_id = Name::new(TARGET_INSTANCE_ID).expect("valid instance id");
    let _running = spawn_real_running_instance(
        &started_core_node,
        TARGET_NODE_NAME,
        TARGET_NODE_TAG,
        &instance_id,
    )
    .await;

    // Simulate the node exposing the shutdown service, so node_remove detects it as running.
    let shutdown_handle =
        MessengerHandle::from_shared(Arc::clone(&started_core_node.shared_messenger));
    let (shutdown_task, shutdown_rx) = listen_for_shutdown(
        &shutdown_handle,
        &started_core_node.core_node_name,
        TARGET_INSTANCE_ID,
        common::test_node_target(TARGET_NODE_NAME),
    )
    .await
    .expect("failed to start shutdown service");
    let _shutdown_task = AbortOnDrop(shutdown_task);

    // Allow the shutdown service to fully establish its listener
    tokio::time::sleep(Duration::from_millis(50)).await;

    let response = poll_node_remove(
        &NodeRemoveRequest::new(TARGET_NODE_NAME, TARGET_NODE_TAG).with_stop_instances(true),
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

/// A one-shot (or crashed) node whose instance exited on its own sits in a
/// terminal state but stays tracked. `node remove` must still clear it: the gate
/// counts it (so a plain remove is rejected), and with `stop_instances` it is
/// removed and the node config goes with it. Regression guard — before the fix
/// the remove collector ignored non-`Running` instances, so a terminal instance
/// was never cleared and `remove_config` rejected the node as "still has
/// instances", making a finished node impossible to remove.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_remove_clears_a_terminal_instance() {
    const TARGET_NODE_NAME: &str = "finished_node";
    const TARGET_NODE_TAG: &str = "v1";
    const TARGET_INSTANCE_ID: &str = "finished_instance";

    let started_core_node = start_core_node_with_mock_messenger().await;
    let node_stack = started_core_node.node_stack.clone();

    let source_dir = tempfile::tempdir().expect("failed to create temp source dir");
    let peppy_json5 = r#"{
            peppy_schema: "node/v1",
            manifest: {
                name: "{TARGET_NODE_NAME}",
                tag: "{TARGET_NODE_TAG}",
            },
            execution: {
                language: "rust",
                run_cmd: ["sleep", "10"]
            }
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
    build_staged_node(&started_core_node, TARGET_NODE_NAME, TARGET_NODE_TAG).await;

    let instance_id = Name::new(TARGET_INSTANCE_ID).expect("valid instance id");
    // Keep the guard so the real `sleep 10` child is reaped on drop; the
    // instance's *tracked* state is driven terminal below independent of the
    // (still-alive) process, which is exactly the state the exit watcher leaves
    // behind when a node's process exits on its own.
    let _running = spawn_real_running_instance(
        &started_core_node,
        TARGET_NODE_NAME,
        TARGET_NODE_TAG,
        &instance_id,
    )
    .await;

    // Drive the instance terminal, as the exit watcher would on a clean exit.
    let entity = node_stack
        .find(TARGET_NODE_NAME, TARGET_NODE_TAG)
        .expect("node should exist in stack");
    let new_state = node_stack::NodeEntity::mark_instance_exited(&entity, &instance_id, true);
    assert_eq!(
        new_state,
        Some(core_node_api::InstanceState::Finished),
        "a clean self-exit should leave the instance Finished"
    );

    // Without `stop_instances`, the tracked terminal instance still gates the remove.
    let gated = poll_node_remove(
        &NodeRemoveRequest::new(TARGET_NODE_NAME, TARGET_NODE_TAG),
        &started_core_node.caller_handle,
        &started_core_node.core_node_name,
        CALLER_INSTANCE_ID,
        &started_core_node.core_node_name,
        Duration::from_secs(5),
    )
    .await
    .expect("node_remove request should complete");
    assert!(
        !gated.success,
        "a tracked terminal instance must gate a plain remove"
    );
    assert!(
        node_stack.contains(TARGET_NODE_NAME, TARGET_NODE_TAG),
        "node must still be present after the gated remove"
    );

    // With `stop_instances`, the terminal instance is cleared and the node removed.
    let response = poll_node_remove(
        &NodeRemoveRequest::new(TARGET_NODE_NAME, TARGET_NODE_TAG).with_stop_instances(true),
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
        "node_remove should clear a terminal instance, got error: {:?}",
        response.error_message
    );
    assert!(
        !node_stack.contains(TARGET_NODE_NAME, TARGET_NODE_TAG),
        "node should be removed from the stack"
    );
    assert_eq!(node_stack.len(), 1, "only root should remain");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_fails_when_stop_instances_parameter_not_set_and_instances_exist() {
    const TARGET_NODE_NAME: &str = "running_node";
    const TARGET_NODE_TAG: &str = "v1";
    const TARGET_INSTANCE_ID: &str = "running_instance";

    let started_core_node = start_core_node_with_mock_messenger().await;
    let node_stack = started_core_node.node_stack.clone();

    let source_dir = tempfile::tempdir().expect("failed to create temp source dir");

    let peppy_json5 = r#"{
            peppy_schema: "node/v1",
            manifest: {
                name: "{TARGET_NODE_NAME}",
                tag: "{TARGET_NODE_TAG}",
            },
            execution: {
                language: "rust",
                run_cmd: ["sleep", "10"]
            }
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
    build_staged_node(&started_core_node, TARGET_NODE_NAME, TARGET_NODE_TAG).await;

    let instance_id = Name::new(TARGET_INSTANCE_ID).expect("valid instance id");
    let _running = spawn_real_running_instance(
        &started_core_node,
        TARGET_NODE_NAME,
        TARGET_NODE_TAG,
        &instance_id,
    )
    .await;

    // Simulate the node exposing the shutdown service, so node_remove detects it as running.
    let shutdown_handle =
        MessengerHandle::from_shared(Arc::clone(&started_core_node.shared_messenger));
    let (shutdown_task, shutdown_rx) = listen_for_shutdown(
        &shutdown_handle,
        &started_core_node.core_node_name,
        TARGET_INSTANCE_ID,
        common::test_node_target(TARGET_NODE_NAME),
    )
    .await
    .expect("failed to start shutdown service");
    let _shutdown_task = AbortOnDrop(shutdown_task);

    // Allow the shutdown service to fully establish its listener
    tokio::time::sleep(Duration::from_millis(50)).await;

    let before_len = node_stack.len();

    let response = poll_node_remove(
        &NodeRemoveRequest::new(TARGET_NODE_NAME, TARGET_NODE_TAG),
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
        error_message.contains("has tracked instances"),
        "error should mention tracked instances, got: {}",
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
    {
        let entity_guard = entity.read();
        assert_eq!(
            entity_guard.instances().len(),
            1,
            "instance should not be removed"
        );
        assert_eq!(
            entity_guard.instances()[0].instance_id().as_str(),
            TARGET_INSTANCE_ID
        );
    }

    assert!(
        tokio::time::timeout(Duration::from_millis(100), shutdown_rx)
            .await
            .is_err(),
        "shutdown service should not be invoked when stop_instances=false"
    );
}
