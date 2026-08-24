//! The type-erased consumer path: the topics, services and actions of a
//! bound contract member, driven with canonical JSON.
//!
//! Each client here makes the messaging calls a generated node's
//! `consumed_*` module makes, with a [`MessageCodec`] in place of generated
//! conversions: the same identities on the wire, the same producer pins,
//! the same envelopes. A provider cannot tell the two apart.

use crate::{ConversionError, MessageCodec};
use peppylib::config::QoSProfile;
use peppylib::messaging::{
    ActionGoalHandle, ActionMessenger, BoundSetSubscription, CancelState, ProducerRef,
    ResultStatus, SenderTarget, ServiceMessenger, ServiceTarget, TopicMessenger, decode_cancel_ack,
};
use peppylib::runtime::CancellationToken;
use peppylib::{Message, MessengerHandle, Payload, PeppyError};
use serde_json::{Map, Value};
use std::time::Duration;

#[derive(Debug, thiserror::Error)]
pub enum ConsumerError {
    #[error(transparent)]
    Messaging(#[from] PeppyError),
    #[error(transparent)]
    Conversion(#[from] ConversionError),
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
/// serves it under, the member's name in the contract, and the producers
/// bound to the slot.
#[derive(Debug, Clone)]
pub struct MemberBinding {
    pub target: SenderTarget,
    pub member: String,
    pub producers: Vec<ProducerRef>,
}

/// A subscription to a topic member across every producer bound to its
/// slot, decoding messages on request.
pub struct TopicConsumer {
    codec: MessageCodec,
    subscription: BoundSetSubscription,
}

impl TopicConsumer {
    /// Subscribes at the topic's declared QoS. The subscription ends when
    /// `shutdown` fires.
    pub async fn subscribe(
        messenger: &MessengerHandle,
        identity: &ConsumerIdentity,
        binding: &MemberBinding,
        qos: QoSProfile,
        codec: MessageCodec,
        shutdown: CancellationToken,
    ) -> Result<Self, ConsumerError> {
        let subscription = TopicMessenger::subscribe_bound_set(
            messenger,
            &identity.core_node,
            &identity.instance_id,
            binding.target.clone(),
            &binding.member,
            &binding.producers,
            qos,
            shutdown,
        )
        .await?;
        Ok(Self {
            codec,
            subscription,
        })
    }

    /// The next message from any bound producer, or `None` once the
    /// subscription has shut down.
    pub async fn next_message(&mut self) -> Option<(ProducerRef, Message)> {
        self.subscription.on_next_message().await
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
}

impl ServiceClient {
    pub fn new(request: Option<MessageCodec>, response: Option<MessageCodec>) -> Self {
        Self { request, response }
    }

    /// Calls the member on `producer` and waits at most `deadline` for the
    /// response.
    pub async fn call(
        &self,
        messenger: &MessengerHandle,
        identity: &ConsumerIdentity,
        binding: &MemberBinding,
        producer: &ProducerRef,
        request: &Value,
        deadline: Duration,
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
        .await?;
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
}

impl ActionClient {
    /// `goal` converts the goal request, `feedback` each feedback message
    /// and `result` the completed result's body; each is `None` for a side
    /// the contract declares without a message format.
    pub fn new(
        goal: Option<MessageCodec>,
        feedback: Option<MessageCodec>,
        result: Option<MessageCodec>,
    ) -> Self {
        Self {
            goal,
            feedback,
            result,
        }
    }

    /// Sends `goal` to `producer`, waiting at most `deadline` for the
    /// producer's admission reply. The handle reports whether the goal was
    /// accepted and drives its feedback, result and cancellation.
    #[allow(clippy::too_many_arguments)]
    pub async fn fire_goal(
        &self,
        messenger: &MessengerHandle,
        identity: &ConsumerIdentity,
        binding: &MemberBinding,
        producer: &ProducerRef,
        goal: &Value,
        feedback_qos: QoSProfile,
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
            feedback_qos,
            deadline,
        )
        .await?;
        Ok(GoalHandle {
            inner,
            feedback: self.feedback.clone(),
            result: self.result.clone(),
        })
    }
}

/// A goal in flight.
pub struct GoalHandle {
    inner: ActionGoalHandle,
    feedback: Option<MessageCodec>,
    result: Option<MessageCodec>,
}

/// How a goal ended, as reported by the producer's result service.
#[derive(Debug, Clone, PartialEq)]
pub enum GoalOutcome {
    /// The goal ran to completion; the value is the decoded result body,
    /// an empty object for an action without a result format.
    Completed(Value),
    Cancelled,
    /// The producer dropped the goal before it settled.
    Abandoned,
    /// The producer no longer retains the goal's result.
    Expired,
}

impl GoalHandle {
    pub fn accepted(&self) -> bool {
        self.inner.goal_reply().accepted
    }

    /// The producer's reason for rejecting the goal, when it gave one.
    pub fn rejection_reason(&self) -> Option<&str> {
        self.inner.goal_reply().reason.as_deref()
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

    /// Requests the goal's terminal outcome, waiting at most `deadline`.
    pub async fn result(
        &self,
        messenger: &MessengerHandle,
        deadline: Duration,
    ) -> Result<GoalOutcome, ConsumerError> {
        let reply = ActionMessenger::request_result(messenger, &self.inner, deadline).await?;
        Ok(match reply.status {
            ResultStatus::Completed => {
                GoalOutcome::Completed(decode_optional(self.result.as_ref(), reply.body.as_ref())?)
            }
            ResultStatus::Cancelled => GoalOutcome::Cancelled,
            ResultStatus::Abandoned => GoalOutcome::Abandoned,
            ResultStatus::Expired => GoalOutcome::Expired,
        })
    }

    /// Asks the producer to cancel the goal, waiting at most `deadline` for
    /// its acknowledgement.
    pub async fn cancel(
        &self,
        messenger: &MessengerHandle,
        deadline: Duration,
    ) -> Result<CancelState, ConsumerError> {
        let acknowledgement =
            ActionMessenger::cancel_goal(messenger, &self.inner, deadline).await?;
        Ok(decode_cancel_ack(acknowledgement.payload_bytes().as_ref())?)
    }
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
