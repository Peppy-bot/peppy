use super::discovery::discover_producer;
use super::generate_short_id;
use super::topics::Subscription;
use super::{MessengerHandle, PROBE_TIMEOUT, ServiceEndpoint, ServiceResponder, TopicPublisher};
use crate::error::{Error, Result};
use crate::runtime::{CancellationToken, TaskHandle, spawn};
use crate::types::{Message, Payload};
use bytes::{BufMut, Bytes, BytesMut};
use config::node::QoSProfile;
use pmi::{ActionWireReceiver, ActionWireSender, PublisherQoS, SenderTarget, ServiceQueryKind};
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;
use tokio::sync::Mutex as TokioMutex;
use tokio::time::Duration;
use tracing::warn;

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
        let recv =
            ActionWireReceiver::new(bound_core_node, as_instance_id, as_identity, as_action_name)?;
        messenger.expose_action(&recv).await
    }

    /// Probe an action service.
    #[allow(clippy::too_many_arguments)]
    pub async fn is_reachable(
        messenger: &MessengerHandle,
        bound_core_node: &str,
        as_instance_id: &str,
        to_target: SenderTarget,
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
    /// in [`Self::expose`].
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
        let started_at = Instant::now();
        let (resolved_core, resolved_inst) =
            if target_instance_id.is_none() || target_core_node.is_none() {
                let probe_sender = ActionWireSender::new(
                    as_core_node,
                    as_instance_id,
                    target_core_node,
                    target_instance_id,
                    to_target.clone(),
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
            to_action_name,
        )?;

        // Feedback subscription is built from the pinned sender, so its
        // wire keyexpr targets only the discovered producer. Losers cannot
        // publish feedback under this goal_id to a slot we are listening on.
        let feedback_subscription = messenger
            .subscribe_action_feedback(&sender, &goal_id, feedback_qos.into())
            .await?;

        // Discovery counts against the caller's single end-to-end budget;
        // pass only the remaining slice to `poll_service` so a tight
        // `goal_timeout` can't be silently doubled by a slow probe.
        let remaining_goal_budget = goal_timeout.saturating_sub(started_at.elapsed());
        if remaining_goal_budget.is_zero() {
            return Err(Error::ServiceTimeout {
                instance_id: resolved_inst.clone(),
                service_name: to_action_name.to_string(),
            });
        }
        let goal_response = messenger
            .poll_service(
                &sender.goal_service(),
                goal_payload,
                ServiceQueryKind::UserRequest,
                remaining_goal_budget,
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
        Self::cancel_with_sender(
            messenger_handle,
            &action_handle.sender,
            &action_handle.goal_id,
            cancel_timeout,
        )
        .await
    }

    /// Like [`cancel_goal`](Self::cancel_goal) but takes a cloned sender and
    /// `goal_id` directly. External wrappers (e.g. Python bindings) hold a
    /// clone so they can cancel without locking the goal handle during the
    /// network round-trip.
    ///
    /// The `goal_id` is sent in the cancel request payload (via the same
    /// length-prefixed envelope as goals) so the server-side concurrent-action
    /// engine can route the cancel to the right in-flight goal.
    pub async fn cancel_with_sender(
        messenger_handle: &MessengerHandle,
        sender: &ActionWireSender,
        goal_id: &str,
        cancel_timeout: Duration,
    ) -> Result<Message> {
        let payload = wrap_goal_payload(goal_id, &[])?;
        messenger_handle
            .poll_service(
                &sender.cancel_service(),
                payload,
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
        Self::request_result_with_sender(
            messenger_handle,
            &action_handle.sender,
            &action_handle.goal_id,
            result_timeout,
        )
        .await
    }

    /// Like [`request_result`](Self::request_result) but takes a cloned sender
    /// and `goal_id` directly. Mirrors [`cancel_with_sender`](Self::cancel_with_sender);
    /// the `goal_id` rides in the result request payload for server-side routing.
    pub async fn request_result_with_sender(
        messenger_handle: &MessengerHandle,
        sender: &ActionWireSender,
        goal_id: &str,
        result_timeout: Duration,
    ) -> Result<Message> {
        let action_name = sender.to_action_name().to_string();
        let payload = wrap_goal_payload(goal_id, &[])?;
        messenger_handle
            .poll_service(
                &sender.result_service(),
                payload,
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

// ---------------------------------------------------------------------------
// Concurrent-action engine
// ---------------------------------------------------------------------------

/// Encode the fixed cancel-ack response (`accepted`, optional `error_message`)
/// the concurrent-action engine sends in reply to a cancel request. The worker
/// reacts to the cancel *signal*; it never produces this payload, so the bytes
/// are encoded here once and reused for both Rust and Python servers.
///
/// The layout is positionally wire-compatible with the codegen's per-action
/// `CancelResponse` reader — see `schemas/action_cancel.capnp` and
/// `cancel_action_response_format()` in generator-internal.
pub fn encode_cancel_ack(accepted: bool, error_message: Option<&str>) -> Result<Payload> {
    let mut builder = ::capnp::message::Builder::new_default();
    {
        let mut root =
            builder.init_root::<crate::action_cancel_capnp::action_cancel_response::Builder>();
        root.set_accepted(accepted);
        if let Some(message) = error_message {
            root.set_error_message(message);
        }
    }
    crate::encoding::encode_message(&builder)
}

/// Decode a cancel-ack produced by [`encode_cancel_ack`] into
/// `(accepted, error_message)`. Used by tests and any caller that needs to read
/// the framework's cancel reply without the generated per-action type.
pub fn decode_cancel_ack(payload: &[u8]) -> Result<(bool, Option<String>)> {
    let reader = crate::encoding::decode_message(payload)?;
    let root = reader
        .get_root::<crate::action_cancel_capnp::action_cancel_response::Reader>()
        .map_err(|e| Error::Deserialization(e.to_string()))?;
    let accepted = root.get_accepted();
    let error_message = if root.has_error_message() {
        Some(
            root.get_error_message()
                .map_err(|e| Error::Deserialization(e.to_string()))?
                .to_string()
                .map_err(|e| Error::Deserialization(e.to_string()))?,
        )
    } else {
        None
    };
    Ok((accepted, error_message))
}

/// Rendezvous between a goal's [`GoalContext::complete`] and the result service
/// loop. Either side may arrive first: the result request parks its responder
/// here when the goal is still running; `complete` stores the value for a
/// result request that has not arrived yet. The first such poll fetches the
/// value and evicts the slot (deliver-once); if no poll arrives, the slot is
/// evicted after a grace window when the [`GoalContext`] drops.
enum ResultRendezvous {
    Empty,
    ValueReady(Payload),
    ResponderWaiting(ServiceResponder),
}

/// Per-goal routing state held in the registry for the life of the goal.
struct GoalSlot {
    cancel: CancellationToken,
    result: Arc<TokioMutex<ResultRendezvous>>,
}

/// `goal_id` → live goal. Guarded by a `std` mutex so the cancel/result loops
/// and [`GoalContext`] drop can touch it without holding a lock across `.await`.
type GoalRegistry = Arc<StdMutex<HashMap<String, GoalSlot>>>;

/// Extract the `goal_id` carried by a cancel/result request payload (the same
/// length-prefixed envelope goals use, with an empty body).
fn goal_id_from_request(payload: &Payload) -> Result<String> {
    let (goal_id, _) = unwrap_goal_payload(payload.as_ref())?;
    Ok(goal_id.to_string())
}

/// Background loop: routes each incoming cancel request to the matching live
/// goal's [`CancellationToken`] and replies with a cancel-ack whose `accepted`
/// reflects whether a goal with that `goal_id` was in flight.
async fn run_cancel_loop(
    mut cancel_service: ServiceEndpoint,
    registry: GoalRegistry,
    stop: CancellationToken,
) {
    loop {
        let next = tokio::select! {
            _ = stop.cancelled() => return,
            next = cancel_service.recv_next_request() => next,
        };
        let (context, responder) = match next {
            Ok(Some(pair)) => pair,
            Ok(None) => return,
            Err(err) => {
                warn!(%err, "action cancel loop stopped");
                return;
            }
        };

        let ack = match goal_id_from_request(&context.message().payload()) {
            Ok(goal_id) => {
                // Clone the token out under the lock, then fire + respond
                // without holding the registry lock across the network reply.
                let token = registry
                    .lock()
                    .unwrap()
                    .get(&goal_id)
                    .map(|s| s.cancel.clone());
                match token {
                    Some(token) => {
                        token.cancel();
                        encode_cancel_ack(true, None)
                    }
                    None => encode_cancel_ack(false, Some("no active goal for goal_id")),
                }
            }
            Err(_) => encode_cancel_ack(false, Some("malformed cancel request payload")),
        };

        match ack {
            Ok(payload) => {
                let _ = responder.respond(payload).await;
            }
            Err(err) => {
                let _ = responder.respond_error(err.to_string()).await;
            }
        }
    }
}

/// Background loop: routes each incoming result request to the matching live
/// goal. If the goal has completed it replies immediately (and keeps the value
/// for late polls); otherwise it parks the responder until
/// [`GoalContext::complete`] delivers the result.
async fn run_result_loop(
    mut result_service: ServiceEndpoint,
    registry: GoalRegistry,
    stop: CancellationToken,
) {
    loop {
        let next = tokio::select! {
            _ = stop.cancelled() => return,
            next = result_service.recv_next_request() => next,
        };
        let (context, responder) = match next {
            Ok(Some(pair)) => pair,
            Ok(None) => return,
            Err(err) => {
                warn!(%err, "action result loop stopped");
                return;
            }
        };

        let goal_id = match goal_id_from_request(&context.message().payload()) {
            Ok(goal_id) => goal_id,
            Err(_) => {
                let _ = responder
                    .respond_error("malformed result request payload".to_string())
                    .await;
                continue;
            }
        };

        let rendezvous = registry
            .lock()
            .unwrap()
            .get(&goal_id)
            .map(|s| s.result.clone());
        let Some(rendezvous) = rendezvous else {
            // The goal finished (and its context dropped) or never existed.
            let _ = responder
                .respond_error("no active goal for goal_id".to_string())
                .await;
            continue;
        };

        // Either respond now (value already available) or park the responder.
        // The guard is dropped before any `.await` on the reply.
        let ready = {
            let mut guard = rendezvous.lock().await;
            match &mut *guard {
                ResultRendezvous::ValueReady(value) => Some((responder, value.clone())),
                slot => {
                    // Empty → park; a superseded parked responder is dropped
                    // (its reply stream closes cleanly).
                    *slot = ResultRendezvous::ResponderWaiting(responder);
                    None
                }
            }
        };
        if let Some((responder, value)) = ready {
            // Deliver-once: drop the slot so the goal_id can't be matched
            // again. This is also what lets a completed goal's context be
            // dropped immediately after `complete` without losing the result.
            registry.lock().unwrap().remove(&goal_id);
            let _ = responder.respond(value).await;
        }
    }
}

/// A concurrent action server. Built from an [`ActionCreation`] by
/// [`Self::expose`]; spawns the cancel/result routing loops and hands out a
/// [`GoalContext`] per accepted goal so many goals can run at once.
///
/// This is the single shared engine: the Rust codegen and the peppylib-py
/// binding both drive it through [`Self::recv_next_goal`] →
/// [`PendingGoal::accept`]/[`PendingGoal::reject`], so server behavior is
/// identical across languages.
pub struct ConcurrentAction {
    goal_service: ServiceEndpoint,
    factory: ActionFeedbackPublisherFactory,
    registry: GoalRegistry,
    has_feedback: bool,
    result_retention_grace: Duration,
    stop: CancellationToken,
    cancel_loop: TaskHandle<()>,
    result_loop: TaskHandle<()>,
}

impl ConcurrentAction {
    /// Expose an action server and start its concurrent engine. `has_feedback`
    /// must reflect whether the action declares a feedback topic; when `false`
    /// the per-goal feedback publisher is not declared. The other arguments
    /// mirror [`ActionMessenger::expose`].
    pub async fn expose(
        messenger: &MessengerHandle,
        bound_core_node: &str,
        as_instance_id: &str,
        as_identity: SenderTarget,
        as_action_name: &str,
        has_feedback: bool,
    ) -> Result<Self> {
        let creation = ActionMessenger::expose(
            messenger,
            bound_core_node,
            as_instance_id,
            as_identity,
            as_action_name,
        )
        .await?;
        Ok(Self::start(creation, has_feedback))
    }

    /// Build the engine from an already-exposed [`ActionCreation`], moving the
    /// cancel/result services into background routing loops.
    pub fn start(creation: ActionCreation, has_feedback: bool) -> Self {
        let ActionCreation {
            goal_service,
            cancel_service,
            feedback_publisher_factory,
            result_service,
        } = creation;
        let registry: GoalRegistry = Arc::new(StdMutex::new(HashMap::new()));
        let stop = CancellationToken::new();
        let cancel_loop = spawn(run_cancel_loop(
            cancel_service,
            Arc::clone(&registry),
            stop.clone(),
        ));
        let result_loop = spawn(run_result_loop(
            result_service,
            Arc::clone(&registry),
            stop.clone(),
        ));
        Self {
            goal_service,
            factory: feedback_publisher_factory,
            registry,
            has_feedback,
            result_retention_grace: RESULT_RETENTION_GRACE,
            stop,
            cancel_loop,
            result_loop,
        }
    }

    /// Override how long a completed-but-unfetched result stays routable after
    /// its [`GoalContext`] drops (default [`RESULT_RETENTION_GRACE`]). Exposed
    /// mainly so tests can exercise eviction without waiting the full window.
    pub fn with_result_retention_grace(mut self, grace: Duration) -> Self {
        self.result_retention_grace = grace;
        self
    }

    /// Wait for the next goal request. Returns a [`PendingGoal`] the caller
    /// inspects and then [`accept`](PendingGoal::accept)s or
    /// [`reject`](PendingGoal::reject)s. Returns `Ok(None)` when the goal
    /// service stream has closed.
    pub async fn recv_next_goal(&mut self) -> Result<Option<PendingGoal>> {
        let Some((context, responder)) = self.goal_service.recv_next_request().await? else {
            return Ok(None);
        };
        let link_id = context.link_id().to_string();
        let core_node = context.message().core_node().to_string();
        let instance_id = context.message().instance_id().to_string();
        let wire = context.message().payload().into_inner();

        let (goal_id, request_bytes, feedback) = if self.has_feedback {
            // Declares the per-goal feedback publisher and strips the envelope.
            let declared = self.factory.declare_from_wire(&link_id, wire).await?;
            (
                declared.goal_id,
                declared.user_payload,
                Some(declared.publisher),
            )
        } else {
            // No feedback topic: just extract the goal_id and user payload.
            let (goal_id, request_bytes) = {
                let (goal_id, body) = unwrap_goal_payload(wire.as_ref())?;
                let offset = wire.len() - body.len();
                (goal_id.to_string(), wire.slice(offset..))
            };
            (goal_id, request_bytes, None)
        };

        Ok(Some(PendingGoal {
            goal_id,
            core_node,
            instance_id,
            request_bytes,
            responder,
            feedback,
            registry: Arc::clone(&self.registry),
            result_retention_grace: self.result_retention_grace,
        }))
    }
}

impl Drop for ConcurrentAction {
    fn drop(&mut self) {
        self.stop.cancel();
        self.cancel_loop.abort();
        self.result_loop.abort();
    }
}

/// A goal that has been received but not yet accepted or rejected. The caller
/// decodes [`request_bytes`](Self::request_bytes), decides (this is where
/// per-resource concurrency limits are enforced), and calls
/// [`accept`](Self::accept) or [`reject`](Self::reject) with the encoded
/// `GoalResponse` payload.
pub struct PendingGoal {
    goal_id: String,
    core_node: String,
    instance_id: String,
    request_bytes: Bytes,
    result_retention_grace: Duration,
    responder: ServiceResponder,
    feedback: Option<ActionFeedbackPublisher>,
    registry: GoalRegistry,
}

impl PendingGoal {
    /// The client-generated correlation id for this goal.
    pub fn goal_id(&self) -> &str {
        &self.goal_id
    }

    /// The core node of the client that sent this goal.
    pub fn core_node(&self) -> &str {
        &self.core_node
    }

    /// The instance id of the client that sent this goal.
    pub fn instance_id(&self) -> &str {
        &self.instance_id
    }

    /// The envelope-stripped goal request payload, ready to decode.
    pub fn request_bytes(&self) -> &[u8] {
        self.request_bytes.as_ref()
    }

    /// Accept the goal: register it for cancel/result routing, reply to the
    /// client with `response`, and hand back the [`GoalContext`] that drives it
    /// to completion. The slot is registered **before** the reply is sent so a
    /// cancel/result request the client fires immediately after `fire_goal`
    /// returns always finds the goal.
    pub async fn accept(self, response: Payload) -> Result<GoalContext> {
        let cancel = CancellationToken::new();
        let result = Arc::new(TokioMutex::new(ResultRendezvous::Empty));
        self.registry.lock().unwrap().insert(
            self.goal_id.clone(),
            GoalSlot {
                cancel: cancel.clone(),
                result: Arc::clone(&result),
            },
        );

        if let Err(err) = self.responder.respond(response).await {
            // Reply failed (client gone): don't leak the just-registered slot.
            self.registry.lock().unwrap().remove(&self.goal_id);
            return Err(err);
        }

        Ok(GoalContext {
            goal_id: self.goal_id,
            request_bytes: self.request_bytes,
            cancel,
            result,
            feedback: self.feedback,
            registry: self.registry,
            completed: AtomicBool::new(false),
            result_retention_grace: self.result_retention_grace,
            // `accept` is always awaited on the runtime, so a handle is
            // available here; `Drop` reuses it to spawn cleanup from any thread.
            runtime: tokio::runtime::Handle::current(),
        })
    }

    /// Reject the goal: reply with `response` and register nothing. No
    /// [`GoalContext`] is produced, so the goal cannot be cancelled or
    /// completed.
    pub async fn reject(self, response: Payload) -> Result<()> {
        self.responder.respond(response).await
    }
}

/// The per-goal handle owned by user code for the life of an accepted goal.
/// Carries the decoded request bytes, the per-goal feedback publisher, the
/// cancel signal, and the result-delivery channel. Cheaply movable into a
/// spawned task.
pub struct GoalContext {
    goal_id: String,
    request_bytes: Bytes,
    cancel: CancellationToken,
    result: Arc<TokioMutex<ResultRendezvous>>,
    feedback: Option<ActionFeedbackPublisher>,
    registry: GoalRegistry,
    completed: AtomicBool,
    /// How long to keep a completed-but-unfetched result routable after this
    /// context drops; propagated from the [`ConcurrentAction`].
    result_retention_grace: Duration,
    /// Runtime handle captured at accept time so `Drop` can schedule its async
    /// cleanup (closing the feedback stream, grace-evicting the slot) even when
    /// the context is dropped off the runtime. This is the case in the
    /// peppylib-py binding, where Python's GC drops the wrapping object on the
    /// interpreter thread, so capturing the handle keeps cleanup identical
    /// across Rust and Python rather than silently degrading off-runtime.
    runtime: tokio::runtime::Handle,
}

impl GoalContext {
    /// The client-generated correlation id for this goal.
    pub fn goal_id(&self) -> &str {
        &self.goal_id
    }

    /// The envelope-stripped goal request payload.
    pub fn request_bytes(&self) -> &[u8] {
        self.request_bytes.as_ref()
    }

    /// Publish a feedback message on this goal's stream. Errors if the action
    /// has no feedback topic.
    pub async fn publish_feedback(&self, payload: NonEmptyPayload) -> Result<()> {
        match &self.feedback {
            Some(publisher) => publisher.publish(payload).await,
            None => Err(Error::Io(std::io::Error::other(
                "publish_feedback called on an action with no feedback topic",
            ))),
        }
    }

    /// Resolves when a cancel request arrives for this goal. Pair it with the
    /// goal's work in a `select!` and react by calling
    /// [`complete_cancelled`](Self::complete_cancelled).
    pub async fn cancel_signal(&self) {
        self.cancel.cancelled().await;
    }

    /// Whether a cancel has been requested for this goal.
    pub fn is_cancelled(&self) -> bool {
        self.cancel.is_cancelled()
    }

    /// Deliver the final result for this goal. Idempotent: the first call wins,
    /// later calls are no-ops.
    pub async fn complete(&self, result: Payload) -> Result<()> {
        self.deliver(result).await
    }

    /// Deliver the final result after observing a cancel. Functionally
    /// identical to [`complete`](Self::complete); the distinct name documents
    /// intent at the call site.
    pub async fn complete_cancelled(&self, result: Payload) -> Result<()> {
        self.deliver(result).await
    }

    async fn deliver(&self, result: Payload) -> Result<()> {
        if self.completed.swap(true, Ordering::SeqCst) {
            return Ok(());
        }

        // Store the value so a result request that has not arrived yet finds
        // it; if one is already parked, answer it now and drop the slot.
        let parked = {
            let mut guard = self.result.lock().await;
            match std::mem::replace(&mut *guard, ResultRendezvous::ValueReady(result.clone())) {
                ResultRendezvous::ResponderWaiting(responder) => Some(responder),
                _ => None,
            }
        };
        if let Some(responder) = parked {
            // Delivered to a waiting request → the slot is done.
            self.registry.lock().unwrap().remove(&self.goal_id);
            let _ = responder.respond(result).await;
        }

        // Close this goal's feedback stream so the client's drain loop ends.
        if let Some(publisher) = &self.feedback {
            let _ = publisher.publish_end().await;
        }
        Ok(())
    }
}

/// How long a completed-but-unfetched result stays routable after its
/// [`GoalContext`] is dropped. This lets a worker `complete` and drop the
/// context immediately (e.g. at the end of a spawned task) while a slightly
/// late client `get_result` can still retrieve the result, without letting an
/// unfetched slot linger for the life of the server.
const RESULT_RETENTION_GRACE: Duration = Duration::from_secs(30);

impl Drop for GoalContext {
    fn drop(&mut self) {
        if !self.completed.load(Ordering::SeqCst) {
            // The goal was abandoned without a result (early return, a panic in
            // the worker, or the context simply dropped). Evict the slot so
            // later cancel/result requests get a definitive answer, and close
            // this goal's feedback stream so a client draining feedback breaks
            // out of its loop instead of hanging forever. Any result request
            // currently parked in the slot is dropped with it, cleanly closing
            // that poll. `publish_end` is async, so fire-and-forget on the
            // captured runtime handle (works even when dropped off-runtime).
            self.registry.lock().unwrap().remove(&self.goal_id);
            if let Some(publisher) = &self.feedback {
                let publisher = publisher.clone();
                self.runtime.spawn(async move {
                    let _ = publisher.publish_end().await;
                });
            }
            return;
        }

        // The goal completed. `deliver` removes the slot as soon as a result
        // request is answered (deliver-once), so if the slot is already gone a
        // poll has fetched the result and there is nothing to clean up. If the
        // slot is still present the result is buffered but unfetched: a worker
        // may `complete` and drop the context in the same breath before the
        // client calls `get_result`, so keep the value routable for a grace
        // window, then evict it. Without this, a completed goal whose result is
        // never fetched would leak its slot for the life of the server.
        let still_buffered = self.registry.lock().unwrap().contains_key(&self.goal_id);
        if still_buffered {
            let registry = Arc::clone(&self.registry);
            let goal_id = self.goal_id.clone();
            let grace = self.result_retention_grace;
            self.runtime.spawn(async move {
                tokio::time::sleep(grace).await;
                registry.lock().unwrap().remove(&goal_id);
            });
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
