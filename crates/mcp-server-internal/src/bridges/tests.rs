//! The goal loop driven against a real provider: an ephemeral router, a
//! mock action server running peppylib's production goal engine, and the
//! bridge settling on each outcome a provider can reach. Every step waits
//! on the peer's action, never on the clock: the deadline only bounds the
//! failure path. The bound of a goal is tested on a bridge whose clock
//! moves only when the test advances it (see [`PausedBridge`]), and the
//! bridge behind a real endpoint over HTTP in [`loopback`].

use super::{
    AfterCancel, Binding, FeedbackStep, GoalFollow, PreparedTask, PreparedTool, Silence, Turn,
    after_cancel, call_service, drive_goal, next_turn, service_call_error,
};
use crate::test_support::{PausedRuntime, ScriptedSurface};
use config::failure_kind::{
    ContractFailureKinds, FailureKindName, MemberFailureKinds, PeppyFailureKind,
};
use config::node::MessageFormat;
use peppy_codec::MessageCodec;
use peppy_codec::consumer::{
    ActionClient, ConsumerError, ConsumerIdentity, MemberBinding, ServiceClient,
};
use peppy_mcp_catalog::GoalBound;
use peppy_mcp_runtime::{ActionExit, CancelledGoal, ProviderFailure, ToolCallError};
use peppylib::PeppyError;
use peppylib::messaging::{
    CancelState, DEFAULT_FEEDBACK_BOUND, Failure, FeedbackBound, GoalContext, MessengerHandle,
    NonEmptyPayload, ProducerRef, SenderTarget,
};
use peppylib::testing::{
    EphemeralRouter, MockActionServerCore, MockServiceCore, READINESS_TIMEOUT,
    wait_action_reachable, wait_service_reachable,
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
    provider_messenger: MessengerHandle,
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
            provider_messenger,
            _router: router,
            contract,
            producer,
        },
        provider,
    )
}

fn codec(label: &str, format: Value) -> MessageCodec {
    let format: MessageFormat = serde_json::from_value(format).expect("the format parses");
    MessageCodec::new(label, &format).expect("the format lays out")
}

/// The result of the gripper action: the width the gripper reached.
fn result_codec() -> MessageCodec {
    codec("move_gripper_result", json!({ "width": "f64" }))
}

/// The kind the gripper's contract declares of its own.
const UNREACHABLE: &str = "unreachable";

/// The kinds the gripper's member can give: the kinds of peppy, and
/// [`UNREACHABLE`], a kind of its contract that it lists.
fn gripper_failure_kinds() -> MemberFailureKinds {
    MemberFailureKinds::of_member(
        ContractFailureKinds::new([(kind(UNREACHABLE), "No plan to the width".to_owned())])
            .expect("a valid failure_kinds block"),
    )
}

fn kind(name: &str) -> FailureKindName {
    FailureKindName::parse(name).expect("a valid failure kind name")
}

/// The failure the provider gives a goal it cannot reach the width of.
fn unreachable_width() -> Failure {
    Failure::new(kind(UNREACHABLE), "no plan reaches 0.09 m")
}

