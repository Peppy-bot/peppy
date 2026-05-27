use super::discovery::discover_producer;
use super::generate_short_id;
use super::topics::Subscription;
use super::{
    MessengerHandle, PROBE_TIMEOUT, ServiceEndpoint, ServiceRequestContext, ServiceResponder,
    TopicPublisher,
};
use crate::error::{Error, Result};
use crate::runtime::{CancellationToken, TaskHandle, spawn};
use crate::types::{Message, Payload};
use bytes::{BufMut, Bytes, BytesMut};
use config::node::QoSProfile;
use pmi::{ActionWireReceiver, ActionWireSender, PublisherQoS, SenderTarget, ServiceQueryKind};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Instant;
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

const CANCEL_ACK_ENCODING: &str = "action_cancel_ack";

/// Encode the SDK-owned cancel acknowledgement. The cancel service no longer
/// runs a user handler: the SDK fires the goal's cancel signal and replies
/// with this one-byte ack. `true` means the goal was in flight (its signal
/// fired); `false` means there was no such goal (unknown or already finished).
/// This is the single source of truth for the cancel response wire shape;
/// generated clients decode it via [`decode_cancel_ack`].
pub fn encode_cancel_ack(accepted: bool) -> Payload {
    Payload::from(Bytes::from_static(if accepted { &[1u8] } else { &[0u8] }))
}

