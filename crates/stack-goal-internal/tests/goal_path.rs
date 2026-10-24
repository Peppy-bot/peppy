//! The goal path driven the way the daemon bridge of the MCP server drives
//! it, against a mock of the daemon's `stack_join` action on peppylib's
//! production goal engine: the acceptance at once, each line of output in
//! order, then the end with the node logs; a refusal with the daemon's
//! failure kind and reason; a caller that drops `next()` while it waits and
//! loses nothing; a goal handed to a task that follows it to its end; a
//! cancel that reaches the daemon; a caller that falls behind the output; a
//! caller that goes away; and a daemon that goes away. Every step waits on
//! the mock's action, never on the clock: a deadline only bounds the failure
//! path.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use config::failure_kind::PeppyFailureKind;
use config::runtime::CoreNodeName;
use config::runtime::Name;
use core_node_api::encoding::{
    CopyAction, CopyChange, CopyInfo, LaunchFeedback, LaunchFeedbackStep, LaunchGoalResponse,
    LaunchOutput, LaunchResult, NodeLogs, NodeRunLogEntry, StackJoinGoal, StackListRequest,
    StackListResponse,
};
use core_node_api::names::CORE_NODE_TAG;
use core_node_api::{ActionGoal, ServiceRequest};
use futures::FutureExt;
use peppylib::messaging::{
    CancelCause, Failure, GoalContext, MessengerHandle, ProducerRef, SenderTarget,
};
use peppylib::testing::{
    EphemeralRouter, MockActionServerCore, MockServiceCore, READINESS_TIMEOUT,
    wait_action_reachable, wait_service_reachable,
};
use peppylib::types::Payload;
use stack_goal::{
    DEFAULT_BUDGETS, DaemonRoute, FollowError, RunningStackGoal, SendError, StackGoalEnd,
    StackGoalEvent, StackGoalOutcome,
};

/// The reason the scripted daemon refuses a goal with.
const REFUSAL_REASON: &str = "a stack change is in progress on this daemon";

/// The coordinator: the daemon's core node, which the bridge is bound to.
const COORDINATOR: &str = "cn-coordinator";
const DAEMON_INSTANCE: &str = "core_root";
const BRIDGE_INSTANCE: &str = "framework_controls_inst";
const LOG_PATH: &str = "/home/peppy/.peppy/logs/stack/join.log";
/// Bounds a wait that the mock or the client ends at once.
const STEP: Duration = Duration::from_secs(10);

/// One mesh per test: the router, a session for the daemon and one for the
/// bridge.
struct Mesh {
    bridge: MessengerHandle,
    daemon: Option<MessengerHandle>,
    _router: EphemeralRouter,
}

impl Mesh {
    /// The route of the bridge to its own daemon.
    fn route(&self) -> DaemonRoute<'_> {
        DaemonRoute {
            messenger: &self.bridge,
            caller_core_node: COORDINATOR,
            caller_instance_id: BRIDGE_INSTANCE,
            daemon_core_node: COORDINATOR,
        }
    }
}

/// Starts the mesh and the daemon's `stack_join`, and returns once the
/// bridge's session reaches it.
async fn mesh() -> (Mesh, MockActionServerCore) {
    let router = EphemeralRouter::start()
        .await
        .expect("the ephemeral router starts");
    let daemon = router.connect().await.expect("the daemon connects");
    let bridge = router.connect().await.expect("the bridge connects");
    let identity = SenderTarget::node(COORDINATOR, CORE_NODE_TAG).expect("a core node identity");
    let action = StackJoinGoal::ID.name();
    let provider = MockActionServerCore::expose(
        &daemon,
        COORDINATOR,
        DAEMON_INSTANCE,
        identity.clone(),
        action,
        true,
    )
    .await
    .expect("the daemon exposes stack_join");
    wait_action_reachable(
        &bridge,
        COORDINATOR,
        BRIDGE_INSTANCE,
        identity,
        action,
        &ProducerRef::new(COORDINATOR, DAEMON_INSTANCE),
        READINESS_TIMEOUT,
    )
    .await
    .expect("stack_join becomes reachable");
    (
        Mesh {
            bridge,
            daemon: Some(daemon),
            _router: router,
        },
        provider,
    )
}

