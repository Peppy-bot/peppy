//! The bridges between each catalog entry and the contract member behind
//! it, the one validation bound the entry to: a codec per message side
//! laid out when the process starts, and the type-erased clients driven at
//! request time against the producers the launcher bound to the entry's
//! target.

use crate::serve::ServeError;
use config::node::{MessageFormat, NativeExposedAction};
use message_codec::MessageCodec;
use message_codec::consumer::{
    ActionClient, ConsumerError, ConsumerIdentity, GoalHandle, GoalOutcome, MemberBinding,
    ServiceClient, TopicConsumer,
};
use peppy_mcp_catalog::{BundleContractPin, ExposureBundle, GoalBound, ValidatedExposure};
use peppy_mcp_runtime::{
    ActionContext, ActionExit, CancelledGoal, Recipient, ResourceIngest, ToolCall, ToolCallError,
};
use peppylib::config::QoSProfile;
use peppylib::messaging::{CancelState, MessengerHandle, ProducerRef, SenderTarget};
use peppylib::runtime::NodeRunner;
use serde_json::Value;
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::{Instant, Sleep};

#[cfg(test)]
mod tests;

/// One exposure ready to serve: its catalog and the bridge behind every
/// entry.
pub(crate) struct PreparedExposure {
    pub bundle: ExposureBundle,
    pub resources: Vec<PreparedResource>,
    pub tools: Vec<PreparedTool>,
    pub tasks: Vec<PreparedTask>,
}

/// Which member of which slot an entry reaches: the target the launcher
/// bound, the contract the slot pins, the member's name in it.
#[derive(Debug, Clone)]
pub(crate) struct Binding {
    /// The contract slot's link id, which the launcher's `links` filled.
    pub target: String,
    /// The contract as producers serve it.
    pub contract: SenderTarget,
    pub member: String,
}

impl Binding {
    /// The wire binding of the member the launcher bound to the slot.
    fn member_binding(&self) -> MemberBinding {
        MemberBinding {
            target: self.contract.clone(),
            member: self.member.clone(),
        }
    }
}

#[derive(Clone)]
pub(crate) struct PreparedResource {
    pub name: String,
    pub binding: Binding,
    pub qos: QoSProfile,
    pub codec: MessageCodec,
}

pub(crate) struct PreparedTool {
    pub name: String,
    pub binding: Binding,
    pub client: ServiceClient,
    pub deadline: Duration,
}

pub(crate) struct PreparedTask {
    pub name: String,
    pub binding: Binding,
    pub client: ActionClient,
    pub feedback_qos: QoSProfile,
    /// How the bridge follows the goal to its end (see [`drive_goal`]).
    pub follow: GoalFollow,
}

/// How the bridge follows a goal to its end: the tool's bound, together
/// with what the bound needs from the action.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum GoalFollow {
    /// A whole-goal deadline. `reports_feedback` says whether the action
    /// declares a feedback message. A goal on such an action settles once
    /// the provider closes the stream at the terminal result; a
    /// feedback-less action's stream carries nothing, not even that close,
    /// so a goal on it settles on a parked result request.
    WholeGoal {
        deadline: Duration,
        reports_feedback: bool,
    },
    /// A progress window, which only an action that declares a feedback
    /// message takes: its messages, and the close of its stream at the
    /// terminal result, are the signs of progress.
    Progress { window: Duration },
}

impl GoalFollow {
    /// How to follow a goal under `bound` on an action that declares a
    /// feedback message when `reports_feedback`. A progress bound on an
    /// action without one has no sign of progress to follow: `None`.
    fn new(bound: GoalBound, reports_feedback: bool) -> Option<Self> {
        match (bound, reports_feedback) {
            (GoalBound::WholeGoal { deadline_ms }, _) => Some(Self::WholeGoal {
                deadline: Duration::from_millis(deadline_ms.get()),
                reports_feedback,
            }),
            (GoalBound::Progress { window_ms }, true) => Some(Self::Progress {
                window: Duration::from_millis(window_ms.get()),
            }),
            (GoalBound::Progress { .. }, false) => None,
        }
    }
}

