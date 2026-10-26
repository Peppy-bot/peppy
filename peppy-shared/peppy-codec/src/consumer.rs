//! The type-erased consumer path: the topics, services and actions of a
//! bound contract member, driven with canonical JSON.
//!
//! Each client here makes the messaging calls a generated node's
//! `consumed_*` module makes, with a [`MessageCodec`] in place of generated
//! conversions: the same identities on the wire, the same producer pins,
//! the same envelopes. A provider cannot tell the two apart.

use crate::contract::PeppyContract;
use crate::{CodecError, ConversionError, MessageCodec};
use config::failure_kind::{MemberFailureKinds, MemberSection};
use config::node::{InterfaceKind, NativeExposedAction, NativeExposedService};
use peppylib::config::{QoSProfile, TopicRetention};
use peppylib::messaging::{
    ActionGoalHandle, ActionMessenger, ActionWireSender, CancelOnDrop, CancelState, Failure,
    FeedbackBound, GoalAnswer, ProducerRef, SenderTarget, ServiceMessenger, ServiceTarget,
    Subscription, TopicBuffer, TopicMessenger, decode_cancel_ack,
};
use peppylib::runtime::BoundSetSubscription;
use peppylib::{Message, MessengerHandle, Payload, PeppyError};
use serde_json::{Map, Value};
use std::future::Future;
use std::time::Duration;

#[derive(Debug, thiserror::Error)]
pub enum ConsumerError {
    #[error(transparent)]
    Messaging(#[from] PeppyError),
    #[error(transparent)]
    Conversion(#[from] ConversionError),
    /// The provider gave a failure kind that is neither a kind of peppy nor
    /// a kind of its contract that the member lists. `failure` is the error
    /// reply, the refusal or the failure that the provider gave, with its
    /// kind and its message.
    #[error(
        "the provider of `{member}` gave the failure kind `{}`, which is neither a kind of peppy nor a kind of its contract that the member lists: {}",
        failure.kind(),
        failure.message()
    )]
    UndeclaredFailureKind { member: String, failure: Failure },
}

/// The failure that `error` carries, checked against the kinds `kinds`
/// allows: an error with an undeclared kind becomes
/// [`ConsumerError::UndeclaredFailureKind`].
fn checked_failure_error(
    error: PeppyError,
    member: &str,
    kinds: &MemberFailureKinds,
) -> ConsumerError {
    match error.failure() {
        Some(failure) if !kinds.allows(failure.kind().as_str()) => {
            ConsumerError::UndeclaredFailureKind {
                member: member.to_string(),
                failure: failure.clone(),
            }
        }
        _ => ConsumerError::Messaging(error),
    }
}

/// `failure` when the member's kinds allow its kind.
fn checked_failure(
    failure: Failure,
    member: &str,
    kinds: &MemberFailureKinds,
) -> Result<Failure, ConsumerError> {
    if kinds.allows(failure.kind().as_str()) {
        return Ok(failure);
    }
    Err(ConsumerError::UndeclaredFailureKind {
        member: member.to_string(),
        failure,
    })
}

/// Who the consumer is in the graph: the core node it is bound to and the
/// instance it runs as. Every subscription, poll and goal is sent as this
/// identity.
#[derive(Debug, Clone)]
pub struct ConsumerIdentity {
    pub core_node: String,
    pub instance_id: String,
}

/// A contract member the consumer is bound to: the target its provider
/// serves it under and the member's name in the contract.
#[derive(Debug, Clone)]
pub struct MemberBinding {
    pub target: SenderTarget,
    pub member: String,
}

impl MemberBinding {
    /// The binding of the member `member` of `contract`: a provider of the
    /// contract serves each of its members under the name and the tag of
    /// the contract.
    pub fn of_contract(contract: &PeppyContract, member: &str) -> Result<Self, MemberError> {
        let name = contract.manifest.name.as_str();
        let tag = contract.manifest.tag.as_str();
        let target = SenderTarget::contract(name, tag).map_err(|error| MemberError::Target {
            contract: format!("{name}:{tag}"),
            reason: error.to_string(),
        })?;
        Ok(Self {
            target,
            member: member.to_owned(),
        })
    }
}

