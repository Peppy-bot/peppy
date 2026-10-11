//! The goal loop driven against a real provider: an ephemeral router, a
//! mock action server running peppylib's production goal engine, and the
//! bridge settling on each outcome a provider can reach. Every step waits
//! on the peer's action, never on the clock: the deadline only bounds the
//! failure path. The bound of a goal is tested on a bridge whose clock
//! moves only when the test advances it (see [`PausedBridge`]), and the
//! bridge behind a real endpoint over HTTP in [`loopback`].

use super::{
    AfterCancel, Binding, CancelReason, FeedbackStep, GoalFollow, PreparedTask, Silence, Turn,
    after_cancel, drive_goal, next_turn,
};
use crate::test_support::{PausedRuntime, ScriptedSurface};
use config::node::{MessageFormat, QoSProfile};
use message_codec::MessageCodec;
use message_codec::consumer::{ActionClient, ConsumerError, ConsumerIdentity, MemberBinding};
use peppy_mcp_catalog::GoalBound;
use peppy_mcp_runtime::{ActionExit, CancelledGoal};
use peppylib::PeppyError;
use peppylib::messaging::{
    CancelState, GoalContext, MessengerHandle, NonEmptyPayload, ProducerRef, SenderTarget,
};
use peppylib::testing::{
    EphemeralRouter, MockActionServerCore, READINESS_TIMEOUT, wait_action_reachable,
};
use peppylib::types::Payload;
use serde_json::{Value, json};
use std::num::NonZeroU64;
use std::sync::Arc;
use std::time::Duration;

mod loopback;

const CORE_NODE: &str = "test_core";
const PROVIDER_INSTANCE: &str = "backbone_inst";
const BRIDGE_INSTANCE: &str = "commander_inst";
const LINK_ID: &str = "limb_motion";
const MEMBER: &str = "move_gripper";
/// The whole-goal deadline: a bound on the failure path only, every step
/// below settles the moment the provider acts.
const DEADLINE: Duration = Duration::from_secs(10);
/// The bound of the goals a [`PausedBridge`] drives. Its clock stands still
/// until the test advances it, so the value only sizes the test's steps.
const WINDOW: Duration = Duration::from_secs(60);
/// A step of the paused clock that stays inside [`WINDOW`].
const WITHIN_THE_WINDOW: Duration = WINDOW.saturating_sub(Duration::from_millis(1));

/// One mesh per test: the router, a session per side, and the provider's
/// identity as the launcher would have bound it to the task's target.
struct Mesh {
    bridge_messenger: MessengerHandle,
    _provider_messenger: MessengerHandle,
    _router: EphemeralRouter,
    contract: SenderTarget,
    producer: ProducerRef,
}

/// Starts the mesh and exposes `MEMBER` on it, returning once the bridge's
/// session can reach the provider.
async fn mesh(has_feedback: bool) -> (Mesh, MockActionServerCore) {
    let router = EphemeralRouter::start()
        .await
        .expect("the ephemeral router starts");
    let provider_messenger = router.connect().await.expect("the provider connects");
    let bridge_messenger = router.connect().await.expect("the bridge connects");
    let contract = SenderTarget::contract("limb_motion", "v1").expect("a valid contract identity");
    let provider = MockActionServerCore::expose(
        &provider_messenger,
        CORE_NODE,
        PROVIDER_INSTANCE,
        contract.clone(),
        MEMBER,
        has_feedback,
    )
    .await
    .expect("the provider exposes the action");
    let producer = ProducerRef::new(CORE_NODE, PROVIDER_INSTANCE);
    wait_action_reachable(
        &bridge_messenger,
        CORE_NODE,
        BRIDGE_INSTANCE,
        contract.clone(),
        MEMBER,
        &producer,
        READINESS_TIMEOUT,
    )
    .await
    .expect("the action becomes reachable");
    (
        Mesh {
            bridge_messenger,
            _provider_messenger: provider_messenger,
            _router: router,
            contract,
            producer,
        },
        provider,
    )
}

fn codec(label: &str, format: Value) -> MessageCodec {
    let format: MessageFormat = serde_json::from_value(format).expect("the format parses");
    MessageCodec::new(label, format).expect("the format lays out")
}

fn result_codec() -> MessageCodec {
    codec("move_gripper_result", json!({ "success": "bool" }))
}

