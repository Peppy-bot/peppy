use super::topics::Subscription;
use super::{
    MessengerHandle, PROBE_TIMEOUT, SERVICE_PROBE_PAYLOAD, ServiceEndpoint, TopicPublisher,
};
use crate::error::{Error, Result};
use crate::types::{Message, Payload};
use bytes::{BufMut, BytesMut};
use config::node::QoSProfile;
use pmi::{MessengerBackend, PublisherQoS};
use std::sync::Arc;
use tokio::time::Duration;

pub struct ActionMessenger;

/// Wire-format envelope helpers for goal payloads.
///
/// The codegen prepends a length-byte-prefixed `goal_id` to each goal
/// request payload so the server can route feedback to a per-goal topic.
/// `goal_id.len()` must fit in a single byte (UUID v4 strings are 36 bytes
/// — well within range). An empty `goal_id` encodes as a single 0 byte
/// followed by the user payload.
///
/// Layout: `[goal_id_len: u8][goal_id_bytes: ASCII][user_payload]`.
///
/// These helpers are used by the codegen-generated action server/client
/// pair. Callers that send goal payloads through other paths (e.g. the
/// internal `core-node-internal` action services that bypass the codegen)
/// must agree on the same wire format with their peer; see also
/// [`ActionFeedbackPublisherFactory::declare_unscoped`].
pub fn wrap_goal_payload(goal_id: &str, user_payload: &[u8]) -> Result<Payload> {
    if goal_id.len() > u8::MAX as usize {
        return Err(Error::InternalEncodingError {
            identifier: "action_goal_envelope".to_string(),
            reason: format!(
                "goal_id length {} exceeds wire limit {}",
                goal_id.len(),
                u8::MAX
            ),
        });
    }
    let mut buf = BytesMut::with_capacity(1 + goal_id.len() + user_payload.len());
    buf.put_u8(goal_id.len() as u8);
    buf.extend_from_slice(goal_id.as_bytes());
    buf.extend_from_slice(user_payload);
    Ok(Payload::from(buf.freeze()))
}

/// Decode an action goal envelope. Returns the embedded `goal_id` (may be
/// empty) and the user payload bytes. See [`wrap_goal_payload`].
pub fn unwrap_goal_payload(wire: &[u8]) -> Result<(&str, &[u8])> {
    let goal_id_len = *wire.first().ok_or_else(|| Error::InternalEncodingError {
        identifier: "action_goal_envelope".to_string(),
        reason: "wire payload is empty".to_string(),
    })? as usize;
    let body_start = 1 + goal_id_len;
    if wire.len() < body_start {
        return Err(Error::InternalEncodingError {
            identifier: "action_goal_envelope".to_string(),
            reason: format!("wire payload too short for declared goal_id_len {goal_id_len}"),
        });
    }
    let goal_id =
        std::str::from_utf8(&wire[1..body_start]).map_err(|err| Error::InternalEncodingError {
            identifier: "action_goal_envelope".to_string(),
            reason: format!("goal_id is not valid UTF-8: {err}"),
        })?;
    Ok((goal_id, &wire[body_start..]))
}

/// Per-goal feedback publisher used by action servers. Wraps a
/// [`TopicPublisher`] bound to a goal-specific feedback topic key. An
/// empty-payload publish is reserved as the end-of-stream sentinel — clients
/// receive `Err(Error::ActionFeedbackChannelClosed)` from
/// [`ActionGoalHandle::on_next_feedback`] when this sentinel arrives.
#[derive(Clone)]
pub struct ActionFeedbackPublisher {
    inner: TopicPublisher,
}

impl ActionFeedbackPublisher {
    pub(crate) fn new(inner: TopicPublisher) -> Self {
        Self { inner }
    }

    /// Publish a feedback message. The payload must be a non-empty
    /// capnp-encoded message — empty payloads are reserved for the
    /// end-of-stream sentinel and would close the stream prematurely.
    pub async fn publish(&self, payload: Payload) -> Result<()> {
        debug_assert!(
            !payload.is_empty(),
            "feedback payload must not be empty; empty is reserved for publish_end"
        );
        self.inner.publish(payload).await
    }