/// A client of a member of a contract cannot be built.
#[derive(Debug, thiserror::Error)]
pub enum MemberError {
    /// The contract declares no member of that kind and name.
    #[error("contract `{contract}` declares no {kind} `{member}`")]
    Undeclared {
        contract: String,
        kind: InterfaceKind,
        member: String,
    },
    /// A message format of the member does not lay out.
    #[error(transparent)]
    Codec(#[from] CodecError),
    /// The name or the tag of the contract is not a segment of the wire.
    #[error("contract `{contract}` has no address on the wire: {reason}")]
    Target { contract: String, reason: String },
}

/// The label of the codecs of the member `member` of `contract`.
fn contract_member_label(contract: &PeppyContract, member: &str) -> String {
    format!(
        "{}_{}_{member}",
        contract.manifest.name.as_str(),
        contract.manifest.tag
    )
}

fn undeclared(contract: &PeppyContract, kind: InterfaceKind, member: &str) -> MemberError {
    MemberError::Undeclared {
        contract: format!(
            "{}:{}",
            contract.manifest.name.as_str(),
            contract.manifest.tag
        ),
        kind,
        member: member.to_owned(),
    }
}

/// The codec of the topic `name` of `contract`.
pub fn contract_topic_codec(
    contract: &PeppyContract,
    name: &str,
) -> Result<MessageCodec, MemberError> {
    let topic = contract
        .topic(name)
        .ok_or_else(|| undeclared(contract, InterfaceKind::Topic, name))?;
    Ok(MessageCodec::of_topic(
        &contract_member_label(contract, name),
        topic,
    )?)
}

/// A subscription to a topic member, decoding messages on request: across
/// every producer bound to a slot of the consumer's node, or from one
/// producer that a client with no node names itself.
pub struct TopicConsumer {
    codec: MessageCodec,
    source: TopicSource,
}

/// Where the messages of a [`TopicConsumer`] come from.
enum TopicSource {
    /// The producers a launcher binds to a slot of the consumer's node.
    BoundSet(BoundSetSubscription),
    /// One producer, which the consumer named.
    Producer {
        producer: ProducerRef,
        subscription: Subscription,
    },
}

impl TopicConsumer {
    /// Decodes what `subscription` receives from the topic's producers with
    /// `codec`.
    pub fn new(subscription: BoundSetSubscription, codec: MessageCodec) -> Self {
        Self {
            codec,
            source: TopicSource::BoundSet(subscription),
        }
    }

    /// Subscribes, as `identity`, to the topic `binding` names on `producer`
    /// alone, with no slot of a node: a contract topic or a topic of the
    /// producer's own. `qos` and `retention` are the topic's declared ones,
    /// so a producer that retains messages gives them first.
    ///
    /// The subscription keeps the newest unread messages
    /// ([`TopicBuffer::KeepLatest`]): a client that reads slower than the
    /// producer publishes loses the oldest unread messages, and never holds
    /// up the producer or the other messages of its session.
    pub async fn of_producer(
        messenger: &MessengerHandle,
        identity: &ConsumerIdentity,
        binding: &MemberBinding,
        producer: &ProducerRef,
        qos: QoSProfile,
        retention: TopicRetention,
        codec: MessageCodec,
    ) -> Result<Self, PeppyError> {
        let subscription = TopicMessenger::subscribe(
            messenger,
            &identity.core_node,
            &identity.instance_id,
            binding.target.clone(),
            &binding.member,
            producer,
            qos,
            retention,
            TopicBuffer::KeepLatest,
        )
        .await?;
        Ok(Self::of_one_producer(producer, subscription, codec))
    }