/// [`unreachable_width`] as the bridge hands it to the runtime.
fn unreachable_width_for_the_client() -> ProviderFailure {
    ProviderFailure::new(kind(UNREACHABLE), "no plan reaches 0.09 m")
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
        client: ActionClient::new(
            None,
            feedback,
            Some(result_codec()),
            gripper_failure_kinds(),
        ),
        feedback_bound: DEFAULT_FEEDBACK_BOUND,
        moves_robot: false,
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
        None,
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
                None,
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

/// The result of a goal that completed.
fn completed_result() -> Value {
    json!({ "width": 0.04 })
}

fn reached_width() -> Payload {
    encoded(&result_codec(), completed_result())
}

/// The result the provider ends a cancelled goal with: where the gripper
/// stopped.
fn cancelled_result() -> Value {
    json!({ "width": 0.02 })
}

/// The reason the provider ends a cancelled goal with.
const PROVIDER_REASON: &str = "the gripper stopped on the cancel";

/// Ends the goal of `context` cancelled, for [`PROVIDER_REASON`], with
/// [`cancelled_result`].
async fn end_cancelled(context: &GoalContext) {
    context
        .complete_cancelled(
            PROVIDER_REASON,
            encoded(&result_codec(), cancelled_result()),
        )
        .await
        .expect("the provider settles the goal cancelled");
}

/// How the bridge reports a goal the provider ended cancelled with
/// [`end_cancelled`]: with the provider's result and reason.
fn ended_cancelled() -> Result<Value, ActionExit> {
    Err(ActionExit::Cancelled(CancelledGoal {
        result: cancelled_result(),
        reason: PROVIDER_REASON.to_owned(),
    }))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_feedback_less_goal_settles_on_its_completed_result() {
    let (mesh, mut provider) = mesh(false).await;
    let task = prepared_task(&mesh, None);
    let surface = ScriptedSurface::new();

    let (outcome, _context) = tokio::join!(drive(&mesh, &task, &surface), async {
        let context = accepted_goal(&mut provider).await;
        context
            .complete(reached_width())
            .await
            .expect("the provider completes the goal");
        context
    });

    assert_eq!(outcome, Ok(completed_result()));
    assert_eq!(surface.feedback(), Vec::<String>::new());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_feedback_less_goal_settles_cancelled_once_the_provider_honors_the_cancel() {
    let (mesh, mut provider) = mesh(false).await;
    let task = prepared_task(&mesh, None);
    let surface = ScriptedSurface::new();

    let (outcome, _context) = tokio::join!(drive(&mesh, &task, &surface), async {
        let context = accepted_goal(&mut provider).await;
        // The client cancels while the goal runs: the bridge forwards it,
        // and the provider settles the goal cancelled once it observes it.
        surface.cancel.cancel();
        context.cancel_signal().await;
        end_cancelled(&context).await;
        context
    });

    assert_eq!(outcome, ended_cancelled());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_goal_with_feedback_reports_it_and_settles_on_its_result() {
    let (mesh, mut provider) = mesh(true).await;
    let feedback = percent_codec();
    let task = prepared_task(&mesh, Some(feedback.clone()));
    let surface = ScriptedSurface::new();

    let (outcome, _context) = tokio::join!(drive(&mesh, &task, &surface), async {
        let context = accepted_goal(&mut provider).await;
        context
            .publish_feedback(percent(&feedback, 50))
            .await
            .expect("the provider publishes feedback");
        context
            .complete(reached_width())
            .await
            .expect("the provider completes the goal");
        context
    });

    assert_eq!(outcome, Ok(completed_result()));
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

    /// The client cancels the goal, and the provider sees the cancel.
    async fn cancelled_by_the_client(&self) {
        self.surface.cancel.cancel();
        cancel_seen(&self.context).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_whole_goal_deadline_is_not_pushed_back_by_feedback_and_cancels_the_goal() {
    let (mesh, mut provider) = mesh(true).await;
    let surface = Arc::new(ScriptedSurface::new());
    let whole_goal = GoalFollow::WholeGoal {
        deadline: WINDOW,
        reports_feedback: true,
    };
    let mut bridge = drive_paused(
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

    let outcome = bridge.ended().await;
    assert!(
        matches!(outcome, Err(ActionExit::Error(_))),
        "the deadline fails the goal: {outcome:?}"
    );
    // The bridge stopped following the goal, so the provider gets a cancel.
    cancel_seen(&context).await;
}

/// The runtime drops the future of a bridge whose goal outlasts its
/// whole-goal deadline or the TTL of its task: the provider then gets a
/// cancel, so the goal does not run on with no one following it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_bridge_dropped_while_it_follows_its_goal_cancels_the_goal() {
    let (mesh, mut provider) = mesh(true).await;
    let task = prepared_task(&mesh, Some(percent_codec()));
    let surface = ScriptedSurface::new();
    let mut bridge = Box::pin(drive(&mesh, &task, &surface));

    let context = tokio::select! {
        outcome = &mut bridge => panic!("the goal runs until the bridge is dropped: {outcome:?}"),
        context = async {
            let context = accepted_goal(&mut provider).await;
            surface.followed().await;
            context
        } => context,
    };
    assert!(!context.is_cancelled(), "the bridge follows the goal");
    drop(bridge);

    cancel_seen(&context).await;
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
        end_cancelled(&context).await;
        context
    });

    assert_eq!(outcome, ended_cancelled());
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
    // A silence would end the follow: once this message is shown, no window
    // went by without a sign of progress.
    publish_percent(&goal.context, 20).await;
    goal.surface.reported(2).await;
    goal.context
        .complete(reached_width())
        .await
        .expect("the provider completes the goal");

    assert_eq!(goal.bridge.outcome().await, Ok(completed_result()));
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
        .complete(reached_width())
        .await
        .expect("the provider completes the goal");

    assert_eq!(goal.bridge.outcome().await, Ok(completed_result()));
    assert_eq!(goal.surface.feedback().len(), 4);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_goal_silent_for_one_window_fails_the_call_and_gets_a_cancel() {
    let (mesh, mut provider) = mesh(true).await;
    let mut goal = ProgressGoal::reporting(&mesh, &mut provider).await;

    goal.bridge.advance(WINDOW).await;

    assert_eq!(
        goal.bridge.ended().await,
        Err(ActionExit::Error(
            "no progress within 60000 ms; the endpoint stopped following the goal and \
             cancelled it"
                .to_owned()
        ))
    );
    cancel_seen(&goal.context).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_provider_that_does_not_answer_the_clients_cancel_fails_the_call_one_window_later() {
    let (mesh, mut provider) = mesh(true).await;
    let goal = ProgressGoal::reporting(&mesh, &mut provider).await;

    goal.cancelled_by_the_client().await;
    // The bridge takes feedback again only once the provider's reply to the
    // cancel is in, and the reply is timed on the bridge's clock: the window
    // passes only after this message is shown. The provider reports once
    // more, then never ends the goal.
    publish_percent(&goal.context, 20).await;
    goal.surface.reported(2).await;
    goal.bridge.advance(WINDOW).await;

    assert_eq!(
        goal.bridge.outcome().await,
        Err(ActionExit::Error(
            "no progress within 60000 ms; a cancel was sent and the provider did not answer it"
                .to_owned()
        ))
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_goal_that_completes_after_the_clients_cancel_is_reported_with_its_result() {
    let (mesh, mut provider) = mesh(true).await;
    let goal = ProgressGoal::reporting(&mesh, &mut provider).await;

    goal.cancelled_by_the_client().await;
    goal.context
        .complete(reached_width())
        .await
        .expect("the provider completes the goal all the same");

    assert_eq!(goal.bridge.outcome().await, Ok(completed_result()));
}

/// The provider completes the goal, and no end of its feedback stream
/// reaches the bridge: a provider without a feedback publisher stands in
/// for a lost end. The client's cancel is answered with "already ended",
/// and the bridge reads the result at once.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_goal_that_ended_unseen_is_reported_with_its_result_once_the_cancel_says_so() {
    let (mesh, mut provider) = mesh(false).await;
    let goal = ProgressGoal::accepted(&mesh, &mut provider).await;
    goal.context
        .complete(reached_width())
        .await
        .expect("the provider completes the goal");

    goal.surface.cancel.cancel();

    assert_eq!(goal.bridge.outcome().await, Ok(completed_result()));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_goal_that_ended_unseen_with_its_result_gone_fails_saying_so() {
    let (mesh, provider) = mesh(false).await;
    let mut provider = provider.with_result_retention_grace(Duration::ZERO);
    let goal = ProgressGoal::accepted(&mesh, &mut provider).await;
    goal.context
        .complete(reached_width())
        .await
        .expect("the provider completes the goal");

    goal.surface.cancel.cancel();

    assert_eq!(
        goal.bridge.outcome().await,
        Err(ActionExit::Error(
            "the goal ended, but its result is no longer readable".to_owned()
        ))
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_client_cancel_ends_a_progress_bound_goal_cancelled() {
    let (mesh, mut provider) = mesh(true).await;
    let goal = ProgressGoal::reporting(&mesh, &mut provider).await;

    goal.cancelled_by_the_client().await;
    end_cancelled(&goal.context).await;

    assert_eq!(goal.bridge.outcome().await, ended_cancelled());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_refused_goal_ends_with_the_providers_failure_kind_and_message() {
    let (mesh, mut provider) = mesh(false).await;
    let task = prepared_task(&mesh, None);
    let surface = ScriptedSurface::new();

    let (outcome, ()) = tokio::join!(drive(&mesh, &task, &surface), async {
        provider
            .next_goal(READINESS_TIMEOUT)
            .await
            .expect("the goal reaches the provider")
            .refuse(unreachable_width())
            .await
            .expect("the provider refuses the goal");
    });

    assert_eq!(
        outcome,
        Err(ActionExit::Refused(unreachable_width_for_the_client()))
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_failed_goal_ends_with_the_providers_failure_kind_and_message() {
    let (mesh, mut provider) = mesh(true).await;
    let task = prepared_task(&mesh, Some(percent_codec()));
    let surface = ScriptedSurface::new();

    let (outcome, _context) = tokio::join!(drive(&mesh, &task, &surface), async {
        let context = accepted_goal(&mut provider).await;
        publish_percent(&context, 50).await;
        context
            .fail(Failure::new(
                PeppyFailureKind::ResourceUnavailable,
                "the gripper is gone",
            ))
            .await
            .expect("the provider fails the goal");
        context
    });

    assert_eq!(
        outcome,
        Err(ActionExit::Failed(ProviderFailure::new(
            PeppyFailureKind::ResourceUnavailable,
            "the gripper is gone"
        )))
    );
    assert_eq!(surface.feedback(), vec![r#"{"percent":50}"#.to_owned()]);
}

/// What the bridge says of a goal that the provider refused or failed
/// with a kind that the gripper's member does not declare.
fn undeclared_kind() -> Result<Value, ActionExit> {
    Err(ActionExit::Error(format!(
        "the provider of `{MEMBER}` gave the failure kind `melted`, which is neither a kind of \
         peppy nor a kind of its contract that the member lists: the jaws melted"
    )))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_kind_the_member_does_not_declare_ends_the_goal_with_a_readable_error() {
    let (mesh, mut provider) = mesh(false).await;
    let task = prepared_task(&mesh, None);
    let surface = ScriptedSurface::new();
    let melted = || Failure::new(kind("melted"), "the jaws melted");

    let (refused, ()) = tokio::join!(drive(&mesh, &task, &surface), async {
        provider
            .next_goal(READINESS_TIMEOUT)
            .await
            .expect("the goal reaches the provider")
            .refuse(melted())
            .await
            .expect("the provider refuses the goal");
    });
    assert_eq!(refused, undeclared_kind());

    let (failed, _context) = tokio::join!(drive(&mesh, &task, &surface), async {
        let context = accepted_goal(&mut provider).await;
        context
            .fail(melted())
            .await
            .expect("the provider fails the goal");
        context
    });
    assert_eq!(failed, undeclared_kind());
}

/// The surface of a slow client: the report of the first message waits
/// until the test lets it go, as a report on a call waits while the client
/// has not taken the message before it, so the next messages wait unread
/// in the goal's buffer.
struct HeldSurface {
    surface: ScriptedSurface,
    hold: std::sync::Mutex<Option<tokio::sync::oneshot::Receiver<()>>>,
    held: tokio::sync::watch::Sender<bool>,
}

impl HeldSurface {
    /// The surface, and what lets the bridge go once it holds it.
    fn new() -> (Self, tokio::sync::oneshot::Sender<()>) {
        let (release, hold) = tokio::sync::oneshot::channel();
        let surface = Self {
            surface: ScriptedSurface::new(),
            hold: std::sync::Mutex::new(Some(hold)),
            held: tokio::sync::watch::Sender::new(false),
        };
        (surface, release)
    }

    /// Returns once the surface holds the bridge.
    async fn holds_the_bridge(&self) {
        let mut held = self.held.subscribe();
        tokio::time::timeout(READINESS_TIMEOUT, held.wait_for(|held| *held))
            .await
            .expect("the bridge reports the first message")
            .expect("the surface outlives the wait");
    }
}

impl super::TaskSurface for HeldSurface {
    async fn report_feedback(&self, message: String) {
        self.surface.report_feedback(message).await;
        let hold = self.hold.lock().expect("the hold is never poisoned").take();
        let Some(hold) = hold else {
            return;
        };
        self.held.send_replace(true);
        hold.await.expect("the test lets the bridge go");
    }

    fn cancel_requested(&self) -> impl std::future::Future<Output = ()> + Send {
        self.surface.cancel_requested()
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_slow_client_makes_the_goal_overflow_and_the_call_fail_with_the_loss() {
    let (mesh, mut provider) = mesh(true).await;
    let task = PreparedTask {
        feedback_bound: FeedbackBound::new(std::num::NonZeroUsize::MIN),
        ..prepared_task(&mesh, Some(percent_codec()))
    };
    let (surface, release) = HeldSurface::new();
    let surface = Arc::new(surface);
    let bridge = tokio::spawn({
        let messenger = mesh.bridge_messenger.clone();
        let binding = member_binding(&mesh);
        let producer = mesh.producer.clone();
        let surface = Arc::clone(&surface);
        async move {
            drive_goal(
                &task,
                &messenger,
                &bridge_identity(),
                &binding,
                &producer,
                json!({}),
                surface.as_ref(),
                None,
            )
            .await
        }
    });
    let context = accepted_goal(&mut provider).await;
    publish_percent(&context, 10).await;
    surface.holds_the_bridge().await;

    // The bridge reads no more feedback while the client holds the first
    // message. The bound keeps one unread message: the second one overflows
    // it, and the goal's handle cancels the goal.
    publish_percent(&context, 20).await;
    publish_percent(&context, 30).await;
    cancel_seen(&context).await;
    release.send(()).expect("the bridge waits for the release");
    end_cancelled(&context).await;

    let outcome = tokio::time::timeout(READINESS_TIMEOUT, bridge)
        .await
        .expect("the bridge ends the call")
        .expect("the bridge does not panic");
    assert_eq!(
        outcome,
        Err(ActionExit::Error(
            "the endpoint fell behind the feedback of the goal by more than 1 unread messages, \
             so it cancelled the goal"
                .to_owned()
        ))
    );
    assert_eq!(
        surface.surface.feedback(),
        vec![r#"{"percent":10}"#.to_owned()],
        "the messages past the bound are lost, and the call says so"
    );
}

/// The service of the gripper's contract: whether a width is within reach.
const SERVICE: &str = "check_width";

/// The tool of [`SERVICE`] as `prepare` lays it out.
fn prepared_tool(contract: &SenderTarget) -> PreparedTool {
    PreparedTool {
        name: "openarm.check_width".to_owned(),
        binding: Binding {
            target: LINK_ID.to_owned(),
            contract: contract.clone(),
            member: SERVICE.to_owned(),
        },
        client: ServiceClient::new(
            None,
            Some(codec("check_width_response", json!({ "width": "f64" }))),
            gripper_failure_kinds(),
        ),
        deadline: DEADLINE,
        moves_robot: false,
    }
}

/// Exposes [`SERVICE`] on the mesh's provider session, returning once the
/// bridge's session can reach it.
async fn service_provider(mesh: &Mesh) -> MockServiceCore {
    let provider = MockServiceCore::listen(
        &mesh.provider_messenger,
        CORE_NODE,
        PROVIDER_INSTANCE,
        mesh.contract.clone(),
        SERVICE,
    )
    .await
    .expect("the provider exposes the service");
    wait_service_reachable(
        &mesh.bridge_messenger,
        CORE_NODE,
        BRIDGE_INSTANCE,
        mesh.contract.clone(),
        SERVICE,
        &mesh.producer,
        READINESS_TIMEOUT,
    )
    .await
    .expect("the service becomes reachable");
    provider
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_error_reply_gives_the_providers_failure_kind_and_an_undeclared_kind_is_an_error() {
    let (mesh, _actions) = mesh(false).await;
    let provider = service_provider(&mesh).await;
    let contract = mesh.contract.clone();
    let bridge_messenger = mesh.bridge_messenger.clone();
    let producer = mesh.producer.clone();
    let tool = prepared_tool(&contract);
    let binding = MemberBinding {
        target: contract.clone(),
        member: SERVICE.to_owned(),
    };
    let identity = bridge_identity();
    let input = json!({});
    let call = || {
        call_service(
            &tool,
            &bridge_messenger,
            &identity,
            &binding,
            &producer,
            &input,
        )
    };

    for (failure, expected) in [
        (
            unreachable_width(),
            ToolCallError::Failed(unreachable_width_for_the_client()),
        ),
        (
            Failure::new(kind("melted"), "the jaws melted"),
            ToolCallError::Error(format!(
                "the provider of `{SERVICE}` gave the failure kind `melted`, which is neither \
                 a kind of peppy nor a kind of its contract that the member lists: the jaws melted"
            )),
        ),
    ] {
        let (answer, ()) = tokio::join!(call(), async {
            let (_request, responder) = provider
                .next_request(READINESS_TIMEOUT)
                .await
                .expect("the call reaches the provider");
            responder
                .respond_failure(failure)
                .await
                .expect("the provider gives an error reply");
        });
        assert_eq!(answer, Err(expected));
    }
}

#[test]
fn a_service_call_error_says_whether_the_provider_answered() {
    let failure = || unreachable_width();
    assert_eq!(
        service_call_error(
            ConsumerError::Messaging(PeppyError::ServiceFailed {
                instance_id: None,
                service_name: SERVICE.to_owned(),
                failure: failure(),
            }),
            DEADLINE,
        ),
        ToolCallError::Failed(unreachable_width_for_the_client())
    );
    assert_eq!(
        service_call_error(
            ConsumerError::Messaging(PeppyError::ServiceTimeout {
                instance_id: None,
                service_name: SERVICE.to_owned(),
            }),
            DEADLINE,
        ),
        ToolCallError::Deadline("the provider did not answer within 10000 ms".to_owned())
    );
    let unreachable = PeppyError::ServiceUnreachable {
        instance_id: None,
        service_name: SERVICE.to_owned(),
    };
    let words = unreachable.to_string();
    assert_eq!(
        service_call_error(ConsumerError::Messaging(unreachable), DEADLINE),
        ToolCallError::Unavailable(words)
    );
}

/// A feedback message, as the stream yields it.
fn shown() -> FeedbackStep {
    FeedbackStep::Message(r#"{"percent":10}"#.to_owned())
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
        let turn = next_turn(&mut silence, &surface, true, std::future::ready(shown())).await;
        assert_eq!(turn, Turn::Feedback(shown()), "round {round}");
    }
}

#[tokio::test(start_paused = true)]
async fn the_client_s_cancel_comes_first_and_the_silence_last() {
    let surface = ScriptedSurface::new();
    surface.cancel.cancel();
    for round in 0..TURN_ROUNDS {
        let mut silence = Silence::new(WINDOW);
        tokio::time::advance(WINDOW).await;
        let delivered = || std::future::ready(shown());
        assert_eq!(
            next_turn(&mut silence, &surface, true, delivered()).await,
            Turn::CancelRequested,
            "round {round}: the client's cancel is taken before anything else"
        );
        assert_eq!(
            next_turn(&mut silence, &surface, false, delivered()).await,
            Turn::Feedback(shown()),
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
    assert_eq!(
        after_cancel(Ok(CancelState::Signalled), "goal"),
        Ok(AfterCancel::KeepFollowing)
    );
    assert_eq!(
        after_cancel(Ok(CancelState::AlreadyTerminal), "goal"),
        Ok(AfterCancel::ReadTheResult)
    );
}

#[test]
fn a_cancel_the_provider_cannot_act_on_ends_the_follow_saying_why() {
    assert_eq!(
        after_cancel(Ok(CancelState::Unknown), "goal"),
        Err(ActionExit::Error(
            "a cancel was sent and the provider does not know the goal".to_owned()
        ))
    );

    let unreachable = PeppyError::Io(std::io::Error::other("the provider is unreachable"));
    let error = unreachable.to_string();
    assert_eq!(
        after_cancel(Err(ConsumerError::Messaging(unreachable)), "goal"),
        Err(ActionExit::Error(format!(
            "a cancel was sent and failed: {error}"
        )))
    );
}
