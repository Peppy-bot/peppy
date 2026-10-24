//! The node bridges, between each catalog entry of a contract target and
//! the contract member behind it, the one validation bound the entry to: a
//! codec per message side laid out when the process starts, and the
//! type-erased clients driven at request time against the producers the
//! launcher bound to the entry's target. [`prepare`] hands the entries of a
//! daemon target to the daemon bridge (see [`crate::daemon_bridge`]).

use crate::daemon_bridge::{DaemonTask, DaemonTool};
use crate::serve::ServeError;
use config::node::MessageFormat;
use message_codec::MessageCodec;
use message_codec::consumer::{
    ActionClient, ConsumerError, ConsumerIdentity, GoalHandle, GoalOutcome, MemberBinding,
    ServiceClient, TopicConsumer,
};
use peppy_mcp_catalog::{
    BundleContractPin, ExposureBundle, GoalBound, MemberSource, ValidatedExposure,
};
use peppy_mcp_runtime::{
    ActionContext, ActionExit, CancelledGoal, ProviderFailure, Recipient, ResourceIngest, ToolCall,
    ToolCallError,
};
use peppylib::PeppyError;
use peppylib::config::{QoSProfile, TopicRetention};
use peppylib::messaging::{
    CancelOnDrop, CancelState, DEFAULT_FEEDBACK_BOUND, Failure, FeedbackBound, MessengerHandle,
    ProducerRef, SenderTarget,
};
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
/// entry. The entries of contract targets go to the node bridges of this
/// module, and the entries of daemon targets to the daemon bridge.
pub(crate) struct PreparedExposure {
    pub bundle: ExposureBundle,
    pub resources: Vec<PreparedResource>,
    pub tools: Vec<PreparedTool>,
    pub tasks: Vec<PreparedTask>,
    pub daemon_tools: Vec<DaemonTool>,
    pub daemon_tasks: Vec<DaemonTask>,
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
    /// How the bridge follows the goal to its end (see [`drive_goal`]).
    pub follow: GoalFollow,
    /// How many feedback messages of a goal may wait unread before the
    /// goal's handle cancels it: peppylib's default.
    pub feedback_bound: FeedbackBound,
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
    /// The codec of `side`, laid out on first use.
    fn lay_out(
        &mut self,
        side: &MemberSide<'_>,
        format: &MessageFormat,
    ) -> Result<MessageCodec, ServeError> {
        let key = side.memo_key();
        if let Some(codec) = self.laid_out.get(&key) {
            return Ok(codec.clone());
        }
        let label = side.label();
        let codec =
            MessageCodec::new(&label, format.clone()).map_err(|source| ServeError::Codec {
                member: label,
                source,
            })?;
        self.laid_out.insert(key, codec.clone());
        Ok(codec)
    }

    /// The codec of an optional side; an absent or empty format is no
    /// payload at all.
    fn optional(
        &mut self,
        side: &MemberSide<'_>,
        format: Option<&MessageFormat>,
    ) -> Result<Option<MessageCodec>, ServeError> {
        match format.filter(|format| !format.0.is_empty()) {
            Some(format) => self.lay_out(side, format).map(Some),
            None => Ok(None),
        }
    }
}

/// One message side of a contract member, as a catalog entry reaches it:
/// both the memo key and the label of its codec derive from it, so the two
/// always name the same side.
struct MemberSide<'a> {
    slot: &'a BundleContractPin,
    target: &'a str,
    member: &'a str,
    side: &'static str,
}

impl MemberSide<'_> {
    /// The memo key of the side: the contract's pinned identity, the
    /// member, the side.
    fn memo_key(&self) -> String {
        format!(
            "{}:{}/{}/{}",
            self.slot.name, self.slot.tag, self.member, self.side
        )
    }

    /// The label of the side's codec, which a layout error names.
    fn label(&self) -> String {
        format!("{}_{}_{}", self.target, self.member, self.side)
    }
}