    /// Subscribes, as `identity`, to the pairing topic `binding` names on
    /// `producer`, as an observer of every pair that the producer's slot
    /// `link_id` holds, with no slot of a node. A pairing topic is live
    /// only, and a slot that holds no pair publishes nothing. The
    /// subscription keeps the newest unread messages, as in
    /// [`Self::of_producer`].
    pub async fn of_pairing_producer(
        messenger: &MessengerHandle,
        identity: &ConsumerIdentity,
        binding: &MemberBinding,
        producer: &ProducerRef,
        link_id: &str,
        qos: QoSProfile,
        codec: MessageCodec,
    ) -> Result<Self, PeppyError> {
        let subscription = TopicMessenger::subscribe_observing(
            messenger,
            &identity.core_node,
            &identity.instance_id,
            binding.target.clone(),
            producer,
            link_id,
            &binding.member,
            qos,
            TopicBuffer::KeepLatest,
        )
        .await?;
        Ok(Self::of_one_producer(producer, subscription, codec))
    }

    fn of_one_producer(
        producer: &ProducerRef,
        subscription: Subscription,
        codec: MessageCodec,
    ) -> Self {
        Self {
            codec,
            source: TopicSource::Producer {
                producer: producer.clone(),
                subscription,
            },
        }
    }

    /// The next message, with the producer that published it, or `None`
    /// once the subscription has shut down.
    pub async fn next_message(&mut self) -> Option<(ProducerRef, Message)> {
        match &mut self.source {
            TopicSource::BoundSet(subscription) => subscription.on_next_message().await,
            TopicSource::Producer {
                producer,
                subscription,
            } => {
                let message = subscription.on_next_message().await?;
                Some((producer.clone(), message))
            }
        }
    }

    pub fn decode(&self, message: &Message) -> Result<Value, ConversionError> {
        self.codec.decode(message.payload_bytes().as_ref())
    }

    pub fn codec(&self) -> &MessageCodec {
        &self.codec
    }
}

/// A client for a service member. A side without a message format carries
/// no payload: the request is empty on the wire and the response reads as
/// an empty JSON object.
#[derive(Debug, Clone)]
pub struct ServiceClient {
    request: Option<MessageCodec>,
    response: Option<MessageCodec>,
    failure_kinds: MemberFailureKinds,
}

impl ServiceClient {
    /// A client of `service`, its codecs laid out under `label` and the
    /// name of each side. `failure_kinds` is the closed set of kinds the
    /// member can give in an error reply.
    pub fn of_service(
        label: &str,
        service: &NativeExposedService,
        failure_kinds: MemberFailureKinds,
    ) -> Result<Self, CodecError> {
        Ok(Self::new(
            MessageCodec::of_payload(
                &format!("{label}_request"),
                service.request_message_format.as_ref(),
            )?,
            MessageCodec::of_payload(
                &format!("{label}_response"),
                service.response_message_format.as_ref(),
            )?,
            failure_kinds,
        ))
    }

    /// A client of the service `name` of `contract`, with the failure kinds
    /// that the contract lists for it.
    pub fn of_contract(contract: &PeppyContract, name: &str) -> Result<Self, MemberError> {
        let service = contract
            .service(name)
            .ok_or_else(|| undeclared(contract, InterfaceKind::Service, name))?;
        Ok(Self::of_service(
            &contract_member_label(contract, name),
            service,
            contract.failure_kinds.of(MemberSection::Service, name),
        )?)
    }

    /// `failure_kinds` is the closed set of kinds the member can give in an
    /// error reply.
    pub fn new(
        request: Option<MessageCodec>,
        response: Option<MessageCodec>,
        failure_kinds: MemberFailureKinds,
    ) -> Self {
        Self {
            request,
            response,
            failure_kinds,
        }
    }