impl Binding {
    /// The binding of `member` through `slot`, the contract slot the
    /// launcher's `links` fill.
    fn new(slot: &BundleContractPin, member: &str) -> Result<Self, ServeError> {
        let contract =
            SenderTarget::contract(&slot.name, &slot.tag).map_err(peppylib::PeppyError::from)?;
        Ok(Self {
            target: slot.link_id.clone(),
            contract,
            member: member.to_owned(),
        })
    }
}

/// The codecs of one deployment, laid out once per member side: the same
/// member of the same pinned contract has one wire format however many
/// exposures reach it, and each layout is a run of the schema compiler.
#[derive(Default)]
struct Codecs {
    laid_out: HashMap<String, MessageCodec>,
}

impl Codecs {
    /// The codec of the side `key` names, laid out on first use.
    fn lay_out(
        &mut self,
        key: String,
        label: &str,
        format: &MessageFormat,
    ) -> Result<MessageCodec, ServeError> {
        if let Some(codec) = self.laid_out.get(&key) {
            return Ok(codec.clone());
        }
        let codec =
            MessageCodec::new(label, format.clone()).map_err(|source| ServeError::Codec {
                member: label.to_owned(),
                source,
            })?;
        self.laid_out.insert(key, codec.clone());
        Ok(codec)
    }

    /// The codec of an optional side; an absent or empty format is no
    /// payload at all.
    fn optional(
        &mut self,
        key: String,
        label: &str,
        format: Option<&MessageFormat>,
    ) -> Result<Option<MessageCodec>, ServeError> {
        match format.filter(|format| !format.0.is_empty()) {
            Some(format) => self.lay_out(key, label, format).map(Some),
            None => Ok(None),
        }
    }
}

/// The memo key of one message side of a member: the contract's pinned
/// identity, the member, the side.
fn side_key(slot: &BundleContractPin, member: &str, side: &str) -> String {
    format!("{}:{}/{member}/{side}", slot.name, slot.tag)
}

/// Prepares every exposure: for each catalog entry, the codecs of the
/// contract member validation bound it to.
pub(crate) fn prepare(
    exposures: Vec<ValidatedExposure>,
) -> Result<Vec<PreparedExposure>, ServeError> {
    let mut codecs = Codecs::default();
    let mut prepared = Vec::with_capacity(exposures.len());
    for exposure in exposures {
        let mut resources = Vec::with_capacity(exposure.bundle.resources.len());
        for (entry, bound) in exposure.resources() {
            let topic = &bound.member;
            let label = format!("{}_{}_topic", entry.target, entry.member);
            let format = topic.message_format.clone().unwrap_or_default();
            let codec = codecs.lay_out(
                side_key(&bound.slot, &entry.member, "topic"),
                &label,
                &format,
            )?;
            resources.push(PreparedResource {
                name: entry.name.clone(),
                binding: Binding::new(&bound.slot, &entry.member)?,
                qos: topic.qos_profile.clone(),
                codec,
            });
        }

        let mut tools = Vec::with_capacity(exposure.bundle.tools.len());
        for (entry, bound) in exposure.tools() {
            let service = &bound.member;
            let label = format!("{}_{}", entry.target, entry.member);
            tools.push(PreparedTool {
                name: entry.name.clone(),
                binding: Binding::new(&bound.slot, &entry.member)?,
                client: ServiceClient::new(
                    codecs.optional(
                        side_key(&bound.slot, &entry.member, "request"),
                        &format!("{label}_request"),
                        service.request_message_format.as_ref(),
                    )?,
                    codecs.optional(
                        side_key(&bound.slot, &entry.member, "response"),
                        &format!("{label}_response"),
                        service.response_message_format.as_ref(),
                    )?,
                ),
                deadline: Duration::from_millis(entry.deadline_ms.get()),
            });
        }

        let mut tasks = Vec::with_capacity(exposure.bundle.tasks.len());
        for (entry, bound) in exposure.tasks() {
            let action = &bound.member;
            let label = format!("{}_{}", entry.target, entry.member);
            let feedback = codecs.optional(
                side_key(&bound.slot, &entry.member, "feedback"),
                &format!("{label}_feedback"),
                action
                    .feedback_topic
                    .as_ref()
                    .map(|feedback| &feedback.message_format),
            )?;
            let follow = GoalFollow::new(entry.bound, feedback.is_some()).ok_or_else(|| {
                ServeError::ProgressWithoutFeedback {
                    tool: entry.name.clone(),
                }
            })?;
            tasks.push(PreparedTask {
                name: entry.name.clone(),
                binding: Binding::new(&bound.slot, &entry.member)?,
                client: ActionClient::new(
                    codecs.optional(
                        side_key(&bound.slot, &entry.member, "goal"),
                        &format!("{label}_goal"),
                        action
                            .goal_service
                            .as_ref()
                            .and_then(|goal| goal.request_message_format.as_ref()),
                    )?,
                    feedback,
                    codecs.optional(
                        side_key(&bound.slot, &entry.member, "result"),
                        &format!("{label}_result"),
                        action
                            .result_service
                            .as_ref()
                            .and_then(|result| result.response_message_format.as_ref()),
                    )?,
                ),
                feedback_qos: feedback_qos(action),
                follow,
            });
        }

        prepared.push(PreparedExposure {
            bundle: exposure.bundle,
            resources,
            tools,
            tasks,
        });
    }
    Ok(prepared)
}