/// Prepares every exposure: for each entry of a contract target, the
/// codecs of the contract member validation bound it to; for each entry of
/// a daemon target, the daemon function behind it.
pub(crate) fn prepare(
    exposures: Vec<ValidatedExposure>,
) -> Result<Vec<PreparedExposure>, ServeError> {
    let mut codecs = Codecs::default();
    let no_fields = MessageFormat::default();
    let mut prepared = Vec::with_capacity(exposures.len());
    for exposure in exposures {
        let mut resources = Vec::with_capacity(exposure.bundle.resources.len());
        for (entry, bound) in exposure.resources() {
            let slot = match &bound.source {
                MemberSource::Slot(slot) => slot,
                MemberSource::Daemon(daemon) => {
                    return Err(ServeError::DaemonResource {
                        resource: entry.name.clone(),
                        target: daemon.target.clone(),
                    });
                }
            };
            let topic = &bound.member;
            let side = MemberSide {
                slot,
                target: &entry.target,
                member: &entry.member,
                side: "topic",
            };
            let format = topic.message_format.as_ref().unwrap_or(&no_fields);
            let codec = codecs.lay_out(&side, format)?;
            resources.push(PreparedResource {
                name: entry.name.clone(),
                binding: Binding::new(slot, &entry.member)?,
                qos: topic.qos_profile.clone(),
                codec,
            });
        }

        let mut tools = Vec::with_capacity(exposure.bundle.tools.len());
        let mut daemon_tools = Vec::new();
        for (entry, bound) in exposure.tools() {
            let slot = match &bound.source {
                MemberSource::Slot(slot) => slot,
                MemberSource::Daemon(daemon) => {
                    daemon_tools.push(DaemonTool::new(entry, daemon)?);
                    continue;
                }
            };
            let service = &bound.member;
            let member_side = |side| MemberSide {
                slot,
                target: &entry.target,
                member: &entry.member,
                side,
            };
            tools.push(PreparedTool {
                name: entry.name.clone(),
                binding: Binding::new(slot, &entry.member)?,
                client: ServiceClient::new(
                    codecs.optional(
                        &member_side("request"),
                        service.request_message_format.as_ref(),
                    )?,
                    codecs.optional(
                        &member_side("response"),
                        service.response_message_format.as_ref(),
                    )?,
                    bound.failure_kinds.clone(),
                ),
                deadline: Duration::from_millis(entry.deadline_ms.get()),
            });
        }

        let mut tasks = Vec::with_capacity(exposure.bundle.tasks.len());
        let mut daemon_tasks = Vec::new();
        for (entry, bound) in exposure.tasks() {
            let slot = match &bound.source {
                MemberSource::Slot(slot) => slot,
                MemberSource::Daemon(daemon) => {
                    daemon_tasks.push(DaemonTask::new(entry, daemon)?);
                    continue;
                }
            };
            let action = &bound.member;
            let member_side = |side| MemberSide {
                slot,
                target: &entry.target,
                member: &entry.member,
                side,
            };
            let feedback = codecs.optional(
                &member_side("feedback"),
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
                binding: Binding::new(slot, &entry.member)?,
                client: ActionClient::new(
                    codecs.optional(
                        &member_side("goal"),
                        action
                            .goal_service
                            .as_ref()
                            .and_then(|goal| goal.request_message_format.as_ref()),
                    )?,
                    feedback,
                    codecs.optional(
                        &member_side("result"),
                        action
                            .result_service
                            .as_ref()
                            .and_then(|result| result.response_message_format.as_ref()),
                    )?,
                    bound.failure_kinds.clone(),
                ),
                follow,
                feedback_bound: DEFAULT_FEEDBACK_BOUND,
            });
        }

        prepared.push(PreparedExposure {
            bundle: exposure.bundle,
            resources,
            tools,
            tasks,
            daemon_tools,
            daemon_tasks,
        });
    }
    Ok(prepared)
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
        // A resource reads live traffic: the pump takes a message only once
        // its producer's ingest is registered.
        TopicRetention::LiveOnly,
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
    call_service(
        tool,
        node_runner.messenger(),
        identity,
        &binding,
        &producer,
        &call.input,
    )
    .await
}

