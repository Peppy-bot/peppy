//! Feedback of a daemon-hosted action goal sent through
//! `core_node::transport::send_goal`: the caller keeps every line
//! (`DAEMON_GOAL_FEEDBACK`).

use std::time::Duration;

use core_node_api::ActionGoal;
use core_node_api::encoding::RepoRefreshGoal;
use peppylib::core_node::transport;
use peppylib::messaging::{ActionMessenger, ResultStatus};
use peppylib::testing::EphemeralRouter;

use super::common::{CLIENT_INSTANCE, CORE_NODE, SERVER_INSTANCE, test_node_target};
use super::feedback_flood::{
    FLOOD_RESULT, FloodedAction, HANG_GUARD, read_to_the_end, start_flooding_provider,
};

/// More lines than a session's default feedback buffer holds.
const LINE_COUNT: usize = 300;
const REPLY_TIMEOUT: Duration = Duration::from_secs(10);

/// The caller reads the feedback only after the goal ended, and still gets
/// every line, in order.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_daemon_goal_keeps_every_feedback_line_for_its_caller() {
    let router = EphemeralRouter::start().await.expect("start zenoh router");
    let server = router.connect().await.expect("server handle");
    let client = router.connect().await.expect("client handle");
    let action = FloodedAction {
        core_node: CORE_NODE,
        instance_id: SERVER_INSTANCE,
        target: test_node_target(CORE_NODE),
        action_name: RepoRefreshGoal::ID.name(),
    };
    let _flood = start_flooding_provider(&server, action, LINE_COUNT).await;

    let mut handle = transport::send_goal(
        &RepoRefreshGoal,
        &client,
        CORE_NODE,
        CLIENT_INSTANCE,
        Some(CORE_NODE),
        REPLY_TIMEOUT,
    )
    .await
    .expect("the goal is sent");
    let reply = tokio::time::timeout(
        HANG_GUARD,
        ActionMessenger::request_result(&client, &handle, REPLY_TIMEOUT),
    )
    .await
    .expect("the result request must not hang")
    .expect("the result arrives");
    assert_eq!(reply.status, ResultStatus::Completed);
    assert_eq!(reply.body.as_ref(), FLOOD_RESULT);

    assert_eq!(
        read_to_the_end(&mut handle).await,
        (0..LINE_COUNT).collect::<Vec<_>>()
    );
}
