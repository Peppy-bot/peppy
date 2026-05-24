use super::discovery::discover_producer;
use super::generate_short_id;
use super::topics::Subscription;
use super::{MessengerHandle, PROBE_TIMEOUT, ServiceEndpoint, TopicPublisher};
use crate::error::{Error, Result};
use crate::types::{Message, Payload};
use bytes::{BufMut, Bytes, BytesMut};
use config::node::QoSProfile;
use pmi::{ActionWireReceiver, ActionWireSender, PublisherQoS, SenderTarget, ServiceQueryKind};
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
/// `peppylib::messaging::NonEmptyPayload` import path.
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

    /// Publish a feedback message.
    pub async fn publish(&self, payload: NonEmptyPayload) -> Result<()> {
        self.inner.publish(payload.into_inner()).await
    }

    /// Publish the end-of-stream sentinel (a zero-length payload).
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
    receiver: ActionWireReceiver,
    qos: PublisherQoS,
}

impl ActionFeedbackPublisherFactory {
    pub(crate) fn new(
        messenger: MessengerHandle,
        receiver: ActionWireReceiver,
        qos: PublisherQoS,
    ) -> Self {
        Self {
            messenger,
            receiver,
            qos,
        }
    }

    /// Standard server-side entry point: unwrap the goal envelope, declare
    /// a feedback publisher on the per-goal topic scoped to the link_id the
    /// consumer targeted, and return both alongside the user payload so the
    /// caller can dispatch it to the goal handler.
    ///
    /// `link_id` comes from the goal request's parsed keyexpr (surfaced via
    /// [`crate::messaging::ServiceRequestContext::link_id`]). A producer
    /// bound to multiple link_ids will see different link_ids for different
    /// goal requests, and each goal's feedback must be addressed back under
    /// the link_id its consumer subscribed for.
    pub async fn declare_from_wire(&self, link_id: &str, wire: Bytes) -> Result<DeclaredFeedback> {
        let (goal_id, user_payload_offset) = {
            let (goal_id, user_payload) = unwrap_goal_payload(wire.as_ref())?;
            // The goal_id is appended to the feedback topic to scope the
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
        let publisher = self.declare(link_id, &goal_id).await?;
        let user_payload = wire.slice(user_payload_offset..);
        Ok(DeclaredFeedback {
            publisher,
            goal_id,
            user_payload,
        })
    }

    async fn declare(&self, link_id: &str, goal_id: &str) -> Result<ActionFeedbackPublisher> {
        let inner = self
            .messenger
            .declare_action_feedback_publisher(&self.receiver, link_id, goal_id, self.qos)
            .await?;
        Ok(ActionFeedbackPublisher::new(TopicPublisher::new(Arc::new(
            inner,
        ))))
    }
}

pub struct ActionGoalHandle {
    sender: ActionWireSender,
    goal_id: String,
    goal_response: Message,
    feedback: Subscription,
}

impl std::fmt::Debug for ActionGoalHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ActionGoalHandle")
            .field("sender", &self.sender)
            .field("goal_id", &self.goal_id)
            .finish_non_exhaustive()
    }
}

impl ActionGoalHandle {
    /// The wire sender used to dispatch this goal. Cloned by external wrappers
    /// (e.g. Python bindings) that need to issue cancel/result calls without
    /// holding a lock on the goal handle.
    pub fn sender(&self) -> &ActionWireSender {
        &self.sender
    }

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
    /// Expose an action server. The producer declares its queryables under
    /// the reserved default `_` link_id segment; consumers pin a specific
    /// producer by `target_instance_id` derived from the consumer's
    /// binding map. `as_identity` must match what callers pass to
    /// [`Self::send_goal`].
    pub async fn expose(
        messenger: &MessengerHandle,
        bound_core_node: &str,
        as_instance_id: &str,
        as_identity: SenderTarget,
        as_action_name: &str,
    ) -> Result<ActionCreation> {
        let recv = ActionWireReceiver::new(
            bound_core_node,
            as_instance_id,
            as_identity,
            &[],
            as_action_name,
        )?;
        messenger.expose_action(&recv).await
    }

