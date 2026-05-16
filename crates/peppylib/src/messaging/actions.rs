use super::generate_short_id;
use super::topics::Subscription;
use super::{
    MessengerHandle, PROBE_TIMEOUT, SERVICE_PROBE_PAYLOAD, ServiceEndpoint, TopicPublisher,
};
use crate::error::{Error, Result};
use crate::types::{Message, Payload};
use bytes::{BufMut, Bytes, BytesMut};
use config::node::QoSProfile;
use pmi::{MessengerBackend, PublisherQoS};
use std::sync::Arc;
use tokio::time::Duration;

pub struct ActionMessenger;

/// Unique `goal_id` for `ActionMessenger::send_goal` and per-goal feedback
/// topic scoping. Returns 16 hex chars (64 bits of entropy).
pub fn generate_goal_id() -> String {
    generate_short_id("goal")
}

const ACTION_GOAL_ENVELOPE: &str = "action_goal_envelope";

fn envelope_error(reason: impl Into<String>) -> Error {
    Error::InternalEncodingError {
        identifier: ACTION_GOAL_ENVELOPE.to_string(),
        reason: reason.into(),
    }
}

/// Wrap a user goal payload with a length-prefixed `goal_id` so the server
/// can route feedback to a per-goal topic. `goal_id` must be non-empty and
/// at most 255 bytes ([`generate_goal_id`] satisfies both).
///
/// Layout: `[goal_id_len: u8][goal_id_bytes: ASCII][user_payload]`.
///
/// All goal-payload-emitting callers must wrap here, and servers must call
/// [`unwrap_goal_payload`] before deserializing.
pub fn wrap_goal_payload(goal_id: &str, user_payload: &[u8]) -> Result<Payload> {
    if goal_id.is_empty() {
        return Err(envelope_error("goal_id must be non-empty"));
    }
    if goal_id.len() > u8::MAX as usize {
        return Err(envelope_error(format!(
            "goal_id length {} exceeds wire limit {}",
            goal_id.len(),
            u8::MAX
        )));
    }
    let mut buf = BytesMut::with_capacity(1 + goal_id.len() + user_payload.len());
    buf.put_u8(goal_id.len() as u8);
    buf.extend_from_slice(goal_id.as_bytes());
    buf.extend_from_slice(user_payload);
    Ok(Payload::from(buf.freeze()))
}

/// Decode an action goal envelope. Returns the embedded `goal_id` (always
/// non-empty) and the user payload bytes. See [`wrap_goal_payload`].
pub fn unwrap_goal_payload(wire: &[u8]) -> Result<(&str, &[u8])> {
    let goal_id_len = *wire
        .first()
        .ok_or_else(|| envelope_error("wire payload is empty"))? as usize;
    if goal_id_len == 0 {
        return Err(envelope_error("goal_id is empty"));
    }
    let body_start = 1 + goal_id_len;
    if wire.len() < body_start {
        return Err(envelope_error(format!(
            "wire payload too short for declared goal_id_len {goal_id_len}"
        )));
    }
    let goal_id = std::str::from_utf8(&wire[1..body_start])
        .map_err(|err| envelope_error(format!("goal_id is not valid UTF-8: {err}")))?;
    Ok((goal_id, &wire[body_start..]))
}