    /// Publish the end-of-stream sentinel (a zero-length payload). The next
    /// `on_next_feedback` call on the matching subscription resolves with
    /// `Err(Error::ActionFeedbackChannelClosed)`.
    pub async fn publish_end(&self) -> Result<()> {
        self.inner.publish(Payload::new()).await
    }
}

/// Vends per-goal [`ActionFeedbackPublisher`]s. Returned by
/// [`ActionMessenger::expose`] inside an [`ActionCreation`] — the codegen
/// calls [`Self::declare`] from inside `handle_goal_next_request` once a
/// goal is accepted, scoping the feedback topic to that single goal cycle.
#[derive(Clone)]
pub struct ActionFeedbackPublisherFactory {
    messenger: MessengerHandle,
    base_topic: String,
    qos: PublisherQoS,
}

impl ActionFeedbackPublisherFactory {
    pub(crate) fn new(messenger: MessengerHandle, base_topic: String, qos: PublisherQoS) -> Self {
        Self {
            messenger,
            base_topic,
            qos,
        }
    }

    /// Declare a feedback publisher whose topic key is the action's base
    /// feedback topic suffixed with `/<goal_id>`. End-of-stream signals
    /// emitted via [`ActionFeedbackPublisher::publish_end`] only reach
    /// subscribers of this specific goal cycle.
    pub async fn declare(&self, goal_id: &str) -> Result<ActionFeedbackPublisher> {
        let topic = format!("{}/{}", self.base_topic, goal_id);
        self.declare_with_topic(topic).await
    }

    /// Declare a feedback publisher bound to the action's base topic without
    /// per-goal scoping. Use this only for callers (e.g. core-node-internal
    /// service actions) that have not migrated to per-goal feedback streams —
    /// matching subscribers must use an empty `goal_id` in
    /// [`ActionMessenger::send_goal`].
    pub async fn declare_unscoped(&self) -> Result<ActionFeedbackPublisher> {
        self.declare_with_topic(self.base_topic.clone()).await
    }

    async fn declare_with_topic(&self, topic: String) -> Result<ActionFeedbackPublisher> {
        let inner = self
            .messenger
            .declare_publisher(topic.clone(), self.qos)
            .await?;
        Ok(ActionFeedbackPublisher::new(TopicPublisher::new(
            Arc::new(inner),
            topic,
        )))
    }
}

pub struct ActionGoalHandle {
    core_node: String,
    instance_id: String,
    node_name: String,
    action_name: String,
    target_core_node: Option<String>,
    target_instance_id: Option<String>,
    goal_response: Message,
    feedback: Subscription,
}

impl std::fmt::Debug for ActionGoalHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ActionGoalHandle")
            .field("core_node", &self.core_node)
            .field("instance_id", &self.instance_id)
            .field("node_name", &self.node_name)
            .field("action_name", &self.action_name)
            .field("target_core_node", &self.target_core_node)
            .field("target_instance_id", &self.target_instance_id)
            .finish_non_exhaustive()
    }
}

impl ActionGoalHandle {
    pub fn goal_response(&self) -> &Message {
        &self.goal_response
    }

    /// Receives the next feedback message.
    ///
    /// Returns `Err(Error::ActionFeedbackChannelClosed)` when the server has
    /// signaled end-of-stream by publishing a zero-length payload — emitted
    /// automatically when the server begins handling the result request,
    /// accepts a cancel request, or its cancel handler errors.
    pub async fn on_next_feedback(&mut self) -> Result<Message> {
        let msg = self
            .feedback
            .on_next_message()
            .await
            .ok_or(Error::ActionFeedbackChannelClosed)?;
        if msg.payload().is_empty() {
            return Err(Error::ActionFeedbackChannelClosed);
        }
        Ok(msg)
    }