    /// Probe an action service. `to_link_id` `None` targets the default
    /// link_id; `Some(value)` targets a specific producer link_id.
    #[allow(clippy::too_many_arguments)]
    pub async fn is_reachable(
        messenger: &MessengerHandle,
        bound_core_node: &str,
        as_instance_id: &str,
        to_target: SenderTarget,
        to_link_id: Option<&str>,
        to_action_name: &str,
        target_core_node: Option<&str>,
        target_instance_id: Option<&str>,
    ) -> Result<bool> {
        let sender = ActionWireSender::new(
            bound_core_node,
            as_instance_id,
            target_core_node,
            target_instance_id,
            to_target,
            to_link_id,
            to_action_name,
        )?;
        match messenger
            .poll_service(
                &sender.goal_service(),
                Payload::new(),
                ServiceQueryKind::Probe,
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
    /// wraps `user_payload` in the per-goal envelope, subscribes to the
    /// matching feedback topic, and polls the goal service.
    ///
    /// `to_target` must match the [`SenderTarget`] the action server used
    /// in [`Self::expose`]. `to_link_id` `None` targets the default
    /// link_id; `Some(value)` targets a specific producer link_id.
    ///
    /// When either `target_core_node` or `target_instance_id` is `None`
    /// (wildcard / from_any), this performs a discover-then-pin sequence:
    /// a lightweight probe to the goal sub-service identifies a single
    /// responding producer, then the real goal is delivered pinned to that
    /// producer. The probe is filtered server-side before the user handler
    /// runs (see [`crate::messaging::services::ServiceEndpoint`]), so
    /// non-winning producers never execute the goal handler. Without this,
    /// every matching producer would run the handler concurrently; for
    /// actions with side effects (motor commands, file writes) that is a
    /// real-world safety hazard. Fully pinned callers (both `target_*`
    /// `Some`) skip discovery and pay no overhead.
    #[allow(clippy::too_many_arguments)]
    pub async fn send_goal(
        messenger: &MessengerHandle,
        as_core_node: &str,
        as_instance_id: &str,
        to_target: SenderTarget,
        to_link_id: Option<&str>,
        to_action_name: &str,
        target_core_node: Option<&str>,
        target_instance_id: Option<&str>,
        user_payload: Payload,
        feedback_qos: QoSProfile,
        goal_timeout: Duration,
    ) -> Result<ActionGoalHandle> {
        let goal_id = generate_goal_id();
        let goal_payload = wrap_goal_payload(&goal_id, user_payload.as_ref())?;

        // Discover a single producer when the caller did not pin either
        // addressing slot. The probe runs server-side without invoking the
        // goal handler; only the discovered producer will receive the real
        // goal request.
        let (resolved_core, resolved_inst) =
            if target_instance_id.is_none() || target_core_node.is_none() {
                let probe_sender = ActionWireSender::new(
                    as_core_node,
                    as_instance_id,
                    target_core_node,
                    target_instance_id,
                    to_target.clone(),
                    to_link_id,
                    to_action_name,
                )?;
                // Cap discovery at PROBE_TIMEOUT or the caller's goal budget,
                // whichever is shorter, so a tight `goal_timeout` still fails
                // fast against unreachable producers.
                let discovery_timeout = goal_timeout.min(PROBE_TIMEOUT);
                let (core, inst) =
                    discover_producer(messenger, &probe_sender.goal_service(), discovery_timeout)
                        .await?;
                (Some(core), Some(inst))
            } else {
                (
                    target_core_node.map(str::to_string),
                    target_instance_id.map(str::to_string),
                )
            };

        let sender = ActionWireSender::new(
            as_core_node,
            as_instance_id,
            resolved_core.as_deref(),
            resolved_inst.as_deref(),
            to_target,
            to_link_id,
            to_action_name,
        )?;

        // Feedback subscription is built from the pinned sender, so its
        // wire keyexpr targets only the discovered producer. Losers cannot
        // publish feedback under this goal_id to a slot we are listening on.
        let feedback_subscription = messenger
            .subscribe_action_feedback(&sender, &goal_id, feedback_qos.into())
            .await?;

        let goal_response = messenger
            .poll_service(
                &sender.goal_service(),
                goal_payload,
                ServiceQueryKind::UserRequest,
                goal_timeout,
            )
            .await?;

        Ok(ActionGoalHandle {
            sender,
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
        Self::cancel_with_sender(messenger_handle, &action_handle.sender, cancel_timeout).await
    }

    /// Like [`cancel_goal`](Self::cancel_goal) but takes a cloned sender
    /// directly. External wrappers (e.g. Python bindings) hold a clone so
    /// they can cancel without locking the goal handle during the network
    /// round-trip.
    pub async fn cancel_with_sender(
        messenger_handle: &MessengerHandle,
        sender: &ActionWireSender,
        cancel_timeout: Duration,
    ) -> Result<Message> {
        messenger_handle
            .poll_service(
                &sender.cancel_service(),
                Payload::new(),
                ServiceQueryKind::UserRequest,
                cancel_timeout,
            )
            .await
    }

    pub async fn request_result(
        messenger_handle: &MessengerHandle,
        action_handle: &ActionGoalHandle,
        result_timeout: Duration,
    ) -> Result<Message> {
        Self::request_result_with_sender(messenger_handle, &action_handle.sender, result_timeout)
            .await
    }

    /// Like [`request_result`](Self::request_result) but takes a cloned sender
    /// directly. Mirrors [`cancel_with_sender`](Self::cancel_with_sender).
    pub async fn request_result_with_sender(
        messenger_handle: &MessengerHandle,
        sender: &ActionWireSender,
        result_timeout: Duration,
    ) -> Result<Message> {
        let action_name = sender.to_action_name().to_string();
        messenger_handle
            .poll_service(
                &sender.result_service(),
                Payload::new(),
                ServiceQueryKind::UserRequest,
                result_timeout,
            )
            .await
            .map_err(|err| Self::map_result_error(err, &action_name))
    }

    fn map_result_error(err: Error, action_name: &str) -> Error {
        match err {
            Error::ServiceTimeout { instance_id, .. } => Error::ActionResultTimeout {
                instance_id,
                action_name: action_name.to_string(),
            },
            Error::ServiceUnreachable { instance_id, .. } => Error::ActionResultUnreachable {
                instance_id,
                action_name: action_name.to_string(),
            },
            other => other,
        }
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