/// Whether a goal_id is safe to splice into the feedback topic key
/// expression. Restricts to a single non-empty segment of ASCII
/// alphanumerics, `_`, and `-` so wildcard markers (`*`, `**`, `+`, `#`)
/// and topic separators (`/`) cannot escape the per-goal scope.
fn is_safe_goal_id(goal_id: &str) -> bool {
    !goal_id.is_empty()
        && goal_id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// Re-exported from `core-node-api` so callers can use the existing
/// `peppylib::messaging::NonEmptyPayload` import path. The type lives in
/// `core-node-api` next to [`Payload`] so capnp `encode()` helpers can
/// return it directly without `core-node-api` having to depend on
/// `peppylib`.
pub use core_node_api::{EmptyPayloadError, NonEmptyPayload};

/// Per-goal feedback publisher used by action servers. The end-of-stream
/// sentinel is a zero-length payload published via
/// [`ActionFeedbackPublisher::publish_end`]; clients then receive
/// `Err(Error::ActionFeedbackChannelClosed)` from
/// [`ActionGoalHandle::on_next_feedback`]. Regular feedback publishes go
/// through [`ActionFeedbackPublisher::publish`], which takes a
/// [`NonEmptyPayload`] so empty payloads cannot reach the publish path.
#[derive(Clone)]
pub struct ActionFeedbackPublisher {
    inner: TopicPublisher,
}

/// Whether `message`'s payload is the end-of-stream sentinel emitted by
/// [`ActionFeedbackPublisher::publish_end`].
fn is_end_sentinel(message: &Message) -> bool {
    message.payload().is_empty()
}

impl ActionFeedbackPublisher {
    pub(crate) fn new(inner: TopicPublisher) -> Self {
        Self { inner }
    }

    /// Publish a feedback message. The [`NonEmptyPayload`] type guarantees
    /// non-emptiness at the type level, since an empty payload is reserved
    /// for the end-of-stream sentinel emitted by [`Self::publish_end`].
    pub async fn publish(&self, payload: NonEmptyPayload) -> Result<()> {
        self.inner.publish(payload.into_inner()).await
    }

    /// Publish the end-of-stream sentinel (a zero-length payload). The next
    /// `on_next_feedback` call on the matching subscription resolves with
    /// `Err(Error::ActionFeedbackChannelClosed)`.
    pub async fn publish_end(&self) -> Result<()> {
        self.inner.publish(Payload::new()).await
    }
}

/// Outcome of [`ActionFeedbackPublisherFactory::declare_from_wire`]:
/// the per-goal feedback publisher, the embedded `goal_id`, and the
/// envelope-stripped user payload ready to be decoded by the goal handler.
pub struct DeclaredFeedback {
    pub publisher: ActionFeedbackPublisher,
    pub goal_id: String,
    pub user_payload: Bytes,
}

/// Vends per-goal [`ActionFeedbackPublisher`]s. Returned by
/// [`ActionMessenger::expose`] inside an [`ActionCreation`]. Server-side
/// callers feed each incoming goal request's wire bytes to
/// [`Self::declare_from_wire`], which extracts the client-originated
/// `goal_id` and declares a feedback publisher scoped to that goal cycle.
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

    /// Standard server-side entry point: unwrap the goal envelope, declare
    /// a feedback publisher on the per-goal topic, and return both alongside
    /// the user payload so the caller can dispatch it to the goal handler.
    pub async fn declare_from_wire(&self, wire: Bytes) -> Result<DeclaredFeedback> {
        let (goal_id, user_payload_offset) = {
            let (goal_id, user_payload) = unwrap_goal_payload(wire.as_ref())?;
            // The goal_id is appended to `base_topic` to scope the feedback
            // publisher per goal cycle. Reject anything that could let a
            // malicious or malformed envelope escape that scope (extra
            // segments, Zenoh wildcards, ...) so the publisher cannot be
            // steered onto a topic the server didn't intend.
            if !is_safe_goal_id(goal_id) {
                return Err(envelope_error(format!(
                    "goal_id contains unsafe characters: {goal_id:?}"
                )));
            }
            // Cheap derived offset so we can slice the original `Bytes`
            // and skip the `Vec<u8>` copy of the user payload.
            let offset = wire.len() - user_payload.len();
            (goal_id.to_string(), offset)
        };
        let publisher = self.declare(&goal_id).await?;
        let user_payload = wire.slice(user_payload_offset..);
        Ok(DeclaredFeedback {
            publisher,
            goal_id,
            user_payload,
        })
    }

    async fn declare(&self, goal_id: &str) -> Result<ActionFeedbackPublisher> {
        let topic = format!("{}/{}", self.base_topic, goal_id);
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
    iface_name: String,
    iface_tag: String,
    action_name: String,
    target_core_node: Option<String>,
    target_instance_id: Option<String>,
    goal_id: String,
    goal_response: Message,
    feedback: Subscription,
}

impl std::fmt::Debug for ActionGoalHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ActionGoalHandle")
            .field("core_node", &self.core_node)
            .field("instance_id", &self.instance_id)
            .field("node_name", &self.node_name)
            .field("iface_name", &self.iface_name)
            .field("iface_tag", &self.iface_tag)
            .field("action_name", &self.action_name)
            .field("target_core_node", &self.target_core_node)
            .field("target_instance_id", &self.target_instance_id)
            .field("goal_id", &self.goal_id)
            .finish_non_exhaustive()
    }
}

