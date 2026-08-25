//! The bridges between each catalog entry and the contract member behind
//! it, the one validation bound the entry to: a codec per message side
//! laid out when the process starts, and the type-erased clients driven at
//! request time against the producers the launcher bound to the entry's
//! target.

use crate::serve::ServeError;
use config::node::{MessageFormat, NativeExposedAction};
use message_codec::MessageCodec;
use message_codec::consumer::{
    ActionClient, ConsumerIdentity, GoalOutcome, MemberBinding, ServiceClient, TopicConsumer,
};
use peppy_mcp_catalog::{BundleContractPin, ExposureBundle, ValidatedExposure};
use peppy_mcp_runtime::{ActionContext, ActionExit, ResourceIngest, ToolCallError};
use peppylib::config::QoSProfile;
use peppylib::messaging::SenderTarget;
use peppylib::runtime::NodeRunner;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

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
    /// The wire binding against every producer the launcher bound to the
    /// slot.
    fn member_binding(&self, node_runner: &NodeRunner) -> MemberBinding {
        MemberBinding {
            target: self.contract.clone(),
            member: self.member.clone(),
            producers: node_runner
                .processor()
                .bound_producers(&self.target)
                .to_vec(),
        }
    }
}

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
    /// Whether the action declares a feedback message; a feedback-less
    /// action's stream carries only the terminal sentinel, which is not
    /// reported as task progress.
    pub reports_feedback: bool,
    pub deadline: Duration,
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
            tasks.push(PreparedTask {
                name: entry.name.clone(),
                binding: Binding::new(&bound.slot, &entry.member)?,
                reports_feedback: feedback.is_some(),
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
                deadline: Duration::from_millis(entry.deadline_ms.get()),
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
/// and a feedback-less action's sentinel-only stream takes the default.
fn feedback_qos(action: &NativeExposedAction) -> QoSProfile {
    action
        .feedback_topic
        .as_ref()
        .map(|topic| topic.qos_profile.clone())
        .unwrap_or_default()
}

/// Feeds a resource from its topic: every message admitted by the
/// update-rate gate is decoded and offered to the resource's policies. The
/// subscription lives as long as the node.
pub(crate) async fn pump_resource(
    node_runner: Arc<NodeRunner>,
    identity: ConsumerIdentity,
    resource: PreparedResource,
    ingest: ResourceIngest,
) {
    let binding = resource.binding.member_binding(&node_runner);
    let mut subscription = match TopicConsumer::subscribe(
        node_runner.messenger(),
        &identity,
        &binding,
        resource.qos.clone(),
        resource.codec.clone(),
        node_runner.cancellation_token().child_token(),
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
    while let Some((_producer, message)) = subscription.next_message().await {
        // The update-rate gate runs before any conversion or transcoding.
        let Some(token) = ingest.admit() else {
            continue;
        };
        match subscription.decode(&message) {
            Ok(value) => {
                if let Err(error) = ingest.publish(token, value) {
                    tracing::debug!(%error, resource = resource.name, "snapshot refused by policy");
                }
            }
            Err(error) => {
                tracing::debug!(%error, resource = resource.name, "message does not convert")
            }
        }
    }
}

/// Calls the service behind a tool on the producer bound to its target.
pub(crate) async fn call_tool(
    tool: &PreparedTool,
    node_runner: &NodeRunner,
    identity: &ConsumerIdentity,
    input: Value,
) -> Result<Value, ToolCallError> {
    let binding = tool.binding.member_binding(node_runner);
    let producer = node_runner
        .processor()
        .sole_bound_producer(&tool.binding.target)
        .clone();
    tool.client
        .call(
            node_runner.messenger(),
            identity,
            &binding,
            &producer,
            &input,
            tool.deadline,
        )
        .await
        .map_err(|error| ToolCallError::Failed(error.to_string()))
}

/// Runs the action behind a task: fires the goal, pumps feedback into the
/// task's status, forwards `tasks/cancel` cooperatively, and maps the Peppy
/// terminal result onto the MCP terminal state.
///
/// The deadline is the whole-goal deadline, so every await after the goal
/// is fired spends what is left of it rather than restarting it: a
/// provider that keeps sending feedback, or a cancel that takes its own
/// time, cannot push the bridge past the deadline the tool advertises.
pub(crate) async fn run_task(
    task: &PreparedTask,
    node_runner: &NodeRunner,
    identity: &ConsumerIdentity,
    input: Value,
    context: ActionContext,
) -> Result<Value, ActionExit> {
    let messenger = node_runner.messenger();
    let binding = task.binding.member_binding(node_runner);
    let producer = node_runner
        .processor()
        .sole_bound_producer(&task.binding.target)
        .clone();
    let started = Instant::now();
    let deadline = task.deadline;
    let remaining = || deadline.saturating_sub(started.elapsed());

    let mut handle = task
        .client
        .fire_goal(
            messenger,
            identity,
            &binding,
            &producer,
            &input,
            task.feedback_qos.clone(),
            deadline,
        )
        .await
        .map_err(|error| ActionExit::Failed(error.to_string()))?;
    if !handle.accepted() {
        return Err(ActionExit::Failed(match handle.rejection_reason() {
            Some(reason) => format!("the provider rejected the goal: {reason}"),
            None => "the provider rejected the goal".to_owned(),
        }));
    }

    // Cancellation is cooperative on both sides: `tasks/cancel` is
    // forwarded once to the Peppy cancel path, and the terminal result the
    // provider settles on decides the task's terminal state. The feedback
    // stream ends at the terminal result (clean close) or when the producer
    // disappears; either way the result reply below settles the outcome.
    let mut cancel_pending = false;
    let mut cancel_forwarded = false;
    // Armed once rather than per message: a feedback stream can be fast,
    // and re-arming a timer on every message would never actually expire.
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
            _ = context.cancel_requested(), if !cancel_forwarded => {
                cancel_pending = true;
            }
            message = handle.next_feedback() => match message {
                Ok(Some(value)) => {
                    if task.reports_feedback {
                        context.report_feedback(value.to_string());
                    }
                }
                Ok(None) | Err(_) => break,
            }
        }
    }
    let outcome = handle
        .result(messenger, remaining())
        .await
        .map_err(|error| ActionExit::Failed(error.to_string()))?;
    match outcome {
        GoalOutcome::Completed(value) => Ok(value),
        GoalOutcome::Cancelled => Err(ActionExit::Cancelled),
        GoalOutcome::Abandoned => Err(ActionExit::Failed(
            "the provider abandoned the goal".to_owned(),
        )),
        GoalOutcome::Expired => Err(ActionExit::Failed(
            "the goal expired before reaching a terminal result".to_owned(),
        )),
    }
}