/// The feedback subscription follows the contract: the QoS profile the
/// action's feedback topic declares picks the subscriber's buffering tier,
/// and a feedback-less action's empty stream takes the default.
fn feedback_qos(action: &NativeExposedAction) -> QoSProfile {
    action
        .feedback_topic
        .as_ref()
        .map(|topic| topic.qos_profile.clone())
        .unwrap_or_default()
}

/// Feeds the resources of one catalog entry from its topic: every message
/// admitted by the update-rate gate is decoded and offered to the resource
/// `ingest_of` picks for the producer that published it. The subscription
/// follows the target's bound set, one wire subscription per bound
/// producer, and lives as long as the node.
pub(crate) async fn pump_resource(
    node_runner: Arc<NodeRunner>,
    resource: PreparedResource,
    ingest_of: impl Fn(&ProducerRef) -> Option<ResourceIngest>,
) {
    let subscription = match peppylib::runtime::subscribe_bound_set(
        &node_runner,
        &resource.binding.target,
        resource.binding.contract.clone(),
        &resource.binding.member,
        resource.qos.clone(),
    )
    .await
    {
        Ok(subscription) => subscription,
        Err(error) => {
            tracing::warn!(
                %error,
                resource = resource.name,
                "subscription failed; the resource stays unavailable"
            );
            return;
        }
    };
    let mut subscription = TopicConsumer::new(subscription, resource.codec.clone());
    while let Some((producer, message)) = subscription.next_message().await {
        // A member the host has not registered yet has nowhere to hold a
        // snapshot; its next message lands once it has.
        let Some(ingest) = ingest_of(&producer) else {
            continue;
        };
        if feed(&ingest, || subscription.decode(&message)) {
            // The decode and the transcode ran on this thread of the
            // runtime: a reader the publish woke gets its turn before the
            // next message is looked at.
            tokio::task::yield_now().await;
        }
    }
}

/// Offers one message to a resource: the update-rate gate runs before any
/// conversion or transcoding, then the decoded snapshot meets the
/// resource's policies. Whether the message was admitted, and so decoded.
fn feed<E: std::fmt::Display>(
    ingest: &ResourceIngest,
    decode: impl FnOnce() -> Result<serde_json::Value, E>,
) -> bool {
    let Some(token) = ingest.admit() else {
        return false;
    };
    match decode() {
        Ok(value) => {
            if let Err(error) = ingest.publish(token, value) {
                tracing::debug!(%error, resource = ingest.resource_name(), "snapshot refused by policy");
            }
        }
        Err(error) => {
            tracing::debug!(%error, resource = ingest.resource_name(), "message does not convert")
        }
    }
    true
}

/// The producer a call goes to: the member the runtime routed it to on a
/// per-robot surface, the producer the launcher bound on a fixed one.
fn producer_of(node_runner: &NodeRunner, target: &str, recipient: Recipient) -> ProducerRef {
    match recipient {
        Recipient::Member(member) => ProducerRef::new(member.core_node, member.instance_id),
        Recipient::BoundProducer => node_runner.processor().sole_bound_producer(target).clone(),
    }
}