/// The join the bridge sends: the copy on the coordinator, no word, no
/// argument, no environment, under the default budgets.
fn join_goal() -> StackJoinGoal {
    StackJoinGoal::new("so101_sim:bravo".parse().unwrap(), DEFAULT_BUDGETS)
}

/// The daemon's side of the next goal: it checks that the goal arrived as
/// sent, then admits it with its log file.
async fn admit(provider: &mut MockActionServerCore, sent: &StackJoinGoal) -> Arc<GoalContext> {
    let pending = provider
        .next_goal(READINESS_TIMEOUT)
        .await
        .expect("the goal reaches the daemon");
    assert_eq!(
        &StackJoinGoal::decode(pending.request_bytes()).expect("the goal decodes"),
        sent
    );
    pending
        .accept(LaunchGoalResponse::new(LOG_PATH).encode().unwrap())
        .await
        .expect("the daemon admits the goal")
}

/// Sends the join and admits it, returning the bridge's running goal and the
/// daemon's context of it.
async fn running_join(
    mesh: &Mesh,
    provider: &mut MockActionServerCore,
) -> (RunningStackGoal, Arc<GoalContext>) {
    let goal = join_goal();
    let (running, context) = tokio::join!(stack_goal::send(mesh.route(), &goal), async {
        admit(provider, &goal).await
    });
    let running = running.expect("the daemon admits the join");
    assert_eq!(running.operation(), "Join");
    assert_eq!(running.log_path(), PathBuf::from(LOG_PATH));
    (running, context)
}

fn output(step: LaunchFeedbackStep, line: &str) -> LaunchOutput {
    LaunchOutput::stdout(line, step)
}

/// The progress of a join, one line per step.
fn join_progress() -> Vec<LaunchOutput> {
    vec![
        output(
            LaunchFeedbackStep::LauncherStep,
            "Adding so101_sim as bravo",
        ),
        output(
            LaunchFeedbackStep::AddingNode,
            "fetching robot_initializer:v1",
        ),
        LaunchOutput::stderr(
            "Compiling robot_initializer",
            LaunchFeedbackStep::BuildingNode,
        ),
        output(
            LaunchFeedbackStep::RunningNode,
            "'bravo' does not stand yet",
        ),
    ]
}

/// The node logs of a join that failed at the start of `robot_initializer`.
fn failed_join_logs() -> NodeLogs {
    NodeLogs {
        node_run_logs: vec![NodeRunLogEntry {
            instance_id: "bravo_robot_initializer_inst".to_owned(),
            node_label: "robot_initializer:v1".to_owned(),
            log_path: PathBuf::from("/home/peppy/.peppy/logs/run/bravo.log"),
            failed: true,
            core_node: COORDINATOR.to_owned(),
        }],
        ..NodeLogs::default()
    }
}

/// The failure of that join.
fn failed_join() -> Failure {
    Failure::new(
        PeppyFailureKind::AttemptFailed,
        "bravo_robot_initializer_inst exited during setup",
    )
}

async fn publish(context: &GoalContext, output: &LaunchOutput) {
    publish_feedback(context, LaunchFeedback::Output(output.clone())).await;
}

async fn publish_feedback(context: &GoalContext, feedback: LaunchFeedback) {
    context
        .publish_feedback(feedback.encode().unwrap())
        .await
        .expect("the daemon publishes its feedback");
}

/// The daemon's end of a failed join: the node logs as the last feedback,
/// then the failure.
async fn fail_join(context: &GoalContext) {
    publish_feedback(context, LaunchFeedback::NodeLogs(failed_join_logs())).await;
    context.fail(failed_join()).await.unwrap();
}

/// The end the client reads for [`fail_join`].
fn failed_join_end() -> StackGoalEnd {
    StackGoalEnd {
        outcome: StackGoalOutcome::Failed(failed_join()),
        node_logs: failed_join_logs(),
    }
}