/// Calls the service behind a tool on `producer` and waits at most the
/// tool's deadline for its answer: the response, or the tool error of a
/// call that failed (see [`service_call_error`]).
pub(crate) async fn call_service(
    tool: &PreparedTool,
    messenger: &MessengerHandle,
    identity: &ConsumerIdentity,
    binding: &MemberBinding,
    producer: &ProducerRef,
    input: &Value,
) -> Result<Value, ToolCallError> {
    tool.client
        .call(messenger, identity, binding, producer, input, tool.deadline)
        .await
        .map_err(|error| service_call_error(error, tool.deadline))
}

/// The tool error of a service call that failed with `error`, under the
/// tool's `deadline`: an error reply gives its failure kind and message, a
/// provider that cannot be reached is unavailable, a provider that does
/// not answer in time overran the deadline, and every other error, an
/// error reply with a kind that the member does not declare included, is
/// a readable text.
fn service_call_error(error: ConsumerError, deadline: Duration) -> ToolCallError {
    let ConsumerError::Messaging(messaging) = &error else {
        return ToolCallError::Error(error.to_string());
    };
    if let Some(failure) = messaging.failure() {
        return ToolCallError::Failed(provider_failure(failure));
    }
    match messaging {
        PeppyError::ServiceUnreachable { .. } => ToolCallError::Unavailable(error.to_string()),
        PeppyError::ServiceTimeout { .. } => ToolCallError::Deadline(format!(
            "the provider did not answer within {} ms",
            deadline.as_millis()
        )),
        _ => ToolCallError::Error(error.to_string()),
    }
}

/// The failure a provider gave, as the runtime carries it to the client.
fn provider_failure(failure: &Failure) -> ProviderFailure {
    ProviderFailure::new(failure.kind().clone(), failure.message())
}

/// The runtime-side surface a goal drives while it runs: its feedback
/// reaches the client through it (as the task's status message, or as
/// progress on the call the goal runs in), and the client's cancel request
/// reaches the provider through it.
pub(crate) trait TaskSurface: Sync {
    /// Shows a feedback message to the client. Resolves once the client's
    /// side takes it, at once on a task and on a call that closed. A bridge
    /// reads the next message of its goal only after this resolves, so the
    /// messages that the client is not ready for wait in the goal's feedback
    /// buffer (see [`ActionContext::report_feedback`]).
    fn report_feedback(&self, message: String) -> impl Future<Output = ()> + Send;

    /// Resolves once the client has requested cancellation (immediately, if
    /// it already has).
    fn cancel_requested(&self) -> impl Future<Output = ()> + Send;
}