/// Calls the service behind a tool on the producer the call goes to.
pub(crate) async fn call_tool(
    tool: &PreparedTool,
    node_runner: &NodeRunner,
    identity: &ConsumerIdentity,
    call: ToolCall,
) -> Result<Value, ToolCallError> {
    let binding = tool.binding.member_binding();
    let producer = producer_of(node_runner, &tool.binding.target, call.recipient);
    tool.client
        .call(
            node_runner.messenger(),
            identity,
            &binding,
            &producer,
            &call.input,
            tool.deadline,
        )
        .await
        .map_err(|error| ToolCallError::Failed(error.to_string()))
}

/// The runtime-side surface a goal drives while it runs: its feedback
/// reaches the client through it (as the task's status message, or as
/// progress on the call the goal runs in), and the client's cancel request
/// reaches the provider through it.
pub(crate) trait TaskSurface: Sync {
    fn report_feedback(&self, message: String);

    /// Resolves once the client has requested cancellation (immediately, if
    /// it already has).
    fn cancel_requested(&self) -> impl Future<Output = ()> + Send;
}

impl TaskSurface for ActionContext {
    fn report_feedback(&self, message: String) {
        ActionContext::report_feedback(self, message);
    }

    fn cancel_requested(&self) -> impl Future<Output = ()> + Send {
        ActionContext::cancel_requested(self)
    }
}

/// Runs the action behind an action-backed tool on the producer the call
/// goes to.
pub(crate) async fn run_task(
    task: &PreparedTask,
    node_runner: &NodeRunner,
    identity: &ConsumerIdentity,
    call: ToolCall,
    context: ActionContext,
) -> Result<Value, ActionExit> {
    let binding = task.binding.member_binding();
    let producer = producer_of(node_runner, &task.binding.target, call.recipient);
    drive_goal(
        task,
        node_runner.messenger(),
        identity,
        &binding,
        &producer,
        call.input,
        &context,
    )
    .await
}

/// Drives the goal behind an action-backed tool: fires it at `producer`,
/// settles it on the provider's terminal result, and maps that result onto
/// the MCP terminal state. Cancellation is cooperative on both sides: the
/// client's cancel request is forwarded once to the Peppy cancel path, and
/// the terminal result the provider settles on decides the terminal state.
/// A goal that ends cancelled, whoever cancelled it, carries the result the
/// provider ended it with (see [`terminal_state`]).
///
/// The tool's bound decides how long the bridge follows the goal:
///
/// - A whole-goal deadline is spent by every await from the moment the goal
///   is fired, never restarted: a provider that keeps sending feedback, or
///   a cancel that takes its own time, cannot push the bridge past the
///   deadline the tool advertises. At the deadline the bridge stops
///   following the goal and sends no cancel.
/// - A progress window bounds the silence, not the work: the goal may run
///   as long as it keeps showing signs of progress, and goes at most one
///   window without one (see [`follow_progress`]).
///
/// Under either bound, the admission of the goal is the first wait, and it
/// is bounded too: by the whole deadline, or by one window. When it runs
/// out, the goal fails with the admission error and no cancel is sent: the
/// bridge has no handle on a goal the provider has not admitted yet. A
/// provider that admits the goal after that runs it with no one following
/// it, until it ends or the provider cancels it for a reason of its own.
pub(crate) async fn drive_goal(
    task: &PreparedTask,
    messenger: &MessengerHandle,
    identity: &ConsumerIdentity,
    binding: &MemberBinding,
    producer: &ProducerRef,
    input: Value,
    surface: &impl TaskSurface,
) -> Result<Value, ActionExit> {
    let started = Instant::now();
    // Admission is the first wait under either bound: the whole deadline,
    // or one window before the admission reply shows the goal is alive.
    let admission = match task.follow {
        GoalFollow::WholeGoal { deadline, .. } => deadline,
        GoalFollow::Progress { window } => window,
    };
    let mut handle = task
        .client
        .fire_goal(
            messenger,
            identity,
            binding,
            producer,
            &input,
            task.feedback_qos.clone(),
            admission,
        )
        .await
        .map_err(failed)?;
    if !handle.accepted() {
        return Err(ActionExit::Failed(match handle.rejection_reason() {
            Some(reason) => format!("the provider rejected the goal: {reason}"),
            None => "the provider rejected the goal".to_owned(),
        }));
    }

    match task.follow {
        GoalFollow::WholeGoal {
            deadline,
            reports_feedback,
        } => {
            let remaining = || deadline.saturating_sub(started.elapsed());
            let outcome = if reports_feedback {
                settle_after_feedback(&mut handle, messenger, surface, &remaining).await
            } else {
                settle_on_result(&handle, messenger, surface, &remaining).await
            }
            .map_err(failed)?;
            terminal_state(outcome, None)
        }
        GoalFollow::Progress { window } => {
            follow_progress(&mut handle, messenger, surface, window).await
        }
    }
}