/// The next event of `running`, which the mock already sent.
async fn next_event(running: &mut RunningStackGoal) -> Result<StackGoalEvent, FollowError> {
    tokio::time::timeout(STEP, running.next())
        .await
        .expect("the client gives the event the daemon sent")
}

/// Waits on `running.next()` beside a cancel that comes first, as a caller
/// whose client cancels does: `next()` is polled, finds nothing, and is
/// dropped at its await point.
async fn stop_waiting(running: &mut RunningStackGoal) {
    tokio::select! {
        biased;
        event = running.next() => panic!("the daemon sent nothing yet: {event:?}"),
        () = std::future::ready(()) => {}
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_join_gives_its_acceptance_then_each_line_in_order_then_its_end() {
    let (mesh, mut provider) = mesh().await;
    let (mut running, context) = running_join(&mesh, &mut provider).await;
    for message in join_progress() {
        publish(&context, &message).await;
    }
    fail_join(&context).await;

    for message in join_progress() {
        let event = next_event(&mut running).await.expect("an output event");
        assert_eq!(event, StackGoalEvent::Output(message));
    }
    assert_eq!(
        next_event(&mut running).await.expect("the end"),
        StackGoalEvent::Ended(failed_join_end())
    );
    assert!(matches!(
        next_event(&mut running).await,
        Err(FollowError::Ended { operation: "Join" })
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_refused_goal_gives_the_daemons_failure_kind_and_reason() {
    let (mesh, mut provider) = mesh().await;
    let goal = join_goal();
    let refusal = Failure::new(PeppyFailureKind::PreconditionFailed, REFUSAL_REASON);
    let (sent, ()) = tokio::join!(stack_goal::send(mesh.route(), &goal), async {
        provider
            .next_goal(READINESS_TIMEOUT)
            .await
            .expect("the goal reaches the daemon")
            .refuse(refusal.clone())
            .await
            .expect("the daemon refuses the goal");
    });
    match sent {
        Err(error @ SendError::Refused { .. }) => {
            assert_eq!(
                error.to_string(),
                format!("Join goal refused (precondition_failed): {REFUSAL_REASON}")
            );
            let SendError::Refused { failure, .. } = error else {
                unreachable!()
            };
            assert_eq!(failure, refusal);
        }
        Err(other) => panic!("expected the daemon's refusal, got {other}"),
        Ok(_) => panic!("the daemon refused the goal"),
    }
}

/// A `next()` dropped before the first message, and one dropped between two
/// messages, lose nothing: the next calls give every message in order, then
/// the result.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_caller_that_drops_next_while_it_waits_loses_no_message() {
    let (mesh, mut provider) = mesh().await;
    let (mut running, context) = running_join(&mesh, &mut provider).await;
    let progress = join_progress();

    stop_waiting(&mut running).await;
    publish(&context, &progress[0]).await;
    assert_eq!(
        next_event(&mut running).await.unwrap(),
        StackGoalEvent::Output(progress[0].clone())
    );
    assert!(
        running.next().now_or_never().is_none(),
        "the daemon sent nothing more yet"
    );
    for message in &progress[1..] {
        publish(&context, message).await;
    }
    fail_join(&context).await;

    for message in &progress[1..] {
        assert_eq!(
            next_event(&mut running).await.unwrap(),
            StackGoalEvent::Output(message.clone())
        );
    }
    assert_eq!(
        next_event(&mut running).await.unwrap(),
        StackGoalEvent::Ended(failed_join_end())
    );
}

/// A caller that stops waiting hands the goal to a task, which follows it to
/// the daemon's result.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_goal_handed_to_a_task_is_followed_to_its_result() {
    let (mesh, mut provider) = mesh().await;
    let (mut running, context) = running_join(&mesh, &mut provider).await;
    stop_waiting(&mut running).await;
    let follower = tokio::spawn(running.follow_to_end());

    for message in join_progress() {
        publish(&context, &message).await;
    }
    let joined = LaunchResult::default();
    context.complete(joined.encode().unwrap()).await.unwrap();

    let end = tokio::time::timeout(STEP, follower)
        .await
        .expect("the task ends with the goal")
        .expect("the task does not panic")
        .expect("the goal has an end");
    assert_eq!(
        end,
        StackGoalEnd {
            outcome: StackGoalOutcome::Completed(joined),
            node_logs: NodeLogs::default(),
        }
    );
}

/// A cancel reaches the daemon's goal with the cause of its caller; the
/// daemon ends the goal cancelled with that reason, and the caller follows
/// the goal to that end.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_cancel_reaches_the_daemon_and_the_goal_ends_cancelled() {
    let (mesh, mut provider) = mesh().await;
    let (mut running, context) = running_join(&mesh, &mut provider).await;
    let progress = join_progress();
    publish(&context, &progress[0]).await;
    assert_eq!(
        next_event(&mut running).await.unwrap(),
        StackGoalEvent::Output(progress[0].clone())
    );

    running.cancel().await.expect("the daemon takes the cancel");
    tokio::time::timeout(STEP, context.cancel_signal())
        .await
        .expect("the cancel reaches the daemon's goal");
    let cause = context
        .cancel_cause()
        .expect("a cancelled goal has a cause");
    assert_eq!(cause, CancelCause::Caller);
    publish_feedback(&context, LaunchFeedback::NodeLogs(failed_join_logs())).await;
    context
        .complete_cancelled(cause.reason(), Payload::new())
        .await
        .unwrap();

    assert_eq!(
        next_event(&mut running).await.unwrap(),
        StackGoalEvent::Ended(StackGoalEnd {
            outcome: StackGoalOutcome::Cancelled {
                reason: "cancelled by its caller".to_owned(),
            },
            node_logs: failed_join_logs(),
        })
    );
}

/// A caller whose session ends, as a command line process that stops does,
/// is a sender that is gone: the daemon's engine cancels its goal with the
/// cause `SenderGone`, under the default sender grace of zero.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_caller_that_goes_away_gets_its_goal_cancelled() {
    let (mut mesh, mut provider) = mesh().await;
    let (running, context) = running_join(&mesh, &mut provider).await;

    drop(running);
    // The bridge's session is the only one of its process: dropping it ends
    // its presence on the router.
    let bridge = std::mem::replace(
        &mut mesh.bridge,
        mesh._router.connect().await.expect("another session"),
    );
    drop(bridge);

    tokio::time::timeout(READINESS_TIMEOUT, context.cancel_signal())
        .await
        .expect("the engine cancels the goal of a sender that is gone");
    assert_eq!(context.cancel_cause(), Some(CancelCause::SenderGone));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_daemon_that_goes_away_ends_the_goal_without_a_result() {
    let (mut mesh, mut provider) = mesh().await;
    let (mut running, context) = running_join(&mesh, &mut provider).await;
    let progress = join_progress();
    publish(&context, &progress[0]).await;
    assert_eq!(
        next_event(&mut running).await.unwrap(),
        StackGoalEvent::Output(progress[0].clone())
    );

    provider.stop();
    drop(mesh.daemon.take());
    let gone = next_event(&mut running).await;
    let error = gone.expect_err("a daemon that is gone sends no result");
    assert!(
        matches!(error, FollowError::DaemonGone { operation: "Join" }),
        "{error}"
    );
    assert_eq!(
        error.to_string(),
        "Join ended without a result: its daemon is gone"
    );
    drop(context);
}

/// A caller that falls behind the output of the goal by more than the
/// bound of its feedback buffer loses the stream: its handle cancels the
/// goal, and the next event says that the caller fell behind and cancelled
/// it. The daemon publishes until the cancel reaches it, so the test waits
/// on that event and never on the clock.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_caller_that_falls_behind_the_output_cancels_the_goal_and_says_so() {
    let (mesh, mut provider) = mesh().await;
    let (mut running, context) = running_join(&mesh, &mut provider).await;
    let bound = peppylib::core_node::transport::DAEMON_GOAL_FEEDBACK_BOUND.get();
    let line = output(
        LaunchFeedbackStep::BuildingNode,
        "Compiling robot_initializer",
    );

    let mut published = 0;
    while !context.is_cancelled() {
        assert!(
            published <= 4 * bound,
            "the handle cancels the goal once more than {bound} messages wait unread"
        );
        publish(&context, &line).await;
        published += 1;
    }
    assert_eq!(context.cancel_cause(), Some(CancelCause::Caller));

    let error = next_event(&mut running)
        .await
        .expect_err("the caller fell behind the output");
    assert!(
        matches!(
            error,
            FollowError::FellBehind { operation: "Join", bound: fell_behind }
                if fell_behind == bound
        ),
        "{error}"
    );
    assert_eq!(
        error.to_string(),
        format!(
            "the caller fell behind the output of the join by more than {bound} unread \
             messages, so it cancelled the join"
        )
    );
    assert!(matches!(
        next_event(&mut running).await,
        Err(FollowError::Ended { operation: "Join" })
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_result_that_expired_before_its_request_ends_the_goal_saying_so() {
    let (mesh, provider) = mesh().await;
    let mut provider = provider.with_result_retention_grace(Duration::ZERO);
    let (mut running, context) = running_join(&mesh, &mut provider).await;
    context
        .complete(LaunchResult::default().encode().unwrap())
        .await
        .unwrap();

    let error = next_event(&mut running)
        .await
        .expect_err("the result is gone");
    assert!(
        matches!(error, FollowError::Expired { operation: "Join" }),
        "{error}"
    );
}

/// The copies come from the daemon's `stack list`, asked along the route the
/// goals take, with every field the daemon answers.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_copies_are_read_from_the_daemon_along_the_route() {
    let (mesh, _provider) = mesh().await;
    let daemon = mesh.daemon.as_ref().expect("the daemon is connected");
    let identity = SenderTarget::node(COORDINATOR, CORE_NODE_TAG).expect("a core node identity");
    let service = StackListRequest::ID.name();
    let list = MockServiceCore::listen(
        daemon,
        COORDINATOR,
        DAEMON_INSTANCE,
        identity.clone(),
        service,
    )
    .await
    .expect("the daemon serves stack_list");
    wait_service_reachable(
        &mesh.bridge,
        COORDINATOR,
        BRIDGE_INSTANCE,
        identity,
        service,
        &ProducerRef::new(COORDINATOR, DAEMON_INSTANCE),
        READINESS_TIMEOUT,
    )
    .await
    .expect("stack_list becomes reachable");
    let bravo = CopyInfo {
        name: config::runtime::CopyName::new("bravo").unwrap(),
        core_node: CoreNodeName::new(COORDINATOR).unwrap(),
        instance_ids: vec![Name::new("bravo_arm_inst").unwrap()],
        selections: vec!["commander=pad@cn-gpu-box".parse().unwrap()],
        option: "so101_sim".to_owned(),
        set_members: Vec::new(),
    };
    let charlie = CopyChange {
        action: CopyAction::Join,
        name: config::runtime::CopyName::new("charlie").unwrap(),
        option: "so101_sim".to_owned(),
    };
    let mut answer = StackListResponse::new("{}", COORDINATOR, DAEMON_INSTANCE, "host")
        .with_copy_change(charlie.clone());
    answer.copies = vec![bravo.clone()];
    list.enqueue_response(answer.encode().unwrap());

    let listed = stack_goal::list_copies(mesh.route(), STEP)
        .await
        .expect("the daemon answers");
    assert_eq!(
        listed,
        stack_goal::StackCopies {
            copies: vec![bravo],
            copy_change: Some(charlie),
        }
    );
    let asked = list.captured();
    assert_eq!(asked.len(), 1, "one request");
    assert_eq!(asked[0].message.instance_id(), BRIDGE_INSTANCE);
}