impl TaskSurface for ActionContext {
    fn report_feedback(&self, message: String) -> impl Future<Output = ()> + Send {
        ActionContext::report_feedback(self, message)
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
/// provider ended it with (see [`terminal_state`]). A goal that the
/// provider refuses or fails carries its failure kind and message.
///
/// The server's node is the sender of the goal, and the server outlives
/// every call: so the bridge sends the provider a cancel whenever it stops
/// following the goal before the goal ended, and at most one cancel. The
/// client cancels the goal with `tasks/cancel` on a task, or by closing the
/// call the goal runs in. A deadline, a progress window that goes by in
/// silence, the TTL of a task and the stop of the endpoint end the follow:
/// the cancel then goes out as the follow ends, also when the runtime drops
/// the bridge's future (see [`FollowedGoal`]). A task's connection that
/// closes ends nothing.
///
/// The bridge reads the next feedback message of the goal once the client
/// took the one before (see [`TaskSurface::report_feedback`]). The goal's
/// handle keeps up to `task.feedback_bound` unread feedback messages. A
/// client that falls further behind loses the stream: the handle cancels
/// the goal, the bridge follows it to its end, and the call fails with the
/// loss (see [`feedback_overflowed`]).
///
/// The tool's bound decides how long the bridge follows the goal:
///
/// - A whole-goal deadline is spent by every await from the moment the goal
///   is fired, never restarted: a provider that keeps sending feedback, or
///   a cancel that takes its own time, cannot push the bridge past the
///   deadline the tool advertises. At the deadline the bridge stops
///   following the goal.
/// - A progress window bounds the silence, not the work: the goal may run
///   as long as it keeps showing signs of progress, and goes at most one
///   window without one (see [`follow_progress`]).
///
/// Under either bound, the admission of the goal is the first wait, and it
/// is bounded too: by the whole deadline, or by one window. When it runs
/// out, the goal fails with the admission error. The bridge has no handle
/// on a goal whose admission it did not read: peppylib cancels the goal of
/// a request that its provider acknowledged and did not answer in time,
/// and a request that reached no provider started no goal.
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
    let handle = task
        .client
        .fire_goal(
            messenger,
            identity,
            binding,
            producer,
            &input,
            task.feedback_bound,
            admission,
        )
        .await
        .map_err(failed)?;
    if let Some(refusal) = handle.refusal() {
        return Err(ActionExit::Refused(provider_failure(refusal)));
    }
    let mut goal = FollowedGoal::new(handle, messenger);

    match task.follow {
        GoalFollow::WholeGoal {
            deadline,
            reports_feedback,
        } => {
            let remaining = || deadline.saturating_sub(started.elapsed());
            let outcome = if reports_feedback {
                settle_after_feedback(&mut goal, surface, &remaining).await
            } else {
                settle_on_result(&mut goal, surface, &remaining)
                    .await
                    .map_err(failed)
            }?;
            terminal_state(outcome)
        }
        GoalFollow::Progress { window } => follow_progress(&mut goal, surface, window).await,
    }
}

/// A goal the bridge follows, which sends its provider one cancel at most.
/// The cancel goes out when the bridge sends it for its client, when a read
/// of the goal's feedback finds that the bridge fell behind, or when the
/// follow ends before the bridge read the end of the goal: the guard of the
/// goal sends it from a task of its own as it is dropped, so no goal runs
/// on with no one following it.
struct FollowedGoal<'m> {
    handle: GoalHandle,
    messenger: &'m MessengerHandle,
    stop_cancel: CancelOnDrop,
}

impl<'m> FollowedGoal<'m> {
    fn new(handle: GoalHandle, messenger: &'m MessengerHandle) -> Self {
        let stop_cancel = handle.cancel_on_drop("the endpoint stopped following the goal");
        Self {
            handle,
            messenger,
            stop_cancel,
        }
    }

    fn goal_id(&self) -> &str {
        self.handle.goal_id()
    }

    /// The next step of the goal's feedback stream (see
    /// [`next_feedback_step`]). The handle cancels a goal whose feedback
    /// overflowed itself.
    async fn next_feedback_step(&mut self) -> FeedbackStep {
        let step = next_feedback_step(&mut self.handle).await;
        if let FeedbackStep::Overflowed { .. } = step {
            self.stop_cancel.disarm();
        }
        step
    }

    /// Requests the goal's end, waiting at most `deadline`. The end read,
    /// the goal needs no cancel.
    async fn result(&mut self, deadline: Duration) -> Result<GoalOutcome, ConsumerError> {
        let outcome = self.handle.result(self.messenger, deadline).await;
        if outcome.is_ok() {
            self.stop_cancel.disarm();
        }
        outcome
    }

    /// Sends the provider the cancel of the bridge's client, waiting at
    /// most `deadline` for its answer. It is the one cancel of the goal.
    async fn cancel(&mut self, deadline: Duration) -> Result<CancelState, ConsumerError> {
        self.stop_cancel.disarm();
        self.handle.cancel(self.messenger, deadline).await
    }
}