/// The failure of a goal the bridge could not follow to its end.
fn failed(error: ConsumerError) -> ActionExit {
    ActionExit::Failed(error.to_string())
}

/// The MCP terminal state of a goal's terminal outcome. A goal that ended
/// cancelled carries the result its provider ended it with, and
/// `cancel_reason`, why the bridge cancelled it when the bridge did so for
/// a reason of its own.
fn terminal_state(
    outcome: GoalOutcome,
    cancel_reason: Option<String>,
) -> Result<Value, ActionExit> {
    match outcome {
        GoalOutcome::Completed(value) => Ok(value),
        GoalOutcome::Cancelled(result) => Err(ActionExit::Cancelled(CancelledGoal {
            result,
            reason: cancel_reason,
        })),
        GoalOutcome::Abandoned => Err(ActionExit::Failed(
            "the provider abandoned the goal".to_owned(),
        )),
        GoalOutcome::Expired => Err(ActionExit::Failed(
            "the goal ended, but its result is no longer readable".to_owned(),
        )),
    }
}

/// One step of a goal's feedback stream.
#[derive(Debug, Clone, Copy, PartialEq)]
enum FeedbackStep {
    /// A message, shown to the client.
    Shown,
    /// A message that does not convert: logged, and not shown to the client.
    Skipped,
    /// The stream ended: the goal settled, or its provider is gone.
    Ended,
}

/// Takes the next message of a goal's feedback stream and shows it to the
/// client. A message that does not convert is one the provider sent for the
/// goal all the same: it is logged and skipped, and the stream goes on.
async fn next_feedback_step(handle: &mut GoalHandle, surface: &impl TaskSurface) -> FeedbackStep {
    match handle.next_feedback().await {
        Ok(Some(value)) => {
            surface.report_feedback(value.to_string());
            FeedbackStep::Shown
        }
        Err(ConsumerError::Conversion(error)) => {
            tracing::warn!(
                %error,
                goal_id = handle.goal_id(),
                "feedback that does not convert is not shown to the client"
            );
            FeedbackStep::Skipped
        }
        Ok(None) | Err(ConsumerError::Messaging(_)) => FeedbackStep::Ended,
    }
}

/// Settles a goal on an action with feedback under a whole-goal deadline:
/// every message the goal's handle yields is shown to the client (see
/// [`next_feedback_step`]) until the provider closes the stream at the
/// terminal result (or disappears), then the result reply decides the
/// outcome.
async fn settle_after_feedback(
    handle: &mut GoalHandle,
    messenger: &MessengerHandle,
    surface: &impl TaskSurface,
    remaining: &impl Fn() -> Duration,
) -> Result<GoalOutcome, ConsumerError> {
    let mut cancel_pending = false;
    let mut cancel_forwarded = false;
    // Armed once: the deadline bounds the whole goal, so no message moves
    // it.
    let expiry = tokio::time::sleep(remaining());
    tokio::pin!(expiry);
    loop {
        if cancel_pending {
            cancel_pending = false;
            cancel_forwarded = true;
            let _ = handle.cancel(messenger, remaining()).await;
        }
        tokio::select! {
            _ = &mut expiry => break,
            _ = surface.cancel_requested(), if !cancel_forwarded => {
                cancel_pending = true;
            }
            step = next_feedback_step(handle, surface) => match step {
                FeedbackStep::Shown | FeedbackStep::Skipped => {}
                FeedbackStep::Ended => break,
            }
        }
    }
    handle.result(messenger, remaining()).await
}