    /// Calls the member on `producer` and waits at most `deadline` for the
    /// response, as long as it takes with no deadline. An error reply is a
    /// [`PeppyError::ServiceFailed`]; one with a kind the member does not
    /// declare is a [`ConsumerError::UndeclaredFailureKind`].
    pub async fn call(
        &self,
        messenger: &MessengerHandle,
        identity: &ConsumerIdentity,
        binding: &MemberBinding,
        producer: &ProducerRef,
        request: &Value,
        deadline: impl Into<Option<Duration>>,
    ) -> Result<Value, ConsumerError> {
        let payload = encode_optional(self.request.as_ref(), request)?;
        let response = ServiceMessenger::poll(
            messenger,
            &identity.core_node,
            &identity.instance_id,
            binding.target.clone(),
            &binding.member,
            ServiceTarget::Producer(producer),
            payload,
            deadline,
        )
        .await
        .map_err(|error| checked_failure_error(error, &binding.member, &self.failure_kinds))?;
        Ok(decode_optional(
            self.response.as_ref(),
            response.payload_bytes().as_ref(),
        )?)
    }
}

/// A client for an action member.
#[derive(Debug, Clone)]
pub struct ActionClient {
    goal: Option<MessageCodec>,
    feedback: Option<MessageCodec>,
    result: Option<MessageCodec>,
    failure_kinds: MemberFailureKinds,
}

impl ActionClient {
    /// A client of `action`, its codecs laid out under `label` and the name
    /// of each side. `failure_kinds` is the closed set of kinds the member
    /// can give when it refuses or fails a goal.
    pub fn of_action(
        label: &str,
        action: &NativeExposedAction,
        failure_kinds: MemberFailureKinds,
    ) -> Result<Self, CodecError> {
        let goal = action
            .goal_service
            .as_ref()
            .and_then(|goal| goal.request_message_format.as_ref());
        let feedback = action
            .feedback_topic
            .as_ref()
            .map(|feedback| &feedback.message_format);
        let result = action
            .result_service
            .as_ref()
            .and_then(|result| result.response_message_format.as_ref());
        Ok(Self::new(
            MessageCodec::of_payload(&format!("{label}_goal"), goal)?,
            MessageCodec::of_payload(&format!("{label}_feedback"), feedback)?,
            MessageCodec::of_payload(&format!("{label}_result"), result)?,
            failure_kinds,
        ))
    }

    /// A client of the action `name` of `contract`, with the failure kinds
    /// that the contract lists for it.
    pub fn of_contract(contract: &PeppyContract, name: &str) -> Result<Self, MemberError> {
        let action = contract
            .action(name)
            .ok_or_else(|| undeclared(contract, InterfaceKind::Action, name))?;
        Ok(Self::of_action(
            &contract_member_label(contract, name),
            action,
            contract.failure_kinds.of(MemberSection::Action, name),
        )?)
    }

    /// Whether the action declares a feedback message, whose stream the
    /// provider closes at the end of a goal.
    pub fn reports_feedback(&self) -> bool {
        self.feedback.is_some()
    }

    /// `goal` converts the goal request, `feedback` each feedback message
    /// and `result` the result's body; each is `None` for a side the
    /// contract declares without a message format. `failure_kinds` is the
    /// closed set of kinds the member can give when it refuses or fails a
    /// goal.
    pub fn new(
        goal: Option<MessageCodec>,
        feedback: Option<MessageCodec>,
        result: Option<MessageCodec>,
        failure_kinds: MemberFailureKinds,
    ) -> Self {
        Self {
            goal,
            feedback,
            result,
            failure_kinds,
        }
    }