fn encoded(codec: &MessageCodec, value: Value) -> Payload {
    Payload::from(codec.encode(&value).expect("the value fits the format"))
}

/// The task as `prepare` lays it out for an action with a result and,
/// optionally, a feedback message.
fn prepared_task(mesh: &Mesh, feedback: Option<MessageCodec>) -> PreparedTask {
    PreparedTask {
        name: "openarm.move_gripper".to_owned(),
        binding: Binding {
            target: LINK_ID.to_owned(),
            contract: mesh.contract.clone(),
            member: MEMBER.to_owned(),
        },
        follow: GoalFollow::WholeGoal {
            deadline: DEADLINE,
            reports_feedback: feedback.is_some(),
        },
        client: ActionClient::new(None, feedback, Some(result_codec())),
        feedback_qos: QoSProfile::Reliable,
    }
}

fn bridge_identity() -> ConsumerIdentity {
    ConsumerIdentity {
        core_node: CORE_NODE.to_owned(),
        instance_id: BRIDGE_INSTANCE.to_owned(),
    }
}

fn member_binding(mesh: &Mesh) -> MemberBinding {
    MemberBinding {
        target: mesh.contract.clone(),
        member: MEMBER.to_owned(),
    }
}

/// Drives one goal through the bridge exactly as `run_task` does once the
/// node runner has resolved the binding.
async fn drive(
    mesh: &Mesh,
    task: &PreparedTask,
    surface: &ScriptedSurface,
) -> Result<Value, ActionExit> {
    drive_goal(
        task,
        &mesh.bridge_messenger,
        &bridge_identity(),
        &member_binding(mesh),
        &mesh.producer,
        json!({}),
        surface,
    )
    .await
}

/// A bridge driving one goal on a runtime whose clock moves only when the
/// test advances it (see [`PausedRuntime`]).
type PausedBridge = PausedRuntime<Result<Value, ActionExit>>;

/// Starts driving the goal of `task` on a [`PausedBridge`], reporting
/// through `surface`. The bridge's warnings are those of the module these
/// tests sit in.
fn drive_paused(mesh: &Mesh, task: PreparedTask, surface: Arc<ScriptedSurface>) -> PausedBridge {
    let bridge_module = module_path!()
        .strip_suffix("::tests")
        .expect("the tests sit in the bridge's module");
    let messenger = mesh.bridge_messenger.clone();
    let binding = member_binding(mesh);
    let producer = mesh.producer.clone();
    PausedRuntime::spawn(
        mesh.bridge_messenger.clone(),
        bridge_module,
        move || async move {
            drive_goal(
                &task,
                &messenger,
                &bridge_identity(),
                &binding,
                &producer,
                json!({}),
                surface.as_ref(),
            )
            .await
        },
    )
}

fn percent_codec() -> MessageCodec {
    codec("move_gripper_feedback", json!({ "percent": "u8" }))
}

/// One feedback message of the goal: how far it got.
fn percent(feedback: &MessageCodec, percent: u8) -> NonEmptyPayload {
    NonEmptyPayload::try_new(encoded(feedback, json!({ "percent": percent })))
        .expect("an encoded message is never empty")
}

/// The provider's side of the next goal: accepted as it arrives.
async fn accepted_goal(provider: &mut MockActionServerCore) -> Arc<GoalContext> {
    provider
        .next_goal(READINESS_TIMEOUT)
        .await
        .expect("the goal reaches the provider")
        .accept(Payload::new())
        .await
        .expect("the provider accepts the goal")
}

/// Returns once the provider has seen a cancel request for its goal.
async fn cancel_seen(context: &GoalContext) {
    tokio::time::timeout(READINESS_TIMEOUT, context.cancel_signal())
        .await
        .expect("the bridge sends the provider a cancel");
}

fn success() -> Payload {
    encoded(&result_codec(), json!({ "success": true }))
}

/// The result the provider ends a cancelled goal with.
fn cancelled_result() -> Value {
    json!({ "success": false })
}

fn cancelled() -> Payload {
    encoded(&result_codec(), cancelled_result())
}