/// Settles a goal on a feedback-less action under a whole-goal deadline:
/// its stream carries nothing, not even a close at the terminal result, so
/// the result request is parked on the provider from the start and its
/// reply decides the outcome. The request stays parked while a cancel is
/// forwarded; the provider settling the goal cancelled is what answers it.
async fn settle_on_result(
    handle: &GoalHandle,
    messenger: &MessengerHandle,
    surface: &impl TaskSurface,
    remaining: &impl Fn() -> Duration,
) -> Result<GoalOutcome, ConsumerError> {
    let result = handle.result(messenger, remaining());
    tokio::pin!(result);
    let mut cancel_forwarded = false;
    loop {
        tokio::select! {
            outcome = &mut result => return outcome,
            _ = surface.cancel_requested(), if !cancel_forwarded => {
                cancel_forwarded = true;
                let _ = handle.cancel(messenger, remaining()).await;
            }
        }
    }
}

/// The time since a progress-bound goal last showed a sign of progress,
/// which may last one window at most.
struct Silence {
    window: Duration,
    expiry: Pin<Box<Sleep>>,
}

impl Silence {
    /// A silence that starts now.
    fn new(window: Duration) -> Self {
        Self {
            window,
            expiry: Box::pin(tokio::time::sleep(window)),
        }
    }

    /// A sign of progress ends the silence: the next window starts now.
    fn restart(&mut self) {
        self.expiry.as_mut().reset(Instant::now() + self.window);
    }

    /// Resolves once the silence has lasted a whole window.
    async fn lasted_a_window(&mut self) {
        self.expiry.as_mut().await;
    }
}

/// What the bridge says of a goal that went `window` without a sign of
/// progress.
fn no_progress_within(window: Duration) -> String {
    format!("no progress within {} ms", window.as_millis())
}

/// The failure of a goal that went `window` without a sign of progress,
/// after which `what` happened.
fn stalled(window: Duration, what: &str) -> ActionExit {
    ActionExit::Failed(format!("{}; {what}", no_progress_within(window)))
}

/// Why the bridge sent the provider its one cancel for a progress-bound
/// goal, which decides how the end of the goal is reported.
#[derive(Debug, Clone, Copy, PartialEq)]
enum CancelReason {
    /// The client asked: a cancelled end is the cancel it wanted.
    Client,
    /// A window went by without a sign of progress: a cancelled end is the
    /// stall the bridge cut short.
    Silence,
}

impl CancelReason {
    /// The failure of a goal whose cancel, sent for this reason, came to
    /// `what`.
    fn failure(self, window: Duration, what: &str) -> ActionExit {
        match self {
            Self::Client => ActionExit::Failed(what.to_owned()),
            Self::Silence => stalled(window, what),
        }
    }

    /// Why a goal that ended cancelled was cancelled, when it is more than
    /// the client knows: the client knows it asked, and the silence is the
    /// bridge's own reason.
    fn stated_reason(self, window: Duration) -> Option<String> {
        match self {
            Self::Client => None,
            Self::Silence => Some(no_progress_within(window)),
        }
    }
}

/// What the provider's reply to a cancel leaves the bridge to do.
#[derive(Debug, PartialEq)]
enum AfterCancel {
    /// The provider signalled the goal: the bridge follows it to its end.
    KeepFollowing,
    /// The goal had already ended: the bridge reads its result at once,
    /// whether or not the end of its feedback stream came through.
    ReadTheResult,
}

/// Reads the provider's reply to the cancel the bridge sent for `reason`.
/// A provider that does not know the goal, or a cancel that failed, ends
/// the follow with a failure that says so. A failed cancel is also logged:
/// the goal may still run on the provider.
fn after_cancel(
    reply: Result<CancelState, ConsumerError>,
    reason: CancelReason,
    window: Duration,
    goal_id: &str,
) -> Result<AfterCancel, ActionExit> {
    match reply {
        Ok(CancelState::Signalled) => Ok(AfterCancel::KeepFollowing),
        Ok(CancelState::AlreadyTerminal) => Ok(AfterCancel::ReadTheResult),
        Ok(CancelState::Unknown) => Err(reason.failure(
            window,
            "a cancel was sent and the provider does not know the goal",
        )),
        Err(error) => {
            tracing::warn!(
                %error,
                goal_id,
                ?reason,
                "the cancel of a goal failed; the provider may still run it"
            );
            Err(reason.failure(window, &format!("a cancel was sent and failed: {error}")))
        }
    }
}