    /// Sends `goal` to `producer`, waiting at most `deadline` for the
    /// producer's admission reply. The handle reports whether the goal was
    /// accepted and drives its feedback, result and cancellation. The
    /// handle keeps up to `feedback_bound` unread feedback messages. A
    /// caller that reads the feedback and falls behind by more loses the
    /// stream: the handle cancels the goal, and the next read reports the
    /// loss. A caller that never reads the feedback keeps its goal.
    #[allow(clippy::too_many_arguments)]
    pub async fn fire_goal(
        &self,
        messenger: &MessengerHandle,
        identity: &ConsumerIdentity,
        binding: &MemberBinding,
        producer: &ProducerRef,
        goal: &Value,
        feedback_bound: FeedbackBound,
        deadline: Duration,
    ) -> Result<GoalHandle, ConsumerError> {
        let payload = encode_optional(self.goal.as_ref(), goal)?;
        let inner = ActionMessenger::send_goal(
            messenger,
            &identity.core_node,
            &identity.instance_id,
            binding.target.clone(),
            &binding.member,
            Some(producer),
            payload,
            feedback_bound,
            deadline,
        )
        .await?;
        if let Some(refusal) = inner.goal_reply().refusal() {
            checked_failure(refusal.clone(), &binding.member, &self.failure_kinds)?;
        }
        Ok(GoalHandle {
            inner,
            member: binding.member.clone(),
            feedback: self.feedback.clone(),
            result: self.result.clone(),
            failure_kinds: self.failure_kinds.clone(),
        })
    }
}

/// A goal in flight.
pub struct GoalHandle {
    inner: ActionGoalHandle,
    member: String,
    feedback: Option<MessageCodec>,
    result: Option<MessageCodec>,
    failure_kinds: MemberFailureKinds,
}

/// How a goal ended, as reported by the producer's result service.
#[derive(Debug, Clone, PartialEq)]
pub enum GoalOutcome {
    /// The goal ran to completion, a success; the value is the decoded
    /// result body, an empty object for an action without a result format.
    Completed(Value),
    /// The goal ended cancelled, for `reason`; `result` is the result body
    /// the producer ended it with, decoded as a completed one is.
    Cancelled { reason: String, result: Value },
    /// The goal failed: a failure kind the member declares and a message.
    Failed(Failure),
    /// The producer dropped the goal before it settled.
    Abandoned,
    /// The producer no longer retains the goal's result.
    Expired,
}

impl GoalHandle {
    pub fn accepted(&self) -> bool {
        self.inner.goal_reply().is_accepted()
    }

    /// The failure the producer refused the goal with: a kind the member
    /// declares, and a message.
    pub fn refusal(&self) -> Option<&Failure> {
        match &self.inner.goal_reply().answer {
            GoalAnswer::Refused(failure) => Some(failure),
            GoalAnswer::Accepted(_) => None,
        }
    }

    pub fn goal_id(&self) -> &str {
        self.inner.goal_id()
    }