/// The end of a goal the bridge could not follow to its end. A refusal or
/// a failure with a kind that the member does not declare is one: the
/// error names the kind.
fn failed(error: ConsumerError) -> ActionExit {
    ActionExit::Error(error.to_string())
}

/// The end of a goal whose feedback overflowed `bound` unread messages.
fn feedback_overflowed(bound: usize) -> ActionExit {
    ActionExit::Error(format!(
        "the endpoint fell behind the feedback of the goal by more than {bound} unread \
         messages, so it cancelled the goal"
    ))
}

/// The MCP terminal state of a goal's terminal outcome. A goal that ended
/// cancelled carries the result its provider ended it with and the
/// provider's reason. A failed goal carries the provider's failure kind and
/// message.
fn terminal_state(outcome: GoalOutcome) -> Result<Value, ActionExit> {
    match outcome {
        GoalOutcome::Completed(value) => Ok(value),
        GoalOutcome::Cancelled { reason, result } => {
            Err(ActionExit::Cancelled(CancelledGoal { result, reason }))
        }
        GoalOutcome::Failed(failure) => Err(ActionExit::Failed(provider_failure(&failure))),
        GoalOutcome::Abandoned => Err(ActionExit::Error(
            "the provider abandoned the goal".to_owned(),
        )),
        GoalOutcome::Expired => Err(ActionExit::Error(
            "the goal ended, but its result is no longer readable".to_owned(),
        )),
    }
}

/// One step of a goal's feedback stream.
#[derive(Debug, Clone, PartialEq)]
enum FeedbackStep {
    /// A message, to show to the client.
    Message(String),
    /// A message that does not convert: logged, and not shown to the client.
    Skipped,
    /// The stream ended: the goal settled, or its provider is gone.
    Ended,
    /// The bridge fell behind by more than `bound` unread messages: the
    /// handle dropped them, took nothing more and cancelled the goal.
    Overflowed { bound: usize },
}

/// Takes the next message of a goal's feedback stream. A message that does
/// not convert is one the provider sent for the goal all the same: it is
/// logged and skipped, and the stream goes on.
async fn next_feedback_step(handle: &mut GoalHandle) -> FeedbackStep {
    match handle.next_feedback().await {
        Ok(Some(value)) => FeedbackStep::Message(value.to_string()),
        Err(ConsumerError::Conversion(error)) => {
            tracing::warn!(
                %error,
                goal_id = handle.goal_id(),
                "feedback that does not convert is not shown to the client"
            );
            FeedbackStep::Skipped
        }
        Err(ConsumerError::Messaging(PeppyError::ActionFeedbackOverflow { bound, .. })) => {
            FeedbackStep::Overflowed { bound }
        }
        Ok(None)
        | Err(ConsumerError::Messaging(_))
        | Err(ConsumerError::UndeclaredFailureKind { .. }) => FeedbackStep::Ended,
    }
}

/// Follows a goal whose feedback overflowed `bound` to its end, within
/// `deadline`, and fails it with the loss. The handle cancelled the goal.
/// How the goal ended does not change the failure: the client lost feedback
/// it was owed.
async fn end_overflowed_goal(
    goal: &mut FollowedGoal<'_>,
    bound: usize,
    deadline: Duration,
) -> ActionExit {
    if let Err(error) = goal.result(deadline).await {
        tracing::warn!(
            %error,
            goal_id = goal.goal_id(),
            "the end of a goal whose feedback overflowed is unknown; the provider may still run it"
        );
    }
    feedback_overflowed(bound)
}

