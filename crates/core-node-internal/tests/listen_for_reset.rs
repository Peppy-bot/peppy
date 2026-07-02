mod common;

use common::{
    AbortOnDrop, CALLER_INSTANCE_ID, NodeRunTestTimeouts, add_and_build_forking_node,
    build_staged_node, children_of, install_kill_on_shutdown_listener, instance_state_in_any_state,
    is_process_running, poll_until, send_node_add_and_wait, send_node_add_then_build,
    send_node_run_and_wait, spawn_real_running_instance, spawn_real_stuck_instance,
    start_core_node_with_mock_messenger, write_peppy_json5,
};
use config::runtime::Name;
use core_node_api::encoding::NodeResetRequest;
use peppylib::core_node::transport::poll_node_reset;
use peppylib::messaging::MessengerHandle;
use peppylib::services::health::listen_for_node_health;
use peppylib::services::ready::listen_for_node_ready;
use std::sync::Arc;
use std::time::Duration;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_reset_clears_node_stack() {
    const TARGET_NODE_A_NAME: &str = "resettable_node_a";
    const TARGET_NODE_A_TAG: &str = "v1";
    const TARGET_NODE_A_INSTANCE_ID: &str = "resettable_instance_a";

    const TARGET_NODE_B_NAME: &str = "resettable_node_b";
    const TARGET_NODE_B_TAG: &str = "v2";

    let started_core_node = start_core_node_with_mock_messenger().await;
    let node_stack = started_core_node.node_stack.clone();
    let root_instance_id_before = node_stack
        .root()
        .read()
        .instances()
        .first()
        .expect("root should have exactly one instance")
        .instance_id()
        .as_str()
        .to_owned();

    let source_dir_a = tempfile::tempdir().expect("failed to create temp source dir");
    let source_dir_b = tempfile::tempdir().expect("failed to create temp source dir");

    let peppy_json5_a = r#"{
            peppy_schema: "node/v1",
            manifest: {
                name: "{TARGET_NODE_A_NAME}",
                tag: "{TARGET_NODE_A_TAG}",
            },
            execution: {
                language: "rust",
                run_cmd: ["sleep", "10"]
            }
        }"#
    .replace("{TARGET_NODE_A_NAME}", TARGET_NODE_A_NAME)
    .replace("{TARGET_NODE_A_TAG}", TARGET_NODE_A_TAG);
    write_peppy_json5(source_dir_a.path(), &peppy_json5_a);

    let add_response_a = send_node_add_and_wait(
        &started_core_node.caller_handle,
        &started_core_node.core_node_name,
        source_dir_a.path(),
        Duration::from_secs(5),
        Duration::from_secs(5),
        None,
    )
    .await
    .expect("node_add should complete");

    assert!(
        add_response_a.success,
        "node_add should succeed, got error: {:?}",
        add_response_a.error_message
    );

    let peppy_json5_b = r#"{
            peppy_schema: "node/v1",
            manifest: {
                name: "{TARGET_NODE_B_NAME}",
                tag: "{TARGET_NODE_B_TAG}",
            },
            execution: {
                language: "rust",
                run_cmd: ["sleep", "10"]
            }
        }"#
    .replace("{TARGET_NODE_B_NAME}", TARGET_NODE_B_NAME)
    .replace("{TARGET_NODE_B_TAG}", TARGET_NODE_B_TAG);
    write_peppy_json5(source_dir_b.path(), &peppy_json5_b);

    let add_response_b = send_node_add_and_wait(
        &started_core_node.caller_handle,
        &started_core_node.core_node_name,
        source_dir_b.path(),
        Duration::from_secs(5),
        Duration::from_secs(5),
        None,
    )
    .await
    .expect("node_add should complete");

    assert!(
        add_response_b.success,
        "node_add should succeed, got error: {:?}",
        add_response_b.error_message
    );

    assert!(node_stack.contains(TARGET_NODE_A_NAME, TARGET_NODE_A_TAG));
    assert!(node_stack.contains(TARGET_NODE_B_NAME, TARGET_NODE_B_TAG));
    assert_eq!(node_stack.len(), 3, "root + two added nodes");
    build_staged_node(&started_core_node, TARGET_NODE_A_NAME, TARGET_NODE_A_TAG).await;
    build_staged_node(&started_core_node, TARGET_NODE_B_NAME, TARGET_NODE_B_TAG).await;

    let instance_id_a = Name::new(TARGET_NODE_A_INSTANCE_ID).expect("valid instance id");
    // Kept alive past the reset (not dropped, not forgotten) so that the reset,
    // not the guard's stop-on-drop, is what terminates the process.
    let running_a = spawn_real_running_instance(
        &started_core_node,
        TARGET_NODE_A_NAME,
        TARGET_NODE_A_TAG,
        &instance_id_a,
    )
    .await;
    let node_a_pid = running_a.pid;
    assert!(
        is_process_running(node_a_pid),
        "node A process {node_a_pid} should be running before reset"
    );
    let entity_a = node_stack
        .find(TARGET_NODE_A_NAME, TARGET_NODE_A_TAG)
        .expect("node A should exist in stack");
    assert_eq!(
        entity_a.read().instances().len(),
        1,
        "node A should have one instance"
    );
    drop(entity_a);

    let reset_response = poll_node_reset(
        &NodeResetRequest::new(),
        &started_core_node.caller_handle,
        &started_core_node.core_node_name,
        CALLER_INSTANCE_ID,
        &started_core_node.core_node_name,
        Duration::from_secs(5),
    )
    .await
    .expect("node_reset request should complete");

    assert!(
        reset_response.success,
        "node_reset should succeed, got error: {:?}",
        reset_response.error_message
    );
    assert!(
        reset_response.error_message.is_none(),
        "success response should not include error_message, got: {:?}",
        reset_response.error_message
    );

    assert_eq!(node_stack.len(), 1, "only root should remain");
    assert!(
        !node_stack.contains(TARGET_NODE_A_NAME, TARGET_NODE_A_TAG),
        "node A should be removed from node stack"
    );
    assert!(
        !node_stack.contains(TARGET_NODE_B_NAME, TARGET_NODE_B_TAG),
        "node B should be removed from node stack"
    );

    // The reset must terminate the running instance's process, not just drop it
    // from tracking. Before this fix the OS process was orphaned and stayed
    // alive after the stack was cleared. `running_a` is still alive here, so the
    // reset is the only thing that could have killed it.
    poll_until(
        Duration::from_secs(10),
        &format!("node A process {node_a_pid} should be terminated by reset, not orphaned"),
        || (!is_process_running(node_a_pid)).then_some(()),
    )
    .await;

    let root_after = node_stack.root();
    let root_guard = root_after.read();
    assert_eq!(
        root_guard.config().manifest.name.as_str(),
        started_core_node.core_node_name,
        "root node name should be preserved"
    );
    assert_eq!(
        root_guard.config().manifest.tag,
        started_core_node.core_node_tag,
        "root node tag should be preserved"
    );
    let root_instance_id_after = root_guard
        .instances()
        .first()
        .expect("root should have exactly one instance")
        .instance_id()
        .as_str()
        .to_owned();
    drop(root_guard);
    assert_eq!(
        root_instance_id_after, root_instance_id_before,
        "root instance id should be preserved"
    );
}