    /// The next feedback message, or `None` once the producer has closed
    /// the stream because the goal reached a terminal state. Feedback on an
    /// action declared without a feedback topic reads as an empty object.
    pub async fn next_feedback(&mut self) -> Result<Option<Value>, ConsumerError> {
        match self.inner.on_next_feedback().await {
            Ok(message) => Ok(Some(decode_optional(
                self.feedback.as_ref(),
                message.payload_bytes().as_ref(),
            )?)),
            Err(PeppyError::ActionFeedbackChannelClosed) => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    /// Requests the goal's terminal outcome, waiting at most `deadline`. A
    /// completed or cancelled result body that does not convert is a
    /// [`ConsumerError::Conversion`], and a failure with a kind the member
    /// does not declare is a [`ConsumerError::UndeclaredFailureKind`].
    pub async fn result(
        &self,
        messenger: &MessengerHandle,
        deadline: Duration,
    ) -> Result<GoalOutcome, ConsumerError> {
        let reply = ActionMessenger::request_result(messenger, &self.inner, deadline).await?;
        goal_outcome(
            reply.outcome,
            self.result.as_ref(),
            &self.member,
            &self.failure_kinds,
        )
    }

    /// The requests of the goal that borrow nothing of the handle: its
    /// cancel and its result. A caller sends them from a task of its own
    /// while another task reads the feedback of the goal.
    pub fn requests(&self) -> GoalRequests {
        GoalRequests {
            sender: self.inner.sender().clone(),
            goal_id: self.inner.goal_id().to_owned(),
            member: self.member.clone(),
            result: self.result.clone(),
            failure_kinds: self.failure_kinds.clone(),
        }
    }

    /// A guard that cancels the goal when it is dropped armed: its caller
    /// stops following the goal before the goal ended (see
    /// [`CancelOnDrop`]).
    pub fn cancel_on_drop(&self, why: &'static str) -> CancelOnDrop {
        self.inner.cancel_on_drop(why)
    }

    /// Asks the producer to cancel the goal, waiting at most `deadline` for
    /// its acknowledgement.
    pub async fn cancel(
        &self,
        messenger: &MessengerHandle,
        deadline: Duration,
    ) -> Result<CancelState, ConsumerError> {
        self.cancel_request(messenger, deadline).await
    }

    /// The request of [`Self::cancel`], which borrows nothing: a caller
    /// sends it from a task of its own while it goes on reading the goal.
    pub fn cancel_request(
        &self,
        messenger: &MessengerHandle,
        deadline: Duration,
    ) -> impl Future<Output = Result<CancelState, ConsumerError>> + Send + 'static {
        let requests = self.requests();
        let messenger = messenger.clone();
        async move { requests.cancel(&messenger, deadline).await }
    }
}

/// The cancel and the result requests of a goal, with no handle (see
/// [`GoalHandle::requests`]).
#[derive(Debug, Clone)]
pub struct GoalRequests {
    sender: ActionWireSender,
    goal_id: String,
    member: String,
    result: Option<MessageCodec>,
    failure_kinds: MemberFailureKinds,
}

impl GoalRequests {
    /// Asks the producer to cancel the goal, waiting at most `deadline` for
    /// its acknowledgement.
    pub async fn cancel(
        &self,
        messenger: &MessengerHandle,
        deadline: Duration,
    ) -> Result<CancelState, ConsumerError> {
        let acknowledgement =
            ActionMessenger::cancel_with_sender(messenger, &self.sender, &self.goal_id, deadline)
                .await?;
        Ok(decode_cancel_ack(acknowledgement.payload_bytes().as_ref())?)
    }