/// How the bridge reports a goal the provider ended cancelled with
/// [`cancelled_result`], with the bridge's own `reason` for the cancel.
fn ended_cancelled(reason: Option<&str>) -> Result<Value, ActionExit> {
    Err(ActionExit::Cancelled(CancelledGoal {
        result: cancelled_result(),
        reason: reason.map(str::to_owned),
    }))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_feedback_less_goal_settles_on_its_completed_result() {
    let (mesh, mut provider) = mesh(false).await;
    let task = prepared_task(&mesh, None);
    let surface = ScriptedSurface::new();

    let (outcome, _context) = tokio::join!(drive(&mesh, &task, &surface), async {
        let goal = provider
            .next_goal(READINESS_TIMEOUT)
            .await
            .expect("the goal reaches the provider");
        let context = goal
            .accept(Payload::new())
            .await
            .expect("the provider accepts the goal");
        context
            .complete(success())
            .await
            .expect("the provider completes the goal");
        context
    });

    assert_eq!(outcome, Ok(json!({ "success": true })));
    assert_eq!(surface.feedback(), Vec::<String>::new());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_feedback_less_goal_settles_cancelled_once_the_provider_honors_the_cancel() {
    let (mesh, mut provider) = mesh(false).await;
    let task = prepared_task(&mesh, None);
    let surface = ScriptedSurface::new();

    let (outcome, _context) = tokio::join!(drive(&mesh, &task, &surface), async {
        let goal = provider
            .next_goal(READINESS_TIMEOUT)
            .await
            .expect("the goal reaches the provider");
        let context = goal
            .accept(Payload::new())
            .await
            .expect("the provider accepts the goal");
        // The client cancels while the goal runs: the bridge forwards it,
        // and the provider settles the goal cancelled once it observes it.
        surface.cancel.cancel();
        context.cancel_signal().await;
        context
            .complete_cancelled(cancelled())
            .await
            .expect("the provider settles the goal cancelled");
        context
    });

    assert_eq!(outcome, ended_cancelled(None));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_goal_with_feedback_reports_it_and_settles_on_its_result() {
    let (mesh, mut provider) = mesh(true).await;
    let feedback = percent_codec();
    let task = prepared_task(&mesh, Some(feedback.clone()));
    let surface = ScriptedSurface::new();

    let (outcome, _context) = tokio::join!(drive(&mesh, &task, &surface), async {
        let goal = provider
            .next_goal(READINESS_TIMEOUT)
            .await
            .expect("the goal reaches the provider");
        let context = goal
            .accept(Payload::new())
            .await
            .expect("the provider accepts the goal");
        context
            .publish_feedback(percent(&feedback, 50))
            .await
            .expect("the provider publishes feedback");
        context
            .complete(success())
            .await
            .expect("the provider completes the goal");
        context
    });

    assert_eq!(outcome, Ok(json!({ "success": true })));
    assert_eq!(surface.feedback(), vec![r#"{"percent":50}"#.to_owned()]);
}

/// A goal on an action with feedback, followed as `follow` says.
fn task_with_feedback(mesh: &Mesh, follow: GoalFollow) -> PreparedTask {
    PreparedTask {
        follow,
        ..prepared_task(mesh, Some(percent_codec()))
    }
}

/// A goal on an action with feedback, bounded by its progress: at most
/// [`WINDOW`] without a sign of it.
fn progress_task(mesh: &Mesh) -> PreparedTask {
    task_with_feedback(mesh, GoalFollow::Progress { window: WINDOW })
}

/// Publishes one feedback message of the goal: how far it got.
async fn publish_percent(context: &GoalContext, value: u8) {
    context
        .publish_feedback(percent(&percent_codec(), value))
        .await
        .expect("the provider publishes feedback");
}

/// Publishes one feedback message that no feedback format decodes.
async fn publish_garbage(context: &GoalContext) {
    let garbage =
        NonEmptyPayload::try_new(Payload::from(vec![0xff; 3])).expect("three bytes are not empty");
    context
        .publish_feedback(garbage)
        .await
        .expect("the provider publishes feedback");
}

/// A goal bounded by its progress, driven by a bridge on a paused clock,
/// with the provider's context and the client's surface of it.
struct ProgressGoal {
    bridge: PausedBridge,
    context: Arc<GoalContext>,
    surface: Arc<ScriptedSurface>,
}

impl ProgressGoal {
    /// Sends the goal of [`progress_task`] and accepts it, returning once
    /// the bridge follows it: its first window runs from here.
    async fn accepted(mesh: &Mesh, provider: &mut MockActionServerCore) -> Self {
        let surface = Arc::new(ScriptedSurface::new());
        let bridge = drive_paused(mesh, progress_task(mesh), Arc::clone(&surface));
        let context = accepted_goal(provider).await;
        surface.followed().await;
        Self {
            bridge,
            context,
            surface,
        }
    }

    /// An accepted goal whose first feedback message the client has seen.
    async fn reporting(mesh: &Mesh, provider: &mut MockActionServerCore) -> Self {
        let goal = Self::accepted(mesh, provider).await;
        publish_percent(&goal.context, 10).await;
        goal.surface.reported(1).await;
        goal
    }

    /// A whole window goes by without a sign of progress, and the provider
    /// sees the cancel the bridge sends for it.
    async fn stall(&self) {
        self.bridge.advance(WINDOW).await;
        cancel_seen(&self.context).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_whole_goal_deadline_is_not_pushed_back_by_feedback() {
    let (mesh, mut provider) = mesh(true).await;
    let surface = Arc::new(ScriptedSurface::new());
    let whole_goal = GoalFollow::WholeGoal {
        deadline: WINDOW,
        reports_feedback: true,
    };
    let bridge = drive_paused(
        &mesh,
        task_with_feedback(&mesh, whole_goal),
        Arc::clone(&surface),
    );
    let context = accepted_goal(&mut provider).await;

    // The same steps a progress window lets through: the second one
    // crosses the deadline, and the feedback before it does not move it.
    for step in 1..=2u8 {
        publish_percent(&context, step * 20).await;
        surface.reported(usize::from(step)).await;
        bridge.advance(WITHIN_THE_WINDOW).await;
    }

    let outcome = bridge.outcome().await;
    assert!(
        matches!(outcome, Err(ActionExit::Failed(_))),
        "the deadline fails the goal: {outcome:?}"
    );
    assert!(
        !context.is_cancelled(),
        "a whole-goal deadline sends the provider no cancel"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_client_cancel_reaches_a_whole_goal_after_feedback_that_does_not_convert() {
    let (mesh, mut provider) = mesh(true).await;
    let task = prepared_task(&mesh, Some(percent_codec()));
    let surface = ScriptedSurface::new();

    let (outcome, _context) = tokio::join!(drive(&mesh, &task, &surface), async {
        let context = accepted_goal(&mut provider).await;
        publish_garbage(&context).await;
        publish_percent(&context, 50).await;
        surface.reported(1).await;
        surface.cancel.cancel();
        cancel_seen(&context).await;
        context
            .complete_cancelled(cancelled())
            .await
            .expect("the provider settles the goal cancelled");
        context
    });

    assert_eq!(outcome, ended_cancelled(None));
    assert_eq!(surface.feedback(), vec![r#"{"percent":50}"#.to_owned()]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn feedback_that_does_not_convert_is_a_sign_of_progress() {
    let (mesh, mut provider) = mesh(true).await;
    let goal = ProgressGoal::reporting(&mesh, &mut provider).await;

    // The message that does not convert comes just before the window ends,
    // and starts a new one: the step after it stays inside that window.
    goal.bridge.advance(WITHIN_THE_WINDOW).await;
    publish_garbage(&goal.context).await;
    goal.bridge.logged_warnings(1).await;
    goal.bridge.advance(WITHIN_THE_WINDOW).await;
    // A cancel sent for a silence is awaited before the next message is
    // taken, so once this one is shown, any such cancel has reached the
    // provider.
    publish_percent(&goal.context, 20).await;
    goal.surface.reported(2).await;
    goal.context
        .complete(success())
        .await
        .expect("the provider completes the goal");

    assert_eq!(goal.bridge.outcome().await, Ok(json!({ "success": true })));
    assert!(
        !goal.context.is_cancelled(),
        "no window went by without a sign of progress"
    );
    assert_eq!(
        goal.surface.feedback(),
        vec![
            r#"{"percent":10}"#.to_owned(),
            r#"{"percent":20}"#.to_owned()
        ]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_goal_that_keeps_reporting_progress_runs_past_one_window() {
    let (mesh, mut provider) = mesh(true).await;
    let goal = ProgressGoal::accepted(&mesh, &mut provider).await;

    // Four steps of just under one window each: the goal runs for close to
    // four windows, and no silence in it lasts one.
    for step in 1..=4u8 {
        publish_percent(&goal.context, step * 20).await;
        goal.surface.reported(usize::from(step)).await;
        goal.bridge.advance(WITHIN_THE_WINDOW).await;
    }
    goal.context
        .complete(success())
        .await
        .expect("the provider completes the goal");

    assert_eq!(goal.bridge.outcome().await, Ok(json!({ "success": true })));
    assert_eq!(goal.surface.feedback().len(), 4);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_goal_silent_for_one_window_is_cancelled_and_reported_stalled() {
    let (mesh, mut provider) = mesh(true).await;
    let goal = ProgressGoal::reporting(&mesh, &mut provider).await;

    goal.stall().await;
    goal.context
        .complete_cancelled(cancelled())
        .await
        .expect("the provider settles the goal cancelled");

    assert_eq!(
        goal.bridge.outcome().await,
        ended_cancelled(Some("no progress within 60000 ms"))
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_provider_that_ignores_the_cancel_is_reported_one_window_later() {
    let (mesh, mut provider) = mesh(true).await;
    let goal = ProgressGoal::reporting(&mesh, &mut provider).await;

    goal.stall().await;
    // The bridge takes feedback again only once the provider's reply to the
    // cancel is in, and the reply is timed on the bridge's clock: the window
    // passes only after this message is shown. The provider reports once
    // more, then never ends the goal.
    publish_percent(&goal.context, 20).await;
    goal.surface.reported(2).await;
    goal.bridge.advance(WINDOW).await;

    assert_eq!(
        goal.bridge.outcome().await,
        Err(ActionExit::Failed(
            "no progress within 60000 ms; a cancel was sent and the provider did not answer it"
                .to_owned()
        ))
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_goal_that_completes_after_the_cancel_is_reported_with_its_result() {
    let (mesh, mut provider) = mesh(true).await;
    let goal = ProgressGoal::reporting(&mesh, &mut provider).await;

    goal.stall().await;
    goal.context
        .complete(success())
        .await
        .expect("the provider completes the goal all the same");

    assert_eq!(goal.bridge.outcome().await, Ok(json!({ "success": true })));
}

/// The provider completes the goal, and no end of its feedback stream
/// reaches the bridge: a provider without a feedback publisher stands in
/// for a lost end. The cancel the silence brings is answered with "already
/// ended", and the bridge reads the result at once.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_goal_that_ended_unseen_is_reported_with_its_result_once_the_cancel_says_so() {
    let (mesh, mut provider) = mesh(false).await;
    let goal = ProgressGoal::accepted(&mesh, &mut provider).await;
    goal.context
        .complete(success())
        .await
        .expect("the provider completes the goal");

    goal.bridge.advance(WINDOW).await;

    assert_eq!(goal.bridge.outcome().await, Ok(json!({ "success": true })));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_goal_that_ended_unseen_with_its_result_gone_fails_saying_so() {
    let (mesh, provider) = mesh(false).await;
    let mut provider = provider.with_result_retention_grace(Duration::ZERO);
    let goal = ProgressGoal::accepted(&mesh, &mut provider).await;
    goal.context
        .complete(success())
        .await
        .expect("the provider completes the goal");

    goal.bridge.advance(WINDOW).await;

    assert_eq!(
        goal.bridge.outcome().await,
        Err(ActionExit::Failed(
            "the goal ended, but its result is no longer readable".to_owned()
        ))
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_client_cancel_ends_a_progress_bound_goal_cancelled() {
    let (mesh, mut provider) = mesh(true).await;
    let goal = ProgressGoal::reporting(&mesh, &mut provider).await;

    goal.surface.cancel.cancel();
    cancel_seen(&goal.context).await;
    goal.context
        .complete_cancelled(cancelled())
        .await
        .expect("the provider settles the goal cancelled");

    assert_eq!(goal.bridge.outcome().await, ended_cancelled(None));
}

/// Rounds of the turn tests below. Tokio's `select!` polls its branches
/// from a random one unless it is biased: with two of them ready, the
/// wrong one wins about one round in three, so an order that is not fixed
/// fails these rounds all but surely.
const TURN_ROUNDS: usize = 64;

#[tokio::test(start_paused = true)]
async fn a_message_already_delivered_wins_over_a_window_that_ran_out() {
    let surface = ScriptedSurface::new();
    for round in 0..TURN_ROUNDS {
        // The window runs out while a message waits to be taken: the goal
        // showed progress, and is not cancelled for a silence.
        let mut silence = Silence::new(WINDOW);
        tokio::time::advance(WINDOW).await;
        let turn = next_turn(
            &mut silence,
            &surface,
            true,
            std::future::ready(FeedbackStep::Shown),
        )
        .await;
        assert_eq!(turn, Turn::Feedback(FeedbackStep::Shown), "round {round}");
    }
}

#[tokio::test(start_paused = true)]
async fn the_client_s_cancel_comes_first_and_the_silence_last() {
    let surface = ScriptedSurface::new();
    surface.cancel.cancel();
    for round in 0..TURN_ROUNDS {
        let mut silence = Silence::new(WINDOW);
        tokio::time::advance(WINDOW).await;
        let delivered = || std::future::ready(FeedbackStep::Shown);
        assert_eq!(
            next_turn(&mut silence, &surface, true, delivered()).await,
            Turn::CancelRequested,
            "round {round}: the client's cancel is taken before anything else"
        );
        assert_eq!(
            next_turn(&mut silence, &surface, false, delivered()).await,
            Turn::Feedback(FeedbackStep::Shown),
            "round {round}: once the cancel is sent, a message comes before the silence"
        );
        assert_eq!(
            next_turn(&mut silence, &surface, false, std::future::pending()).await,
            Turn::Silence,
            "round {round}: without a message, the window that ran out ends the turn"
        );
    }
}

#[test]
fn a_progress_bound_is_followed_only_on_an_action_that_reports_feedback() {
    let window_ms = NonZeroU64::new(60_000).expect("nonzero");
    assert_eq!(
        GoalFollow::new(GoalBound::Progress { window_ms }, true),
        Some(GoalFollow::Progress { window: WINDOW })
    );
    assert_eq!(
        GoalFollow::new(GoalBound::Progress { window_ms }, false),
        None,
        "a feedback-less action shows no sign of progress"
    );
    let deadline_ms = NonZeroU64::new(10_000).expect("nonzero");
    assert_eq!(
        GoalFollow::new(GoalBound::WholeGoal { deadline_ms }, false),
        Some(GoalFollow::WholeGoal {
            deadline: DEADLINE,
            reports_feedback: false,
        })
    );
}

#[test]
fn the_reply_to_a_cancel_says_whether_to_follow_the_goal_or_read_its_result() {
    for reason in [CancelReason::Client, CancelReason::Silence] {
        assert_eq!(
            after_cancel(Ok(CancelState::Signalled), reason, WINDOW, "goal"),
            Ok(AfterCancel::KeepFollowing)
        );
        assert_eq!(
            after_cancel(Ok(CancelState::AlreadyTerminal), reason, WINDOW, "goal"),
            Ok(AfterCancel::ReadTheResult)
        );
    }
}

#[test]
fn a_cancel_the_provider_cannot_act_on_ends_the_follow_saying_why() {
    assert_eq!(
        after_cancel(
            Ok(CancelState::Unknown),
            CancelReason::Silence,
            WINDOW,
            "goal"
        ),
        Err(ActionExit::Failed(
            "no progress within 60000 ms; a cancel was sent and the provider does not know the goal"
                .to_owned()
        ))
    );
    assert_eq!(
        after_cancel(
            Ok(CancelState::Unknown),
            CancelReason::Client,
            WINDOW,
            "goal"
        ),
        Err(ActionExit::Failed(
            "a cancel was sent and the provider does not know the goal".to_owned()
        ))
    );

    let unreachable = || PeppyError::Io(std::io::Error::other("the provider is unreachable"));
    let error = unreachable().to_string();
    assert_eq!(
        after_cancel(
            Err(ConsumerError::Messaging(unreachable())),
            CancelReason::Silence,
            WINDOW,
            "goal"
        ),
        Err(ActionExit::Failed(format!(
            "no progress within 60000 ms; a cancel was sent and failed: {error}"
        )))
    );
    assert_eq!(
        after_cancel(
            Err(ConsumerError::Messaging(unreachable())),
            CancelReason::Client,
            WINDOW,
            "goal"
        ),
        Err(ActionExit::Failed(format!(
            "a cancel was sent and failed: {error}"
        )))
    );
}