/// Settles a goal on an action with feedback under a whole-goal deadline:
/// every message the goal's handle yields is shown to the client (see
/// [`next_feedback_step`]), the next one read once the client took it,
/// until the provider closes the stream at the terminal result (or
/// disappears), then the result reply decides the outcome. A stream that
/// overflows fails the goal (see [`end_overflowed_goal`]).
async fn settle_after_feedback(
    goal: &mut FollowedGoal<'_>,
    surface: &impl TaskSurface,
    remaining: &impl Fn() -> Duration,
) -> Result<GoalOutcome, ActionExit> {
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
            let _ = goal.cancel(remaining()).await;
        }
        tokio::select! {
            _ = &mut expiry => break,
            _ = surface.cancel_requested(), if !cancel_forwarded => {
                cancel_pending = true;
            }
            step = goal.next_feedback_step() => match step {
                FeedbackStep::Message(message) => tokio::select! {
                    () = surface.report_feedback(message) => {}
                    _ = &mut expiry => break,
                },
                FeedbackStep::Skipped => {}
                FeedbackStep::Ended => break,
                FeedbackStep::Overflowed { bound } => {
                    return Err(end_overflowed_goal(goal, bound, remaining()).await);
                }
            }
        }
    }
    goal.result(remaining()).await.map_err(failed)
}

/// Settles a goal on a feedback-less action under a whole-goal deadline:
/// its stream carries nothing, not even a close at the terminal result, so
/// the result request is parked on the provider from the start and its
/// reply decides the outcome. The request stays parked while a cancel is
/// forwarded; the provider settling the goal cancelled is what answers it.
async fn settle_on_result(
    goal: &mut FollowedGoal<'_>,
    surface: &impl TaskSurface,
    remaining: &impl Fn() -> Duration,
) -> Result<GoalOutcome, ConsumerError> {
    let FollowedGoal {
        handle,
        messenger,
        stop_cancel,
    } = goal;
    let (handle, messenger) = (&*handle, *messenger);
    let result = handle.result(messenger, remaining());
    tokio::pin!(result);
    let mut cancel_forwarded = false;
    loop {
        tokio::select! {
            outcome = &mut result => {
                if outcome.is_ok() {
                    stop_cancel.disarm();
                }
                return outcome;
            }
            _ = surface.cancel_requested(), if !cancel_forwarded => {
                cancel_forwarded = true;
                stop_cancel.disarm();
                let _ = handle.cancel(messenger, remaining()).await;
            }
        }
    }
}

/// The time since a progress-bound goal last showed a sign of progress,
/// which may last one window at most. The node bridges and the daemon
/// bridge both keep it.
pub(crate) struct Silence {
    window: Duration,
    expiry: Pin<Box<Sleep>>,
}

impl Silence {
    /// A silence that starts now.
    pub(crate) fn new(window: Duration) -> Self {
        Self {
            window,
            expiry: Box::pin(tokio::time::sleep(window)),
        }
    }

    /// A sign of progress ends the silence: the next window starts now.
    pub(crate) fn restart(&mut self) {
        self.expiry.as_mut().reset(Instant::now() + self.window);
    }

    /// Resolves once the silence has lasted a whole window.
    pub(crate) async fn lasted_a_window(&mut self) {
        self.expiry.as_mut().await;
    }

    /// The most a silence lasts.
    pub(crate) fn window(&self) -> Duration {
        self.window
    }
}