/// A `stack reset` must cooperatively-then-force terminate the running stack's
/// processes, not orphan them. This node ignores the cooperative
/// `SHUTDOWN_SERVICE` (a "stuck" instance) and forks two grandchildren in its
/// process group, so the reset must time out the graceful phase and SIGKILL the
/// whole group. Mirrors `teardown_all_instances`'s force-kill test, but drives
/// it through the `node_reset` request path that backs `peppy stack reset`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn node_reset_force_kills_whole_process_group() {
    const NODE_NAME: &str = "reset_doomed_node";
    const NODE_TAG: &str = "v1";
    const INSTANCE_ID: &str = "reset_doomed_instance";

    let started_core_node = start_core_node_with_mock_messenger().await;
    let node_stack = started_core_node.node_stack.clone();

    let _source_dir = add_and_build_forking_node(&started_core_node, NODE_NAME, NODE_TAG).await;

    let instance_id = Name::new(INSTANCE_ID).expect("valid instance id");
    // "stuck": never answers SHUTDOWN_SERVICE, so the reset must force-kill.
    let running =
        spawn_real_stuck_instance(&started_core_node, NODE_NAME, NODE_TAG, &instance_id).await;
    let node_pid = running.pid;
    // Keep `running` in scope: the reset is what kills the process group, but
    // its Drop stays as a teardown fallback if an assertion panics first. On the
    // success path the reset already reaped everything, so the guard's
    // stop-on-drop is a harmless no-op.

    poll_until(
        Duration::from_secs(5),
        "expected the node to fork two grandchildren",
        || (children_of(node_pid).len() >= 2).then_some(()),
    )
    .await;
    let grandchildren = children_of(node_pid);
    assert!(
        is_process_running(node_pid),
        "node process {node_pid} should be running before reset"
    );
    for &gc in &grandchildren {
        assert!(
            is_process_running(gc),
            "grandchild {gc} should be running before reset"
        );
    }

    // The reset request waits out the stuck node's cooperative window, then
    // force-kills its group, so the client timeout must exceed
    // force_kill_deadline(grace) + reap. Default grace is 5s.
    let reset_response = poll_node_reset(
        &NodeResetRequest::new(),
        &started_core_node.caller_handle,
        &started_core_node.core_node_name,
        CALLER_INSTANCE_ID,
        &started_core_node.core_node_name,
        Duration::from_secs(30),
    )
    .await
    .expect("node_reset request should complete");
    assert!(
        reset_response.success,
        "node_reset should succeed, got error: {:?}",
        reset_response.error_message
    );

    assert_eq!(node_stack.len(), 1, "only root should remain after reset");

    // The node and every grandchild must be gone, with no orphans left behind.
    poll_until(
        Duration::from_secs(5),
        &format!("node process {node_pid} should be gone after reset"),
        || (!is_process_running(node_pid)).then_some(()),
    )
    .await;
    for &gc in &grandchildren {
        poll_until(
            Duration::from_secs(5),
            &format!("grandchild {gc} should be gone after reset (group kill)"),
            || (!is_process_running(gc)).then_some(()),
        )
        .await;
    }

    // The root/core node, which is this test process, must be untouched.
    assert!(
        is_process_running(std::process::id()),
        "reset must never kill the root/core node (the daemon itself)"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_reset_is_idempotent() {
    let started_core_node = start_core_node_with_mock_messenger().await;
    let node_stack = started_core_node.node_stack.clone();
    assert_eq!(node_stack.len(), 1, "only root should exist initially");

    let root_instance_id_before = node_stack
        .root()
        .read()
        .instances()
        .first()
        .expect("root should have exactly one instance")
        .instance_id()
        .as_str()
        .to_owned();

    let response = poll_node_reset(
        &NodeResetRequest::new(),
        &started_core_node.caller_handle,
        &started_core_node.core_node_name,
        CALLER_INSTANCE_ID,
        &started_core_node.core_node_name,
        Duration::from_secs(5),
    )
    .await
    .expect("node_reset request should complete");

    assert!(response.success, "node_reset should succeed");
    assert_eq!(node_stack.len(), 1, "only root should remain after reset");

    let root_instance_id_after = node_stack
        .root()
        .read()
        .instances()
        .first()
        .expect("root should have exactly one instance")
        .instance_id()
        .as_str()
        .to_owned();
    assert_eq!(
        root_instance_id_after, root_instance_id_before,
        "root instance id should be preserved"
    );
}

/// `stack reset` of a node that has a LIVE exit watcher (started through the
/// real `node_run` path) must clear the stack to the root and remove the
/// instance from every lifecycle state, and the teardown force-kill must never
/// be recorded as a crash. The reset teardown claims each instance
/// (`mark_stopping`, via `collect_doomed_instances`) before signaling it, so its
/// watcher leaves removal to the teardown rather than relabeling the kill -
/// exactly the claim the launch/relaunch teardown shares. The other reset tests
/// drive instances spawned without a watcher, so this is the one that exercises
/// the watcher-versus-teardown interaction end to end.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn node_reset_with_live_exit_watcher_clears_without_recording_a_crash() {
    const TARGET_NODE_NAME: &str = "watched_resettable_node";
    const TARGET_NODE_TAG: &str = "v1";
    const TARGET_INSTANCE_ID: &str = "watched_resettable_instance";

    let started = start_core_node_with_mock_messenger().await;

    let peppy_json5 = r#"{
            peppy_schema: "node/v1",
            manifest: {
                name: "{TARGET_NODE_NAME}",
                tag: "{TARGET_NODE_TAG}",
            },
            execution: {
                language: "rust",
                run_cmd: ["sleep", "300"]
            }
        }"#
    .replace("{TARGET_NODE_NAME}", TARGET_NODE_NAME)
    .replace("{TARGET_NODE_TAG}", TARGET_NODE_TAG);
    let source_dir = tempfile::tempdir().expect("failed to create temp source dir");
    write_peppy_json5(source_dir.path(), &peppy_json5);

    let add_response = send_node_add_then_build(
        &started.caller_handle,
        &started.core_node_name,
        source_dir.path(),
        Duration::from_secs(5),
        Duration::from_secs(5),
    )
    .await
    .expect("node_add should succeed");
    assert!(
        add_response.success,
        "node_add should succeed, got error: {:?}",
        add_response.error_message
    );

    // Mocks satisfy the readiness gate so the real node_run path commits the
    // instance and spawns its exit watcher.
    let node_messenger = MessengerHandle::from_shared(Arc::clone(&started.shared_messenger));
    let _ready_task = AbortOnDrop(
        listen_for_node_ready(
            &node_messenger,
            &started.core_node_name,
            TARGET_INSTANCE_ID,
            common::test_node_target(TARGET_NODE_NAME),
        )
        .await
        .expect("node ready service should start"),
    );
    let _health_task = AbortOnDrop(
        listen_for_node_health(
            &node_messenger,
            &started.core_node_name,
            TARGET_INSTANCE_ID,
            common::test_node_target(TARGET_NODE_NAME),
        )
        .await
        .expect("node health service should start"),
    );
    tokio::time::sleep(Duration::from_millis(50)).await;

    let runtime_config_json5 = common::default_runtime_config_json5(
        &started.core_node_name,
        TARGET_NODE_NAME,
        TARGET_NODE_TAG,
        TARGET_INSTANCE_ID,
    );
    let start_response = send_node_run_and_wait(
        &started.caller_handle,
        &started.core_node_name,
        &runtime_config_json5,
        TARGET_NODE_NAME,
        TARGET_NODE_TAG,
        &NodeRunTestTimeouts {
            goal: Duration::from_secs(10),
            result: Duration::from_secs(30),
        },
        None,
    )
    .await
    .expect("node_run action should complete");
    assert!(
        start_response.result.success,
        "node_run should succeed, got error: {:?}",
        start_response.result.error_message
    );
    let pid = start_response
        .result
        .pid
        .expect("should have a PID on success");
    let instance_id = Name::new(TARGET_INSTANCE_ID).expect("valid instance id");

    // Bridge the cooperative shutdown to a real kill so the reset completes fast
    // instead of waiting out the force-kill deadline; the kill lands strictly
    // after the teardown's `mark_stopping` claim.
    let _kill_on_shutdown =
        install_kill_on_shutdown_listener(&started, TARGET_NODE_NAME, &instance_id, pid).await;

    let reset_response = poll_node_reset(
        &NodeResetRequest::new(),
        &started.caller_handle,
        &started.core_node_name,
        CALLER_INSTANCE_ID,
        &started.core_node_name,
        Duration::from_secs(30),
    )
    .await
    .expect("node_reset request should complete");
    assert!(
        reset_response.success,
        "node_reset should succeed, got error: {:?}",
        reset_response.error_message
    );

    assert_eq!(
        started.node_stack.len(),
        1,
        "only the root should remain after reset"
    );
    poll_until(
        Duration::from_secs(5),
        "node process should be gone after reset",
        || (!is_process_running(pid)).then_some(()),
    )
    .await;

    // The instance is removed from EVERY state, not left as a terminal record.
    assert!(
        instance_state_in_any_state(&started.node_stack, &instance_id).is_none(),
        "the reset instance must be fully removed, not lingering as a terminal state"
    );

    // The teardown claim made the watcher leave the kill alone, so no crash was
    // recorded for this instance. A regression that dropped the claim from
    // `collect_doomed_instances` would let the watcher relabel the force-kill as
    // `failed` in the stack log.
    let log_content =
        std::fs::read_to_string(started.peppy_dirs.stack_log_path()).unwrap_or_default();
    assert!(
        !log_content
            .lines()
            .any(|line| line.contains(TARGET_INSTANCE_ID) && line.contains("failed")),
        "a reset teardown must never be recorded as a crash, got:
{}",
        log_content
    );
}