/// Decode a cancel acknowledgement produced by [`encode_cancel_ack`]. Rejects
/// any payload that is not exactly the single accepted/rejected byte rather
/// than silently coercing it (parse, don't validate).
pub fn decode_cancel_ack(payload: &[u8]) -> Result<bool> {
    match payload {
        [0] => Ok(false),
        [1] => Ok(true),
        _ => Err(Error::InternalEncodingError {
            identifier: CANCEL_ACK_ENCODING.to_string(),
            reason: format!("expected a single 0/1 byte, got {} bytes", payload.len()),
        }),
    }
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

// ---------------------------------------------------------------------------
// Concurrent goals: per-goal registry, rendezvous, and `GoalContext`
// ---------------------------------------------------------------------------

/// Result-rendezvous state for one in-flight goal. The worker delivers the
/// result via [`GoalContext::complete`] while the client requests it via the
/// result service; the two can arrive in either order, so the slot buffers
/// whichever comes first.
enum ResultSlot {
    /// Neither side has arrived yet.
    Empty,
    /// `complete` ran first: the result waits here for the next result request.
    Buffered(Payload),
    /// A result request arrived first: its responder is parked here until
    /// `complete` delivers.
    Waiting(ServiceResponder),
    /// The result has been handed to at least one requester. Retained (rather
    /// than dropped) so a client retry or a late duplicate request still gets
    /// an answer for as long as the [`GoalContext`] is alive.
    Delivered(Payload),
}

/// Per-goal server-side state, shared between the [`GoalContext`] the worker
/// owns and the cancel/result pumps that route requests by `goal_id`.
struct GoalSlot {
    /// Fired by the cancel pump when a cancel request names this goal.
    /// `GoalContext::cancel_signal` awaits it; idempotent and resolves
    /// immediately if the cancel already arrived.
    cancel: CancellationToken,
    result: StdMutex<ResultSlot>,
}

impl GoalSlot {
    fn new() -> Self {
        Self {
            cancel: CancellationToken::new(),
            result: StdMutex::new(ResultSlot::Empty),
        }
    }
}

/// `goal_id -> GoalSlot` map shared by the cancel pump, the result pump, and
/// every live [`GoalContext`]. Cloning shares the same underlying map.
#[derive(Clone, Default)]
struct GoalRegistry {
    inner: Arc<StdMutex<HashMap<String, Arc<GoalSlot>>>>,
}

impl GoalRegistry {
    fn insert(&self, goal_id: String, slot: Arc<GoalSlot>) {
        self.inner.lock().unwrap().insert(goal_id, slot);
    }

    fn get(&self, goal_id: &str) -> Option<Arc<GoalSlot>> {
        self.inner.lock().unwrap().get(goal_id).cloned()
    }

    fn remove(&self, goal_id: &str) {
        self.inner.lock().unwrap().remove(goal_id);
    }

    /// Fire the cancel signal for `goal_id`. Returns whether the goal was
    /// in flight (so the cancel pump can ack accepted/not-accepted).
    fn signal_cancel(&self, goal_id: &str) -> bool {
        match self.inner.lock().unwrap().get(goal_id) {
            Some(slot) => {
                slot.cancel.cancel();
                true
            }
            None => false,
        }
    }
}

/// Deliver a buffered result to `responder`, or park `responder` until
/// [`GoalContext::complete`] delivers. The decision is made under the slot
/// lock; the (async) reply happens after the lock is released.
async fn rendezvous_result(slot: &GoalSlot, responder: ServiceResponder) {
    enum Outcome {
        Deliver(Payload, ServiceResponder),
        Parked,
        Busy(ServiceResponder),
    }

    let outcome = {
        let mut guard = slot.result.lock().unwrap();
        match std::mem::replace(&mut *guard, ResultSlot::Empty) {
            ResultSlot::Buffered(payload) => {
                *guard = ResultSlot::Delivered(payload.clone());
                Outcome::Deliver(payload, responder)
            }
            ResultSlot::Delivered(payload) => {
                let reply = payload.clone();
                *guard = ResultSlot::Delivered(payload);
                Outcome::Deliver(reply, responder)
            }
            ResultSlot::Empty => {
                *guard = ResultSlot::Waiting(responder);
                Outcome::Parked
            }
            ResultSlot::Waiting(previous) => {
                *guard = ResultSlot::Waiting(previous);
                Outcome::Busy(responder)
            }
        }
    };

    match outcome {
        Outcome::Deliver(payload, responder) => {
            let _ = responder.respond(payload).await;
        }
        Outcome::Parked => {}
        Outcome::Busy(responder) => {
            let _ = responder
                .respond_error("a result request is already pending for this goal".to_string())
                .await;
        }
    }
}

/// Drains cancel requests for one action server, routing each to the named
/// goal's cancel signal and replying with the SDK cancel ack. Ends when the
/// endpoint closes.
async fn run_cancel_pump(mut endpoint: ServiceEndpoint, registry: GoalRegistry) {
    loop {
        match endpoint.recv_next_request().await {
            Ok(Some((context, responder))) => {
                let accepted = match unwrap_goal_payload(context.message().payload().as_ref()) {
                    Ok((goal_id, _)) => registry.signal_cancel(goal_id),
                    Err(_) => false,
                };
                let _ = responder.respond(encode_cancel_ack(accepted)).await;
            }
            Ok(None) => break,
            Err(err) => {
                warn!(%err, "action cancel pump stopping after error");
                break;
            }
        }
    }
}

/// Drains result requests for one action server, routing each to the named
/// goal's result rendezvous. Ends when the endpoint closes.
async fn run_result_pump(mut endpoint: ServiceEndpoint, registry: GoalRegistry) {
    loop {
        match endpoint.recv_next_request().await {
            Ok(Some((context, responder))) => {
                match unwrap_goal_payload(context.message().payload().as_ref()) {
                    Ok((goal_id, _)) => match registry.get(goal_id) {
                        Some(slot) => rendezvous_result(&slot, responder).await,
                        None => {
                            let _ = responder
                                .respond_error(format!("no in-flight goal `{goal_id}`"))
                                .await;
                        }
                    },
                    Err(err) => {
                        let _ = responder
                            .respond_error(format!("malformed result request: {err}"))
                            .await;
                    }
                }
            }
            Ok(None) => break,
            Err(err) => {
                warn!(%err, "action result pump stopping after error");
                break;
            }
        }
    }
}

/// Per-goal handle handed to user code for a single accepted goal. Owns that
/// goal's feedback publisher, cancel signal, and result delivery; it is the
/// only handle the worker needs to drive the goal to completion. `Send` +
/// `'static`, so it moves freely into a spawned worker task (see the
/// compile-time assertion in the tests).
pub struct GoalContext {
    goal_id: String,
    request: Bytes,
    core_node: String,
    instance_id: String,
    link_id: String,
    feedback: ActionFeedbackPublisher,
    slot: Arc<GoalSlot>,
    registry: GoalRegistry,
    finalized: AtomicBool,
}

impl GoalContext {
    /// The `goal_id` this context drives, as generated by the client and
    /// carried in the goal envelope.
    pub fn goal_id(&self) -> &str {
        &self.goal_id
    }

    /// Envelope-stripped goal request bytes, ready for the typed decoder.
    pub fn request_bytes(&self) -> &[u8] {
        &self.request
    }

    /// Caller core node that fired this goal.
    pub fn core_node(&self) -> &str {
        &self.core_node
    }

    /// Caller instance id that fired this goal.
    pub fn instance_id(&self) -> &str {
        &self.instance_id
    }

    /// Producer-side link_id this goal was received on.
    pub fn link_id(&self) -> &str {
        &self.link_id
    }

    /// Resolves when a cancel request for this goal has been received.
    /// Idempotent and resolves immediately if the cancel already arrived, so
    /// it is safe to await inside a `tokio::select!`.
    pub async fn cancel_signal(&self) {
        self.slot.cancel.cancelled().await;
    }

    /// Whether a cancel for this goal has already been received.
    pub fn is_cancelled(&self) -> bool {
        self.slot.cancel.is_cancelled()
    }

    /// Publish one feedback message on this goal's stream. Empty payloads are
    /// rejected because the empty payload is the end-of-stream sentinel that
    /// [`Self::complete`] emits.
    pub async fn publish_feedback(&self, payload: NonEmptyPayload) -> Result<()> {
        self.feedback.publish(payload).await
    }

    /// A clone of this goal's feedback publisher, for handing to a feedback
    /// forwarder task that runs alongside the worker. The publisher is scoped
    /// to this goal, so forwarded messages only reach this goal's stream.
    pub fn feedback_publisher(&self) -> ActionFeedbackPublisher {
        self.feedback.clone()
    }

    /// Deliver the final result for this goal. Closes the feedback stream
    /// first (so a client draining feedback breaks out before reading the
    /// result), then rendezvous with the client's result request by
    /// `goal_id`. Idempotent: a second call is a no-op. The result is
    /// retained for retries until this context is dropped.
    pub async fn complete(&self, result: Payload) -> Result<()> {
        if self.finalized.swap(true, Ordering::SeqCst) {
            return Ok(());
        }
        let _ = self.feedback.publish_end().await;

        let parked = {
            let mut guard = self.slot.result.lock().unwrap();
            match std::mem::replace(&mut *guard, ResultSlot::Empty) {
                ResultSlot::Waiting(responder) => {
                    *guard = ResultSlot::Delivered(result.clone());
                    Some(responder)
                }
                ResultSlot::Empty => {
                    *guard = ResultSlot::Buffered(result.clone());
                    None
                }
                // A result is already buffered or delivered for this goal:
                // keep it (the `finalized` guard above normally prevents this).
                existing => {
                    *guard = existing;
                    None
                }
            }
        };
        if let Some(responder) = parked {
            let _ = responder.respond(result).await;
        }
        Ok(())
    }
}

impl Drop for GoalContext {
    fn drop(&mut self) {
        // Evict the slot so any later cancel/result request gets a definitive
        // "no such goal" answer instead of routing to a finished goal.
        self.registry.remove(&self.goal_id);

        // If the worker abandoned the goal without completing it, close the
        // feedback stream so a draining client doesn't hang. Drop is sync and
        // `publish_end` is async, so fire-and-forget on the runtime (only when
        // one is available, to avoid panicking during runtime teardown).
        if !self.finalized.swap(true, Ordering::SeqCst)
            && tokio::runtime::Handle::try_current().is_ok()
        {
            let publisher = self.feedback.clone();
            spawn(async move {
                let _ = publisher.publish_end().await;
            });
        }
    }
}

/// Server-side handle for an action that supports multiple concurrent goals.
/// Returned by [`ActionMessenger::expose`]. Background pumps (spawned here)
/// own the cancel and result services and route each request to the right
/// [`GoalContext`] by `goal_id`; the caller drives the goal-accept loop via
/// [`Self::recv_next_goal`] + [`Self::register_goal`].
pub struct ActionServer {
    goal_service: ServiceEndpoint,
    feedback_factory: ActionFeedbackPublisherFactory,
    registry: GoalRegistry,
    cancel_pump: TaskHandle<()>,
    result_pump: TaskHandle<()>,
}

impl ActionServer {
    fn from_creation(creation: ActionCreation) -> Self {
        let registry = GoalRegistry::default();
        let cancel_pump = spawn(run_cancel_pump(creation.cancel_service, registry.clone()));
        let result_pump = spawn(run_result_pump(creation.result_service, registry.clone()));
        Self {
            goal_service: creation.goal_service,
            feedback_factory: creation.feedback_publisher_factory,
            registry,
            cancel_pump,
            result_pump,
        }
    }

    /// Wait for the next goal request. Returns the request context plus a
    /// [`ServiceResponder`] so the caller can run its own accept/reject
    /// decision (typed goal response, admission control) before registering.
    /// Returns `Ok(None)` when the goal service has closed.
    pub async fn recv_next_goal(
        &mut self,
    ) -> Result<Option<(ServiceRequestContext, ServiceResponder)>> {
        self.goal_service.recv_next_request().await
    }

    /// Register an accepted goal: declare its per-goal feedback publisher,
    /// insert its routing slot, and return the [`GoalContext`] the worker
    /// drives. Insert the slot *before* responding "accepted" to the client
    /// (the client only sends cancel/result after it sees acceptance), so a
    /// fast cancel/result can never miss the slot.
    pub async fn register_goal(&self, context: &ServiceRequestContext) -> Result<GoalContext> {
        let link_id = context.link_id().to_string();
        let wire = context.message().payload().into_inner();
        let declared = self
            .feedback_factory
            .declare_from_wire(&link_id, wire)
            .await?;
        let slot = Arc::new(GoalSlot::new());
        self.registry
            .insert(declared.goal_id.clone(), Arc::clone(&slot));
        Ok(GoalContext {
            goal_id: declared.goal_id,
            request: declared.user_payload,
            core_node: context.message().core_node().to_string(),
            instance_id: context.message().instance_id().to_string(),
            link_id,
            feedback: declared.publisher,
            slot,
            registry: self.registry.clone(),
            finalized: AtomicBool::new(false),
        })
    }
}

impl Drop for ActionServer {
    fn drop(&mut self) {
        self.cancel_pump.abort();
        self.result_pump.abort();
    }
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
    ) -> Result<ActionServer> {
        let creation = Self::expose_creation(
            messenger,
            bound_core_node,
            as_instance_id,
            as_identity,
            as_action_name,
        )
        .await?;
        Ok(ActionServer::from_creation(creation))
    }

    /// Low-level expose that yields the raw [`ActionCreation`] (the three
    /// service endpoints plus the feedback factory) instead of an
    /// [`ActionServer`]. [`Self::expose`] builds the concurrent goal server on
    /// top of this; it is also the entry point for callers (the Python
    /// bindings, messaging-layer tests) that drive the goal/cancel/result
    /// services manually.
    pub async fn expose_creation(
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
    /// the `goal_id` directly. External wrappers (e.g. Python bindings) hold a
    /// clone so they can cancel without locking the goal handle during the
    /// network round-trip. The `goal_id` is wrapped into the request envelope
    /// so the server routes the cancel to the right in-flight goal.
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
    /// and the `goal_id` directly. Mirrors
    /// [`cancel_with_sender`](Self::cancel_with_sender).
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

    #[test]
    fn cancel_ack_roundtrips() {
        assert!(decode_cancel_ack(encode_cancel_ack(true).as_ref()).expect("decode true"));
        assert!(!decode_cancel_ack(encode_cancel_ack(false).as_ref()).expect("decode false"));
    }

    #[test]
    fn cancel_ack_rejects_malformed() {
        assert!(decode_cancel_ack(&[]).is_err());
        assert!(decode_cancel_ack(&[2]).is_err());
        assert!(decode_cancel_ack(&[0, 1]).is_err());
    }

    #[test]
    fn goal_context_and_server_are_send_static() {
        fn assert_send_static<T: Send + 'static>() {}
        // The worker moves a `GoalContext` into a spawned task, and the
        // `ActionServer` is held across awaits; both must be `Send + 'static`.
        assert_send_static::<GoalContext>();
        assert_send_static::<ActionServer>();
    }
}