/// The failure of a goal that went `window` without a sign of progress. A
/// goal with no cancel of the client gets the bridge's cancel as the
/// follow ends (see [`FollowedGoal`]); a goal with one did not answer it.
fn went_silent(window: Duration, cancel_sent: bool) -> ActionExit {
    let what = if cancel_sent {
        "a cancel was sent and the provider did not answer it"
    } else {
        "the endpoint stopped following the goal and cancelled it"
    };
    ActionExit::Error(format!(
        "no progress within {} ms; {what}",
        window.as_millis()
    ))
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

/// Reads the provider's reply to the cancel the bridge sent for its
/// client. A provider that does not know the goal, or a cancel that failed,
/// ends the follow with a failure that says so. A failed cancel is also
/// logged: the goal may still run on the provider.
fn after_cancel(
    reply: Result<CancelState, ConsumerError>,
    goal_id: &str,
) -> Result<AfterCancel, ActionExit> {
    match reply {
        Ok(CancelState::Signalled) => Ok(AfterCancel::KeepFollowing),
        Ok(CancelState::AlreadyTerminal) => Ok(AfterCancel::ReadTheResult),
        Ok(CancelState::Unknown) => Err(ActionExit::Error(
            "a cancel was sent and the provider does not know the goal".to_owned(),
        )),
        Err(error) => {
            tracing::warn!(
                %error,
                goal_id,
                "the cancel of a goal failed; the provider may still run it"
            );
            Err(ActionExit::Error(format!(
                "a cancel was sent and failed: {error}"
            )))
        }
    }
}

/// Sends the provider the bridge's one cancel of the goal, for its client,
/// and reads its reply (see [`after_cancel`]). The round trip is bounded by
/// one window.
async fn cancel_goal(
    goal: &mut FollowedGoal<'_>,
    window: Duration,
) -> Result<AfterCancel, ActionExit> {
    let reply = goal.cancel(window).await;
    after_cancel(reply, goal.goal_id())
}

/// Follows a goal bounded by its progress, `window` at a time. The signs of
/// progress are the admission reply (the goal is admitted when this
/// starts), each feedback message, and the end of the feedback stream;
/// each one starts a new window, and the result request after the end of
/// the stream gets one window of its own. The window restarts on progress
/// only, never on a timer, so a provider that stops moving is caught
/// whatever it did before. A feedback message that does not convert is
/// still one the provider sent for the goal: it starts a new window, and
/// the client is not shown it. The next message is read once the client
/// took the one before, and a message that arrived meanwhile is progress
/// all the same (see [`next_turn`]).
///
/// When a window goes by without a sign of progress, the follow ends with
/// a failure that says so (see [`went_silent`]), and the provider gets the
/// bridge's cancel as the follow ends. The bridge sends the provider one
/// cancel at most: when its client cancels, the bridge sends it and keeps
/// following the goal under the same rule to learn how it ended. The
/// cancel round trip is bounded by one window while the silence keeps
/// running: a cancel acknowledgement is no progress of the goal. The reply
/// to the cancel is read (see [`after_cancel`]): a goal that had already
/// ended has its result read at once, since the end of its stream is the
/// one sign that did not come through. A stream that overflows fails the
/// goal (see [`end_overflowed_goal`]), the end of the goal read within one
/// window.
async fn follow_progress(
    goal: &mut FollowedGoal<'_>,
    surface: &impl TaskSurface,
    window: Duration,
) -> Result<Value, ActionExit> {
    let mut silence = Silence::new(window);
    let mut cancel_sent = false;
    loop {
        let feedback = goal.next_feedback_step();
        match next_turn(&mut silence, surface, !cancel_sent, feedback).await {
            Turn::CancelRequested => {
                cancel_sent = true;
                match cancel_goal(goal, window).await? {
                    AfterCancel::KeepFollowing => {}
                    AfterCancel::ReadTheResult => break,
                }
            }
            Turn::Feedback(FeedbackStep::Message(message)) => {
                silence.restart();
                surface.report_feedback(message).await;
            }
            Turn::Feedback(FeedbackStep::Skipped) => silence.restart(),
            Turn::Feedback(FeedbackStep::Ended) => break,
            Turn::Feedback(FeedbackStep::Overflowed { bound }) => {
                return Err(end_overflowed_goal(goal, bound, window).await);
            }
            Turn::Silence => return Err(went_silent(window, cancel_sent)),
        }
    }
    let outcome = goal.result(window).await.map_err(failed)?;
    terminal_state(outcome)
}

/// What comes next while the bridge follows a progress-bound goal.
#[derive(Debug, Clone, PartialEq)]
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
/// its host: the goal showed progress, and the call does not fail for a
/// silence. Taking the silence last cannot hide a stall, because a stream
/// that is always ready is progress.
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