    /// Attempts to receive the next feedback message without waiting.
    ///
    /// An empty-payload message resolves to `Err(ActionFeedbackChannelClosed)`,
    /// matching the semantics of [`Self::on_next_feedback`].
    pub fn try_next_feedback(&mut self) -> Result<Option<Message>> {
        match self.feedback.try_on_next_message() {
            Ok(message) if message.payload().is_empty() => Err(Error::ActionFeedbackChannelClosed),
            Ok(message) => Ok(Some(message)),
            Err(crate::types::TryRecvError::Empty) => Ok(None),
            Err(crate::types::TryRecvError::Disconnected) => {
                Err(Error::ActionFeedbackChannelClosed)
            }
        }
    }
}

// https://docs.ros.org/en/foxy/_images/Action-SingleActionClient.gif
pub struct ActionCreation {
    pub goal_service: ServiceEndpoint,
    pub cancel_service: ServiceEndpoint,
    pub feedback_publisher_factory: ActionFeedbackPublisherFactory,
    pub result_service: ServiceEndpoint,
}

impl ActionMessenger {
    pub async fn expose(
        messenger: &MessengerHandle,
        bound_core_node: &str,
        as_instance_id: &str,
        as_node_name: &str,
        as_action_name: &str,
    ) -> Result<ActionCreation> {
        messenger
            .expose_action(
                bound_core_node,
                as_node_name,
                as_action_name,
                as_instance_id,
            )
            .await
    }

    /// Sends a lightweight probe to check whether an action service is listening.
    #[allow(clippy::too_many_arguments)]
    pub async fn is_reachable(
        messenger: &MessengerHandle,
        bound_core_node: &str,
        as_instance_id: &str,
        target_node_name: &str,
        target_action_name: &str,
        target_core_node: Option<&str>,
        target_instance_id: Option<&str>,
    ) -> Result<bool> {
        match messenger
            .poll_service(
                "action",
                bound_core_node,
                as_instance_id,
                target_node_name,
                target_action_name,
                target_core_node,
                target_instance_id,
                Payload::from_static(SERVICE_PROBE_PAYLOAD),
                PROBE_TIMEOUT,
            )
            .await
        {
            Ok(_) => Ok(true),
            Err(Error::ServiceUnreachable { .. }) => Ok(false),
            Err(Error::ServiceTimeout { .. }) => Ok(true),
            Err(e) => Err(e),
        }
    }

    /// Send a goal to an action server.
    ///
    /// `goal_id` scopes the feedback subscription to a single goal cycle. Pass
    /// a unique value (e.g. UUID) per call to ensure End-of-stream signals from
    /// other goals don't terminate this client's stream. An empty string is
    /// reserved for legacy "unscoped" callers and matches a server publisher
    /// declared via [`ActionFeedbackPublisherFactory::declare_unscoped`].
    #[allow(clippy::too_many_arguments)]
    pub async fn send_goal(
        messenger: &MessengerHandle,
        as_core_node: &str,
        as_instance_id: &str,
        to_node_name: &str,
        to_action_name: &str,
        target_core_instance_id: Option<&str>,
        target_instance_id: Option<&str>,
        goal_id: &str,
        goal_payload: Payload,
        feedback_qos: QoSProfile,
        goal_timeout: Duration,
    ) -> Result<ActionGoalHandle> {
        let goal_segment = if goal_id.is_empty() {
            String::new()
        } else {
            format!("/{goal_id}")
        };
        let feedback_topic = {
            let sender_core_node = target_core_instance_id.unwrap_or("*");
            match target_instance_id {
                Some(target_instance_id) => {
                    format!(
                        "{as_core_node}/{sender_core_node}/{as_instance_id}/{target_instance_id}/action/{to_node_name}/{to_action_name}/feedback/{target_instance_id}{goal_segment}"
                    )
                }
                None => format!(
                    "{as_core_node}/*/{as_instance_id}/*/action/{to_node_name}/{to_action_name}/feedback/*{goal_segment}"
                ),
            }
        };
        let goal_service_name = format!("{to_action_name}/goal");

        let feedback_subscription = {
            let messenger = messenger.messenger.lock().await;
            messenger
                .subscribe(&feedback_topic, feedback_qos.into())
                .await
        }
        .map_err(Error::PeppyMessagingInterface)?;

        let goal_response = messenger
            .poll_service(
                "action",
                as_core_node,
                as_instance_id,
                to_node_name,
                &goal_service_name,
                target_core_instance_id,
                target_instance_id,
                goal_payload,
                goal_timeout,
            )
            .await?;

        Ok(ActionGoalHandle {
            core_node: as_core_node.to_string(),
            instance_id: as_instance_id.to_string(),
            node_name: to_node_name.to_string(),
            action_name: to_action_name.to_string(),
            target_core_node: target_core_instance_id.map(|name| name.to_string()),
            target_instance_id: target_instance_id.map(|id| id.to_string()),
            goal_response,
            feedback: Subscription::new(feedback_subscription),
        })
    }