    /// Requests the goal's terminal outcome, waiting at most `deadline`, as
    /// [`GoalHandle::result`] does. It asks the producer even when the
    /// handle saw it leave, and a producer that is gone gives
    /// [`GoalOutcome::Abandoned`].
    pub async fn result(
        &self,
        messenger: &MessengerHandle,
        deadline: Duration,
    ) -> Result<GoalOutcome, ConsumerError> {
        let reply = ActionMessenger::request_result_with_sender(
            messenger,
            &self.sender,
            &self.goal_id,
            deadline,
        )
        .await?;
        goal_outcome(
            reply.outcome,
            self.result.as_ref(),
            &self.member,
            &self.failure_kinds,
        )
    }
}

/// How a goal of `member` ended, its result body decoded with `codec` and
/// its failure checked against `failure_kinds`.
fn goal_outcome(
    outcome: peppylib::messaging::GoalOutcome,
    codec: Option<&MessageCodec>,
    member: &str,
    failure_kinds: &MemberFailureKinds,
) -> Result<GoalOutcome, ConsumerError> {
    use peppylib::messaging::GoalOutcome as Ended;
    let body = |payload: &Payload| decode_optional(codec, payload.as_ref());
    Ok(match outcome {
        Ended::Completed(result) => GoalOutcome::Completed(body(&result)?),
        Ended::Cancelled { reason, result } => GoalOutcome::Cancelled {
            reason,
            result: body(&result)?,
        },
        Ended::Failed(failure) => {
            GoalOutcome::Failed(checked_failure(failure, member, failure_kinds)?)
        }
        Ended::Abandoned => GoalOutcome::Abandoned,
        Ended::Expired => GoalOutcome::Expired,
    })
}

fn encode_optional(
    codec: Option<&MessageCodec>,
    value: &Value,
) -> Result<Payload, ConversionError> {
    match codec {
        Some(codec) => Ok(Payload::from(codec.encode(value)?)),
        None => Ok(Payload::new()),
    }
}

fn decode_optional(codec: Option<&MessageCodec>, bytes: &[u8]) -> Result<Value, ConversionError> {
    match codec {
        Some(codec) => codec.decode(bytes),
        None => Ok(Value::Object(Map::new())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contract::PeppyContractParser;

    const ARM: &str = r#"{
        peppy_schema: "contract/v1",
        manifest: { name: "arm", tag: "v1" },
        failure_kinds: { unreachable: "No plan to the target" },
        interfaces: {
            topics: [{ name: "state", message_format: { angle: "f64" } }, { name: "tick" }],
            services: [
                { name: "home", request_message_format: { speed: "f32" }, failure_kinds: ["unreachable"] },
            ],
            actions: [
                {
                    name: "move",
                    goal_service: { request_message_format: { x: "f64" } },
                    feedback_topic: { message_format: { progress: "f32" } },
                    failure_kinds: ["unreachable"],
                },
                { name: "wave" },
            ],
        },
    }"#;

    fn arm() -> PeppyContract {
        PeppyContractParser::from_content(ARM).expect("the contract parses")
    }

    #[test]
    fn a_service_client_lays_out_each_side_that_carries_a_payload() {
        let client = ServiceClient::of_contract(&arm(), "home").expect("the service lays out");
        assert!(client.request.is_some());
        assert!(
            client.response.is_none(),
            "the service declares no response"
        );
        assert!(client.failure_kinds.allows("unreachable"));
    }

    #[test]
    fn an_action_client_reports_feedback_when_the_action_declares_it() {
        let contract = arm();
        let moving = ActionClient::of_contract(&contract, "move").expect("the action lays out");
        assert!(moving.reports_feedback());
        assert!(moving.goal.is_some() && moving.result.is_none());
        assert!(moving.failure_kinds.allows("unreachable"));

        let waving = ActionClient::of_contract(&contract, "wave").expect("the action lays out");
        assert!(!waving.reports_feedback());
        assert!(!waving.failure_kinds.allows("unreachable"));
    }

    #[test]
    fn a_topic_with_no_format_decodes_as_an_empty_object() {
        let contract = arm();
        let tick = contract_topic_codec(&contract, "tick").expect("the topic lays out");
        let bytes = tick.encode(&Value::Object(Map::new())).expect("encodes");
        assert_eq!(
            tick.decode(&bytes).expect("decodes"),
            Value::Object(Map::new())
        );
        contract_topic_codec(&contract, "state").expect("the topic lays out");
    }

    #[test]
    fn a_member_that_the_contract_does_not_declare_is_refused_with_its_kind() {
        let contract = arm();
        let error = ServiceClient::of_contract(&contract, "move").expect_err("`move` is an action");
        assert_eq!(
            error.to_string(),
            "contract `arm:v1` declares no service `move`"
        );
        let error = ActionClient::of_contract(&contract, "home").expect_err("`home` is a service");
        assert_eq!(
            error.to_string(),
            "contract `arm:v1` declares no action `home`"
        );
        let error = contract_topic_codec(&contract, "home").expect_err("`home` is no topic");
        assert_eq!(
            error.to_string(),
            "contract `arm:v1` declares no topic `home`"
        );
    }

    #[test]
    fn a_member_of_a_contract_is_served_under_the_contract() {
        let binding = MemberBinding::of_contract(&arm(), "home").expect("a binding");
        assert_eq!(
            binding.target,
            SenderTarget::contract("arm", "v1").expect("a target")
        );
        assert_eq!(binding.member, "home");
    }
}
