mod common;

use common::{
    AbortOnDrop, GOAL_TIMEOUT, RESULT_TIMEOUT, assert_failure, build_staged_node,
    install_shutdown_listener_for_instance, send_node_add_and_wait, spawn_real_running_instance,
    spawn_real_stuck_instance, start_core_node_with_mock_messenger, wait_until_service_reachable,
    write_peppy_json5,
};
use config::failure_kind::PeppyFailureKind;
use config::runtime::Name;
use core_node_api::encoding::NodeRemoveRequest;
use peppylib::messaging::MessengerHandle;
use peppylib::services::shutdown::listen_for_shutdown;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::oneshot::error::TryRecvError;

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
        GOAL_TIMEOUT,
        RESULT_TIMEOUT,
        None,
    )
    .await
    .expect("node_add should complete");

    add_response.completed();

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

    common::poll_core_node(
        &started_core_node,
        &NodeRemoveRequest::new(TARGET_NODE_NAME, TARGET_NODE_TAG),
    )
    .await;

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

    let failure = common::poll_core_node_failure(
        &started_core_node,
        &NodeRemoveRequest::new(MISSING_NODE_NAME, MISSING_NODE_TAG),
    )
    .await;

    assert_failure(
        &failure,
        PeppyFailureKind::Rejected,
        "not found in node stack",
    );
    assert_failure(&failure, PeppyFailureKind::Rejected, MISSING_NODE_NAME);

    assert_eq!(node_stack.len(), before_len, "stack should be unchanged");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_remove_refuses_the_core_node() {
    let started_core_node = start_core_node_with_mock_messenger().await;
    let node_stack = started_core_node.node_stack.clone();
    let before_len = node_stack.len();
    let (root_name, root_tag) = {
        let root = node_stack.root();
        let guard = root.read();
        (
            guard.config().manifest.name.as_str().to_owned(),
            guard.config().manifest.tag.clone(),
        )
    };

    let failure = common::poll_core_node_failure(
        &started_core_node,
        &NodeRemoveRequest::new(&root_name, &root_tag).with_stop_instances(true),
    )
    .await;

    common::assert_failure_kind(&failure, PeppyFailureKind::Rejected);
    assert_eq!(
        failure.message(),
        "Cannot remove the core node from the node stack"
    );
    assert!(
        node_stack.contains(&root_name, &root_tag),
        "the core node should still be in the stack"
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
        GOAL_TIMEOUT,
        RESULT_TIMEOUT,
        None,
    )
    .await
    .expect("node_add should complete");

    add_response.completed();
    build_staged_node(&started_core_node, TARGET_NODE_NAME, TARGET_NODE_TAG).await;

    let instance_id = Name::new(TARGET_INSTANCE_ID).expect("valid instance id");
    let _running = spawn_real_stuck_instance(
        &started_core_node,
        TARGET_NODE_NAME,
        TARGET_NODE_TAG,
        &instance_id,
    )
    .await;

    // Serve the node's shutdown service: node_remove probes it to tell a
    // running instance from an unreachable one, and the removal waits for the
    // process once the request is answered. The stuck variant above installs
    // no shutdown listener, so this one is the only SHUTDOWN_SERVICE responder
    // of the instance and sees every shutdown request node_remove sends; it
    // ends the process as a node does, so the removal returns at once with
    // nothing force-killed.
    let (_shutdown_task, shutdown_rx) = install_shutdown_listener_for_instance(
        &started_core_node,
        TARGET_NODE_NAME,
        TARGET_NODE_TAG,
        TARGET_INSTANCE_ID,
    )
    .await;
    let shutdown_handle =
        MessengerHandle::from_shared(Arc::clone(&started_core_node.shared_messenger));
    wait_until_service_reachable(
        &shutdown_handle,
        &started_core_node.core_node_name,
        TARGET_NODE_NAME,
        peppylib::messaging::SHUTDOWN_SERVICE,
        &started_core_node.core_node_name,
        TARGET_INSTANCE_ID,
        Duration::from_secs(5),
    )
    .await;

    let response = common::poll_core_node_with_timeout(
        &started_core_node,
        &NodeRemoveRequest::new(TARGET_NODE_NAME, TARGET_NODE_TAG).with_stop_instances(true),
        core_node::node_remove_bound(node_stack.shutdown_grace()) + Duration::from_secs(5),
    )
    .await;

    assert!(
        response.force_killed.is_empty(),
        "the instance exited on its shutdown request, got force-killed: {:?}",
        response.force_killed
    );

    // node_remove answers only after the shutdown request got its answer,
    // which the listener reports once it ended the process.
    tokio::time::timeout(Duration::from_secs(1), shutdown_rx)
        .await
        .expect("the shutdown listener reports the request the removal sent")
        .expect("the shutdown listener is still running");

    assert!(
        !node_stack.contains(TARGET_NODE_NAME, TARGET_NODE_TAG),
        "node should be removed from node stack"
    );
    assert_eq!(node_stack.len(), 1, "only root should remain");
}