    pub async fn cancel_goal(
        messenger_handle: &MessengerHandle,
        action_handle: &ActionGoalHandle,
        cancel_timeout: Duration,
    ) -> Result<Message> {
        Self::cancel_goal_with(
            messenger_handle,
            &action_handle.core_node,
            &action_handle.instance_id,
            &action_handle.node_name,
            &action_handle.action_name,
            action_handle.target_core_node.as_deref(),
            action_handle.target_instance_id.as_deref(),
            cancel_timeout,
        )
        .await
    }

    /// Like [`cancel_goal`](Self::cancel_goal) but accepts individual fields,
    /// allowing callers to avoid holding a lock on the goal handle during the
    /// network round-trip.
    #[allow(clippy::too_many_arguments)]
    pub async fn cancel_goal_with(
        messenger_handle: &MessengerHandle,
        core_node: &str,
        instance_id: &str,
        node_name: &str,
        action_name: &str,
        target_core_node: Option<&str>,
        target_instance_id: Option<&str>,
        cancel_timeout: Duration,
    ) -> Result<Message> {
        let cancel_service_name = format!("{action_name}/cancel");

        messenger_handle
            .poll_service(
                "action",
                core_node,
                instance_id,
                node_name,
                &cancel_service_name,
                target_core_node,
                target_instance_id,
                Payload::new(),
                cancel_timeout,
            )
            .await
    }

    pub async fn request_result(
        messenger_handle: &MessengerHandle,
        action_handle: &ActionGoalHandle,
        result_timeout: Duration,
    ) -> Result<Message> {
        Self::request_result_with(
            messenger_handle,
            &action_handle.core_node,
            &action_handle.instance_id,
            &action_handle.node_name,
            &action_handle.action_name,
            action_handle.target_core_node.as_deref(),
            action_handle.target_instance_id.as_deref(),
            result_timeout,
        )
        .await
    }

    /// Like [`request_result`](Self::request_result) but accepts individual
    /// fields, allowing callers to avoid holding a lock on the goal handle
    /// during the network round-trip.
    #[allow(clippy::too_many_arguments)]
    pub async fn request_result_with(
        messenger_handle: &MessengerHandle,
        core_node: &str,
        instance_id: &str,
        node_name: &str,
        action_name: &str,
        target_core_node: Option<&str>,
        target_instance_id: Option<&str>,
        result_timeout: Duration,
    ) -> Result<Message> {
        let result_service_name = format!("{action_name}/result");

        messenger_handle
            .poll_service(
                "action",
                core_node,
                instance_id,
                node_name,
                &result_service_name,
                target_core_node,
                target_instance_id,
                Payload::new(),
                result_timeout,
            )
            .await
            .map_err(|err| match err {
                Error::ServiceTimeout { instance_id, .. } => Error::ActionResultTimeout {
                    instance_id,
                    action_name: action_name.to_string(),
                },
                Error::ServiceUnreachable { instance_id, .. } => Error::ActionResultUnreachable {
                    instance_id,
                    action_name: action_name.to_string(),
                },
                other => other,
            })
    }
}