/// Sends the provider the bridge's one cancel of the goal, for `reason`,
/// and reads its reply (see [`after_cancel`]). The round trip is bounded by
/// one window.
async fn cancel_goal(
    handle: &GoalHandle,
    messenger: &MessengerHandle,
    reason: CancelReason,
    window: Duration,
) -> Result<AfterCancel, ActionExit> {
    let reply = handle.cancel(messenger, window).await;
    after_cancel(reply, reason, window, handle.goal_id())
}

/// Follows a goal bounded by its progress, `window` at a time. The signs of
/// progress are the admission reply (the goal is admitted when this
/// starts), each feedback message, and the end of the feedback stream;
/// each one starts a new window, and the result request after the end of
/// the stream gets one window of its own. The window restarts on progress
/// only, never on a timer, so a provider that stops moving is caught
/// whatever it did before. A feedback message that does not convert is
/// still one the provider sent for the goal: it starts a new window, and
/// the client is not shown it.
///
/// When a window goes by without a sign of progress, the bridge sends the
/// provider one cancel, then keeps following the goal under the same rule
/// to learn how it really ended: a completed result is the tool's result,
/// a cancelled end is reported with the provider's result and the silence
/// as the reason for the cancel, and one more silent window fails the tool
/// as a stall the provider did not answer. The bridge sends one cancel at
/// most, for the client or for the silence, and each cancel round trip is
/// bounded by one window while the silence keeps running: a cancel
/// acknowledgement is no progress of the goal. The reply to the cancel is
/// read (see [`after_cancel`]): a goal that had already ended has its
/// result read at once, since the end of its stream is the one sign that
/// did not come through. A message that arrives as the window runs out is
/// progress all the same (see [`next_turn`]).
async fn follow_progress(
    handle: &mut GoalHandle,
    messenger: &MessengerHandle,
    surface: &impl TaskSurface,
    window: Duration,
) -> Result<Value, ActionExit> {
    let mut silence = Silence::new(window);
    let mut cancel: Option<CancelReason> = None;
    let mut went_silent = false;
    loop {
        let feedback = next_feedback_step(handle, surface);
        let reason = match next_turn(&mut silence, surface, cancel.is_none(), feedback).await {
            Turn::CancelRequested => CancelReason::Client,
            Turn::Feedback(FeedbackStep::Shown | FeedbackStep::Skipped) => {
                silence.restart();
                continue;
            }
            Turn::Feedback(FeedbackStep::Ended) => break,
            Turn::Silence if went_silent => {
                return Err(stalled(
                    window,
                    "a cancel was sent and the provider did not answer it",
                ));
            }
            Turn::Silence => {
                went_silent = true;
                silence.restart();
                CancelReason::Silence
            }
        };
        // The one cancel went out for the client already.
        if cancel.is_some() {
            continue;
        }
        cancel = Some(reason);
        match cancel_goal(handle, messenger, reason, window).await? {
            AfterCancel::KeepFollowing => {}
            AfterCancel::ReadTheResult => break,
        }
    }
    let outcome = handle.result(messenger, window).await.map_err(failed)?;
    terminal_state(
        outcome,
        cancel.and_then(|reason| reason.stated_reason(window)),
    )
}

/// What comes next while the bridge follows a progress-bound goal.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Turn {
    /// The client asked for the goal to be cancelled.
    CancelRequested,
    /// The goal's feedback stream moved.
    Feedback(FeedbackStep),
    /// A whole window went by without a sign of progress.
    Silence,
}

/// Waits for what comes next: the client's cancel request (watched while
/// `watch_cancel`), the step `feedback` takes, or the end of the window.
/// When several are ready at once, they are taken in that order. A message
/// delivered before the bridge looks at it thus wins over a window that ran
/// out meanwhile, for example while the bridge waited on the client or on
/// its host: the goal showed progress, and is not cancelled for a silence.
/// Taking the silence last cannot hide a stall, because a stream that is
/// always ready is progress.
async fn next_turn(
    silence: &mut Silence,
    surface: &impl TaskSurface,
    watch_cancel: bool,
    feedback: impl Future<Output = FeedbackStep>,
) -> Turn {
    tokio::select! {
        biased;
        () = surface.cancel_requested(), if watch_cancel => Turn::CancelRequested,
        step = feedback => Turn::Feedback(step),
        () = silence.lasted_a_window() => Turn::Silence,
    }
}