/// A one-shot (or crashed) node whose instance exited on its own sits in a
/// terminal state but stays tracked. `node remove` must still clear it: the gate
/// counts it (so a plain remove is rejected), and with `stop_instances` it is
/// removed and the node config goes with it. A remove collector that skipped
/// non-`Running` instances would never clear a terminal instance, and
/// `remove_config` would reject the node as "still has instances", making a
/// finished node impossible to remove.
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
        GOAL_TIMEOUT,
        RESULT_TIMEOUT,
        None,
    )
    .await
    .expect("node_add should complete");
    add_response.completed();
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
    let pid = node_stack
        .find_instance_and_entity_any_state(&instance_id)
        .and_then(|(instance, _)| instance.pid())
        .expect("a running instance has a pid");
    let exited = node_stack::NodeEntity::mark_instance_exited(
        &entity,
        &instance_id,
        pid,
        core_node_api::ExitCause::Code(0),
        &core_node::test_support::instant_restart_backoff(),
        node_stack::Registrations::default,
    );
    assert!(exited.is_some(), "a running instance exits");
    assert_eq!(
        node_stack.instance_state(instance_id.as_str()),
        Some(core_node_api::InstanceState::Finished),
        "a clean self-exit should leave the instance Finished"
    );

    // Without `stop_instances`, the tracked terminal instance still gates the remove.
    let gated = common::poll_core_node_failure(
        &started_core_node,
        &NodeRemoveRequest::new(TARGET_NODE_NAME, TARGET_NODE_TAG),
    )
    .await;
    assert_failure(
        &gated,
        PeppyFailureKind::PreconditionFailed,
        "has tracked instances",
    );
    assert!(
        node_stack.contains(TARGET_NODE_NAME, TARGET_NODE_TAG),
        "node must still be present after the gated remove"
    );

    // With `stop_instances`, the terminal instance is cleared and the node removed.
    common::poll_core_node_with_timeout(
        &started_core_node,
        &NodeRemoveRequest::new(TARGET_NODE_NAME, TARGET_NODE_TAG).with_stop_instances(true),
        Duration::from_secs(10),
    )
    .await;
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
        GOAL_TIMEOUT,
        RESULT_TIMEOUT,
        None,
    )
    .await
    .expect("node_add should complete");

    add_response.completed();
    build_staged_node(&started_core_node, TARGET_NODE_NAME, TARGET_NODE_TAG).await;

    let instance_id = Name::new(TARGET_INSTANCE_ID).expect("valid instance id");
    let _running = spawn_real_stuck_instance(
        &started_core_node,
        TARGET_NODE_NAME,
        TARGET_NODE_TAG,
        &instance_id,
    )
    .await;

    // Simulate the node exposing the shutdown service: node_remove probes it to
    // tell a running instance from an unreachable one. The stuck variant above
    // installs no shutdown listener, so this one is the only SHUTDOWN_SERVICE
    // responder of the instance and sees every shutdown request node_remove
    // sends.
    let shutdown_handle =
        MessengerHandle::from_shared(Arc::clone(&started_core_node.shared_messenger));
    let (shutdown_task, mut shutdown_rx) = listen_for_shutdown(
        &shutdown_handle,
        &started_core_node.core_node_name,
        TARGET_INSTANCE_ID,
        common::test_node_target(TARGET_NODE_NAME),
    )
    .await
    .expect("failed to start shutdown service");
    let _shutdown_task = AbortOnDrop(shutdown_task);
    wait_until_service_reachable(
        &shutdown_handle,
        &started_core_node.core_node_name,
        TARGET_NODE_NAME,
        peppylib::messaging::SHUTDOWN_SERVICE,
        &started_core_node.core_node_name,
        TARGET_INSTANCE_ID,
        Duration::from_secs(5),
    )
    .await;

    let before_len = node_stack.len();

    // `stop_instances` is not set and instances run, so the remove fails.
    let failure = common::poll_core_node_failure(
        &started_core_node,
        &NodeRemoveRequest::new(TARGET_NODE_NAME, TARGET_NODE_TAG),
    )
    .await;

    for text in [
        "has tracked instances",
        TARGET_NODE_NAME,
        TARGET_INSTANCE_ID,
    ] {
        assert_failure(&failure, PeppyFailureKind::PreconditionFailed, text);
    }

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

    // node_remove rejects the request before it sends any shutdown request.
    assert!(
        matches!(shutdown_rx.try_recv(), Err(TryRecvError::Empty)),
        "shutdown service should not be invoked when stop_instances=false"
    );
}