impl ActionGoalHandle {
    pub fn goal_response(&self) -> &Message {
        &self.goal_response
    }

    /// Correlation ID generated by `send_goal` and embedded in the goal
    /// envelope. Useful for tracing or logging.
    pub fn goal_id(&self) -> &str {
        &self.goal_id
    }

    /// Receives the next feedback message.
    ///
    /// Returns `Err(Error::ActionFeedbackChannelClosed)` when the server
    /// publishes the end-of-stream sentinel: the framework emits it when
    /// the server begins handling the result request, accepts a cancel,
    /// or the cancel handler errors.
    pub async fn on_next_feedback(&mut self) -> Result<Message> {
        let msg = self
            .feedback
            .on_next_message()
            .await
            .ok_or(Error::ActionFeedbackChannelClosed)?;
        if is_end_sentinel(&msg) {
            return Err(Error::ActionFeedbackChannelClosed);
        }
        Ok(msg)
    }

    /// Non-blocking variant of [`Self::on_next_feedback`].
    pub fn try_next_feedback(&mut self) -> Result<Option<Message>> {
        match self.feedback.try_on_next_message() {
            Ok(message) if is_end_sentinel(&message) => Err(Error::ActionFeedbackChannelClosed),
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
    /// `iface_name`/`iface_tag` scope the wire path to a `conforms_to` interface;
    /// pass `NATIVE_IFACE_SEGMENT_NAME`/`NATIVE_IFACE_SEGMENT_TAG` for native actions.
    #[allow(clippy::too_many_arguments)]
    pub async fn expose(
        messenger: &MessengerHandle,
        bound_core_node: &str,
        as_instance_id: &str,
        as_node_name: &str,
        iface_name: &str,
        iface_tag: &str,
        as_action_name: &str,
    ) -> Result<ActionCreation> {
        messenger
            .expose_action(
                bound_core_node,
                as_node_name,
                iface_name,
                iface_tag,
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
        iface_name: &str,
        iface_tag: &str,
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
                iface_name,
                iface_tag,
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

    /// Send a goal to an action server. Generates a fresh `goal_id`,
    /// wraps `user_payload` in the per-goal envelope, and subscribes to
    /// the matching feedback topic before polling the goal service.
    ///
    /// `iface_name`/`iface_tag` must match the segments the action server used in
    /// [`Self::expose`].
    #[allow(clippy::too_many_arguments)]
    pub async fn send_goal(
        messenger: &MessengerHandle,
        as_core_node: &str,
        as_instance_id: &str,
        to_node_name: &str,
        iface_name: &str,
        iface_tag: &str,
        to_action_name: &str,
        target_core_instance_id: Option<&str>,
        target_instance_id: Option<&str>,
        user_payload: Payload,
        feedback_qos: QoSProfile,
        goal_timeout: Duration,
    ) -> Result<ActionGoalHandle> {
        let goal_id = generate_goal_id();
        let goal_payload = wrap_goal_payload(&goal_id, user_payload.as_ref())?;
        let normalized_iface_tag = crate::messaging::normalize_iface_segment(iface_tag);
        let feedback_topic = {
            let sender_core_node = target_core_instance_id.unwrap_or("*");
            match target_instance_id {
                Some(target_instance_id) => {
                    format!(
                        "{as_core_node}/{sender_core_node}/{as_instance_id}/{target_instance_id}/action/{to_node_name}/{iface_name}/{normalized_iface_tag}/{to_action_name}/feedback/{target_instance_id}/{goal_id}"
                    )
                }
                None => format!(
                    "{as_core_node}/{sender_core_node}/{as_instance_id}/*/action/{to_node_name}/{iface_name}/{normalized_iface_tag}/{to_action_name}/feedback/*/{goal_id}"
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
                iface_name,
                iface_tag,
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
            iface_name: iface_name.to_string(),
            iface_tag: iface_tag.to_string(),
            action_name: to_action_name.to_string(),
            target_core_node: target_core_instance_id.map(|name| name.to_string()),
            target_instance_id: target_instance_id.map(|id| id.to_string()),
            goal_id,
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
            &action_handle.iface_name,
            &action_handle.iface_tag,
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
        iface_name: &str,
        iface_tag: &str,
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
                iface_name,
                iface_tag,
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
            &action_handle.iface_name,
            &action_handle.iface_tag,
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
        iface_name: &str,
        iface_tag: &str,
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
                iface_name,
                iface_tag,
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

#[cfg(test)]
mod envelope_tests {
    use super::*;

    fn assert_envelope_error(result: Result<Payload>) {
        match result {
            Err(Error::InternalEncodingError { identifier, .. }) => {
                assert_eq!(identifier, "action_goal_envelope");
            }
            Err(other) => panic!("expected InternalEncodingError, got {other:?}"),
            Ok(_) => panic!("expected error, got Ok"),
        }
    }

    fn assert_unwrap_error(result: Result<(&str, &[u8])>) {
        match result {
            Err(Error::InternalEncodingError { identifier, .. }) => {
                assert_eq!(identifier, "action_goal_envelope");
            }
            Err(other) => panic!("expected InternalEncodingError, got {other:?}"),
            Ok(_) => panic!("expected error, got Ok"),
        }
    }

    #[test]
    fn wrap_then_unwrap_roundtrip() {
        let wire = wrap_goal_payload("goal-abc", b"hello").expect("wrap should succeed");
        let (goal_id, body) = unwrap_goal_payload(wire.as_ref()).expect("unwrap should succeed");
        assert_eq!(goal_id, "goal-abc");
        assert_eq!(body, b"hello");
    }

    #[test]
    fn wrap_unwrap_with_empty_user_payload() {
        let wire = wrap_goal_payload("goal-xyz", b"").expect("wrap should succeed");
        let (goal_id, body) = unwrap_goal_payload(wire.as_ref()).expect("unwrap should succeed");
        assert_eq!(goal_id, "goal-xyz");
        assert!(body.is_empty());
    }

    #[test]
    fn wrap_rejects_empty_goal_id() {
        assert_envelope_error(wrap_goal_payload("", b"payload"));
    }

    #[test]
    fn wrap_rejects_goal_id_over_255_bytes() {
        let oversized = "a".repeat(256);
        assert_envelope_error(wrap_goal_payload(&oversized, b"payload"));
    }

    #[test]
    fn wrap_accepts_max_length_goal_id() {
        let max_len = "a".repeat(255);
        let wire = wrap_goal_payload(&max_len, b"x").expect("wrap should succeed at 255 bytes");
        let (goal_id, body) = unwrap_goal_payload(wire.as_ref()).expect("unwrap should succeed");
        assert_eq!(goal_id, max_len);
        assert_eq!(body, b"x");
    }

    #[test]
    fn unwrap_rejects_empty_wire() {
        assert_unwrap_error(unwrap_goal_payload(&[]));
    }

    #[test]
    fn unwrap_rejects_zero_length_prefix() {
        assert_unwrap_error(unwrap_goal_payload(&[0x00]));
    }

    #[test]
    fn unwrap_rejects_truncated_wire() {
        // Declares a 5-byte goal_id but only provides 1 byte after the length prefix.
        assert_unwrap_error(unwrap_goal_payload(&[0x05, b'a']));
    }

    #[test]
    fn unwrap_rejects_non_utf8_goal_id() {
        // 0xFF / 0xFE form an invalid UTF-8 sequence.
        assert_unwrap_error(unwrap_goal_payload(&[0x02, 0xFF, 0xFE, b'p']));
    }
}
