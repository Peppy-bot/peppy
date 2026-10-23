use super::*;

/// An HTTP server that accepts each connection and never answers it: an
/// add that downloads its source from it stays in its download until it is
/// cancelled. The guard stops the server on drop.
async fn stalled_http_server() -> (url::Url, AbortOnDrop<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("the stalled server binds");
    let address = listener
        .local_addr()
        .expect("the stalled server has an address");
    let server = peppylib::runtime::spawn(async move {
        let mut held = Vec::new();
        while let Ok((connection, _)) = listener.accept().await {
            held.push(connection);
        }
    });
    let url = url::Url::parse(&format!("http://{address}/node.tar.zst"))
        .expect("the stalled server URL parses");
    (url, AbortOnDrop(server))
}

/// Sends a `node_add` goal whose download never ends, and returns as soon as
/// the daemon accepts it: the add holds the action until a forced add
/// supersedes it. The test holds the returned guard and action handle until
/// it ends.
async fn send_stalled_add_goal(
    started_core_node: &common::StartedCoreNode,
) -> (AbortOnDrop<()>, peppylib::messaging::ActionGoalHandle) {
    let (url, server) = stalled_http_server().await;
    let action_handle = common::send_daemon_goal(
        &started_core_node.caller_handle,
        &started_core_node.core_node_name,
        &NodeAddGoal::new_http(url, None, TEST_GIT_HASH, RESULT_TIMEOUT.as_secs()),
        GOAL_TIMEOUT,
    )
    .await
    .expect("first goal should be sent");
    assert!(
        action_handle.goal_reply().is_accepted(),
        "first goal should be accepted: {:?}",
        action_handle.goal_reply().refusal()
    );
    (server, action_handle)
}

/// A node config whose add completes: its build and run commands are never
/// run by the add.
fn fast_node_source(node_name: &str, node_tag: &str) -> TempDir {
    let source_dir = tempfile::tempdir().expect("failed to create temp source dir");
    let peppy_json5 = node_config_with_execution(
        node_name,
        node_tag,
        r#"{ language: "rust", build_cmd: ["true"], run_cmd: ["true"] }"#,
    );
    write_peppy_json5(source_dir.path(), &peppy_json5);
    source_dir
}

/// Tests that the daemon refuses a second goal when an action is already in
/// progress, and that the refusal suggests using `--force`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_add_rejects_second_goal_when_action_in_progress() {
    let started_core_node = start_core_node_with_mock_messenger().await;

    let (_server, _first_action_handle) = send_stalled_add_goal(&started_core_node).await;
    let source_dir = fast_node_source("second_add_node", "v1");

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

/// A forced add supersedes the add in progress: the forced add completes,
/// and the superseded one ends cancelled with the reason.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_add_force_overrides_in_progress_action() {
    const SECOND_NODE_NAME: &str = "force_add_node";
    const SECOND_NODE_TAG: &str = "v1";

    let started_core_node = start_core_node_with_mock_messenger().await;
    let node_stack = started_core_node.node_stack.clone();

    let (_server, mut first_action_handle) = send_stalled_add_goal(&started_core_node).await;
    let fast_source_dir = fast_node_source(SECOND_NODE_NAME, SECOND_NODE_TAG);

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

    wait_until_action_completes(&mut first_action_handle, RESULT_TIMEOUT).await;
    let first_end = common::fetch_end(
        &started_core_node.caller_handle,
        &first_action_handle,
        GOAL_TIMEOUT,
        NodeAddResult::decode,
        "result",
    )
    .await
    .expect("the superseded add ends");
    assert_eq!(first_end.cancel_reason(), "superseded by a forced node add");
}
