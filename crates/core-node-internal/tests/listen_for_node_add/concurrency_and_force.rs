use super::*;
use peppylib::messaging::Iface;

/// Tests that a second goal is rejected when an action is already in progress,
/// and that the rejection message suggests using `--force`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_add_rejects_second_goal_when_action_in_progress() {
    let started_core_node = start_core_node_with_mock_messenger().await;

    // The add action stays in Running state while it copies the source
    // directory and processes the node config. build_cmd is not executed
    // during the add phase, so we use a benign no-op here.
    let source_dir = tempfile::tempdir().expect("failed to create temp source dir");
    let peppy_json5 = node_config_with_execution(
        "slow_add_node",
        "v1",
        r#"{ language: "rust", build_cmd: ["true"], run_cmd: ["true"] }"#,
    );
    write_peppy_json5(source_dir.path(), &peppy_json5);

    // Create the .peppy/git.hash file so the first goal's background task does
    // not fail fast on git-hash verification (which would transition the action
    // state from Running → Completed before the second goal arrives, making the
    // rejection check non-deterministic).
    let peppy_dir = source_dir.path().join(PEPPY_OUTPUT_DIR);
    std::fs::create_dir_all(&peppy_dir).expect("failed to create .peppy dir");
    std::fs::write(peppy_dir.join("git.hash"), TEST_GIT_HASH).expect("failed to write git.hash");

    // Send first goal — should be accepted and start running the add.
    let first_goal = NodeAddGoal::new(source_dir.path(), TEST_GIT_HASH, RESULT_TIMEOUT.as_secs());
    let first_goal_payload = first_goal.encode().expect("failed to encode goal");

    let first_action_handle = ActionMessenger::send_goal(
        &started_core_node.caller_handle,
        &started_core_node.core_node_name,
        CALLER_INSTANCE_ID,
        &started_core_node.core_node_name,
        Iface::native(),
        names::NODE_ADD_ACTION,
        Some(&started_core_node.core_node_name),
        None,
        first_goal_payload,
        QoSProfile::default(),
        GOAL_TIMEOUT,
    )
    .await
    .expect("first goal should be sent");

    let first_response_payload = first_action_handle.goal_response().payload();
    let first_response = NodeAddGoalResponse::decode(&first_response_payload)
        .expect("failed to decode first goal response");
    assert!(first_response.accepted, "first goal should be accepted");

    // Send second goal (no force) — should be rejected.
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

    assert!(
        !second_result.success,
        "second goal without --force should fail"
    );
    let error_msg = second_result
        .error_message
        .as_deref()
        .expect("rejection should have an error message");
    assert!(
        error_msg.contains("action already in progress"),
        "error should mention action in progress, got: {error_msg}"
    );
    assert!(
        error_msg.contains("--force"),
        "error should suggest --force, got: {error_msg}"
    );
}

/// Tests that `--force` aborts an in-progress action and starts a new one.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_add_force_overrides_in_progress_action() {
    const SECOND_NODE_NAME: &str = "force_add_node";
    const SECOND_NODE_TAG: &str = "v1";

    let started_core_node = start_core_node_with_mock_messenger().await;
    let node_stack = started_core_node.node_stack.clone();

    // The first add action stays in Running state while it copies the source
    // directory and processes the node config. build_cmd is not executed
    // during the add phase, so we use a benign no-op here.
    let slow_source_dir = tempfile::tempdir().expect("failed to create temp source dir");
    let slow_peppy_json5 = node_config_with_execution(
        "slow_node",
        "v1",
        r#"{ language: "rust", build_cmd: ["true"], run_cmd: ["true"] }"#,
    );
    write_peppy_json5(slow_source_dir.path(), &slow_peppy_json5);

    // Create the .peppy/git.hash file so the first goal's background task does
    // not fail fast on git-hash verification (same race as the rejection test).
    let peppy_dir = slow_source_dir.path().join(PEPPY_OUTPUT_DIR);
    std::fs::create_dir_all(&peppy_dir).expect("failed to create .peppy dir");
    std::fs::write(peppy_dir.join("git.hash"), TEST_GIT_HASH).expect("failed to write git.hash");

    // Send first goal — starts the add.
    let first_goal = NodeAddGoal::new(
        slow_source_dir.path(),
        TEST_GIT_HASH,
        RESULT_TIMEOUT.as_secs(),
    );
    let first_goal_payload = first_goal.encode().expect("failed to encode goal");

    let first_action_handle = ActionMessenger::send_goal(
        &started_core_node.caller_handle,
        &started_core_node.core_node_name,
        CALLER_INSTANCE_ID,
        &started_core_node.core_node_name,
        Iface::native(),
        names::NODE_ADD_ACTION,
        Some(&started_core_node.core_node_name),
        None,
        first_goal_payload,
        QoSProfile::default(),
        GOAL_TIMEOUT,
    )
    .await
    .expect("first goal should be sent");

    let first_response_payload = first_action_handle.goal_response().payload();
    let first_response = NodeAddGoalResponse::decode(&first_response_payload)
        .expect("failed to decode first goal response");
    assert!(first_response.accepted, "first goal should be accepted");

    // Create a fast node for the second goal.
    let fast_source_dir = tempfile::tempdir().expect("failed to create temp source dir");
    let fast_peppy_json5 = node_config_with_execution(
        SECOND_NODE_NAME,
        SECOND_NODE_TAG,
        r#"{ language: "rust", build_cmd: ["true"], run_cmd: ["true"] }"#,
    );
    write_peppy_json5(fast_source_dir.path(), &fast_peppy_json5);

    // Send second goal with force — should abort the slow action and succeed.
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

    assert!(
        second_result.success,
        "force node_add should succeed, got error: {:?}",
        second_result.error_message
    );

    assert!(
        node_stack.contains(SECOND_NODE_NAME, SECOND_NODE_TAG),
        "force-added node should be in the stack"
    );
}
