use super::*;

/// Sends a `node_add` goal for a new `node_name` source and returns as soon as
/// the daemon accepts it, so the test can send a second goal while this add is
/// still in progress. The test holds the returned source dir and action handle
/// until it ends.
async fn send_add_goal_without_waiting(
    started_core_node: &common::StartedCoreNode,
    node_name: &str,
) -> (TempDir, peppylib::messaging::ActionGoalHandle) {
    // The add action stays in Running state while it copies the source
    // directory and processes the node config. build_cmd is not executed
    // during the add phase, so we use a benign no-op here.
    let source_dir = tempfile::tempdir().expect("failed to create temp source dir");
    let peppy_json5 = node_config_with_execution(
        node_name,
        "v1",
        r#"{ language: "rust", build_cmd: ["true"], run_cmd: ["true"] }"#,
    );
    write_peppy_json5(source_dir.path(), &peppy_json5);

    // The recorded git hash keeps the first goal's background task from
    // failing fast on git-hash verification, which would end the add before
    // the second goal arrives.
    let action_handle = common::send_daemon_goal(
        &started_core_node.caller_handle,
        &started_core_node.core_node_name,
        &fs_add_goal(source_dir.path()),
        GOAL_TIMEOUT,
    )
    .await
    .expect("first goal should be sent");
    assert!(
        action_handle.goal_reply().is_accepted(),
        "first goal should be accepted: {:?}",
        action_handle.goal_reply().refusal()
    );
    (source_dir, action_handle)
}

/// Tests that the daemon refuses a second goal when an action is already in
/// progress, and that the refusal suggests using `--force`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_add_rejects_second_goal_when_action_in_progress() {
    let started_core_node = start_core_node_with_mock_messenger().await;

    let (source_dir, _first_action_handle) =
        send_add_goal_without_waiting(&started_core_node, "slow_add_node").await;

    // Send second goal (no force); the daemon refuses it.
    let second_result = send_node_add_and_wait(
        &started_core_node.caller_handle,
        &started_core_node.core_node_name,
        source_dir.path(),
        GOAL_TIMEOUT,
        RESULT_TIMEOUT,
        None,
    )
    .await
    .expect("second node_add request should complete");

    let refusal = second_result.refusal();
    assert_failure(
        refusal,
        PeppyFailureKind::PreconditionFailed,
        "action already in progress",
    );
    assert!(
        refusal.message().contains("--force"),
        "the refusal should suggest --force, got: {refusal}"
    );
}

/// Tests that `--force` aborts an in-progress action and starts a new one.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_add_force_overrides_in_progress_action() {
    const SECOND_NODE_NAME: &str = "force_add_node";
    const SECOND_NODE_TAG: &str = "v1";

    let started_core_node = start_core_node_with_mock_messenger().await;
    let node_stack = started_core_node.node_stack.clone();

    let (_slow_source_dir, _first_action_handle) =
        send_add_goal_without_waiting(&started_core_node, "slow_node").await;

    // Create a fast node for the second goal.
    let fast_source_dir = tempfile::tempdir().expect("failed to create temp source dir");
    let fast_peppy_json5 = node_config_with_execution(
        SECOND_NODE_NAME,
        SECOND_NODE_TAG,
        r#"{ language: "rust", build_cmd: ["true"], run_cmd: ["true"] }"#,
    );
    write_peppy_json5(fast_source_dir.path(), &fast_peppy_json5);

    // Send second goal with force; should abort the slow action and succeed.
    let second_result = send_node_add_and_wait_with_force(
        &started_core_node.caller_handle,
        &started_core_node.core_node_name,
        fast_source_dir.path(),
        GOAL_TIMEOUT,
        RESULT_TIMEOUT,
        None,
    )
    .await
    .expect("force node_add request should complete");

    second_result.completed();

    assert!(
        node_stack.contains(SECOND_NODE_NAME, SECOND_NODE_TAG),
        "force-added node should be in the stack"
    );
}
