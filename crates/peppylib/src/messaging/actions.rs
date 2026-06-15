use super::discovery::discover_producer;
use super::generate_short_id;
use super::topics::Subscription;
use super::{
    DISCOVERY_TIMEOUT, MessengerHandle, ServiceEndpoint, ServiceResponder, TopicPublisher,
};
use crate::error::{Error, Result};
use crate::messaging::ProducerRef;
use crate::runtime::{CancellationToken, TaskHandle, spawn};
use crate::types::{Message, Payload};
use bytes::{BufMut, Bytes, BytesMut};
use config::node::QoSProfile;
use pmi::{
    ActionLivelinessEvent, ActionLivelinessToken, ActionLivelinessWatch, ActionWireReceiver,
    ActionWireSender, PublisherQoS, SenderTarget, ServiceQueryKind,
};
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::{Mutex as TokioMutex, Notify};
// tokio's clock so the retention/eviction timeouts run on virtual time under
// `#[tokio::test(start_paused = true)]`; in a normal runtime it reads the real
// monotonic clock, so production behavior is unchanged.
use tokio::time::{Duration, Instant};
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

/// Split a goal-envelope `wire` into its owned `goal_id` and a zero-copy `Bytes`
/// slice of the user payload. The slice reuses `wire`'s buffer: the
/// user-payload offset is derived from the suffix length that
/// [`unwrap_goal_payload`] returns (a sub-slice of the same buffer), so no
/// payload copy happens. Shared by both goal-receive paths. The feedback path
/// (`declare_from_wire`) additionally validates the goal_id with
/// [`is_safe_goal_id`] before splicing it into a topic keyexpr; the no-feedback
/// path does not, because there the goal_id is only a registry key / request
/// payload field and never reaches a keyexpr.
fn split_goal_envelope(wire: &Bytes) -> Result<(String, Bytes)> {
    let (goal_id, user_payload) = unwrap_goal_payload(wire.as_ref())?;
    let offset = wire.len() - user_payload.len();
    Ok((goal_id.to_string(), wire.slice(offset..)))
}

const RESULT_OUTCOME_ENVELOPE: &str = "action_result_outcome_envelope";

/// The terminal status of a goal, carried as a 1-byte tag prefixing the result
/// reply. Mirrors the engine-level framing of [`wrap_goal_payload`] (no capnp):
/// the body that follows is the worker's raw result payload for
/// [`Completed`](ResultStatus::Completed) / [`Cancelled`](ResultStatus::Cancelled)
/// and empty for [`Abandoned`](ResultStatus::Abandoned) / [`Expired`](ResultStatus::Expired).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum ResultStatus {
    /// The worker delivered a result via `complete`.
    Completed = 0,
    /// The worker delivered a result via `complete_cancelled` after a cancel.
    Cancelled = 1,
    /// The worker abandoned the goal: its context dropped without delivering a
    /// result (early return, panic, or simply dropped).
    Abandoned = 2,
    /// The goal reached a terminal state, but its result was retained only for a
    /// bounded window that has since elapsed (or the result was already evicted).
    Expired = 3,
}

impl ResultStatus {
    fn from_tag(tag: u8) -> Option<Self> {
        match tag {
            0 => Some(Self::Completed),
            1 => Some(Self::Cancelled),
            2 => Some(Self::Abandoned),
            3 => Some(Self::Expired),
            _ => None,
        }
    }
}

/// Frame a result reply as `[status:u8][body]`. `body` is the worker's raw
/// result payload, empty for [`ResultStatus::Abandoned`] / [`ResultStatus::Expired`].
/// Symmetric counterpart of [`unwrap_result_outcome`].
pub fn wrap_result_outcome(status: ResultStatus, body: &[u8]) -> Payload {
    let mut buf = BytesMut::with_capacity(1 + body.len());
    buf.put_u8(status as u8);
    buf.extend_from_slice(body);
    Payload::from(buf.freeze())
}

/// Decode a result-outcome envelope produced by [`wrap_result_outcome`] into its
/// [`ResultStatus`] and body bytes. Errors on an empty wire or an unknown tag.
pub(crate) fn unwrap_result_outcome(wire: &[u8]) -> Result<(ResultStatus, &[u8])> {
    let tag = *wire.first().ok_or_else(|| Error::InternalEncodingError {
        identifier: RESULT_OUTCOME_ENVELOPE.to_string(),
        reason: "wire payload is empty".to_string(),
    })?;
    let status = ResultStatus::from_tag(tag).ok_or_else(|| Error::InternalEncodingError {
        identifier: RESULT_OUTCOME_ENVELOPE.to_string(),
        reason: format!("unknown result status tag {tag}"),
    })?;
    Ok((status, &wire[1..]))
}

/// The typed reply from [`ActionMessenger::request_result`]. The engine frames
/// every result reply as a [`ResultStatus`] tag plus the worker's raw result
/// body; this is the stripped, decoded form. `body` is the raw user result
/// payload for [`ResultStatus::Completed`] / [`ResultStatus::Cancelled`] and
/// empty for [`ResultStatus::Abandoned`] / [`ResultStatus::Expired`]. Stripping
/// here (in the messenger, mirroring how `send_goal` owns the goal envelope)
/// keeps the framing out of generated code, so Rust and Python decode identically.
pub struct ActionResultReply {
    pub status: ResultStatus,
    pub body: Payload,
    pub instance_id: String,
    pub core_node: String,
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
    message.payload_bytes().is_empty()
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
        let (goal_id, user_payload) = split_goal_envelope(&wire)?;
        // The goal_id is appended to the feedback topic to scope the publisher
        // per goal cycle. Reject anything that could let a malicious or
        // malformed envelope escape that scope (extra segments, Zenoh
        // wildcards, ...) so the publisher cannot be steered onto a topic the
        // server didn't intend.
        if !is_safe_goal_id(&goal_id) {
            return Err(envelope_error(format!(
                "goal_id contains unsafe characters: {goal_id:?}"
            )));
        }
        let publisher = self.declare(link_id, &goal_id).await?;
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

// ---------------------------------------------------------------------------
// Producer-disappearance detection
// ---------------------------------------------------------------------------
//
// The end-of-stream sentinel only reaches the consumer while the producer's
// session is alive to publish it. A producer that dies hard (SIGKILL, OOM,
// runtime teardown winning the race against `GoalContext::Drop`'s spawned
// cleanup) never sends it, and the consumer's feedback subscription — pinned
// to the dead `instance_id` — would otherwise wait forever. Every exposed
// action therefore advertises a transport liveliness token
// (see `MessengerHandle::expose_action`), and every goal handle watches the
// pinned producer's token so `on_next_feedback` can fail over to
// [`Error::ActionFeedbackProducerGone`] when the producer disappears.

/// Delay before each confirmation probe after a raw `Gone` liveliness event.
/// Gives a gracefully-closing producer's final sentinel time to arrive (so
/// the drain still ends with the typed `ActionFeedbackChannelClosed`) and
/// lets a router bounce re-announce a still-alive producer's token instead
/// of misreporting it dead.
const PRODUCER_GONE_CONFIRM_DELAY: Duration = Duration::from_millis(250);

/// Budget for one liveliness probe (`liveliness get`); the local routing
/// view answers well within this.
const LIVELINESS_PROBE_TIMEOUT: Duration = Duration::from_secs(1);

/// How long the watcher waits for the history-replayed `Alive` of a
/// producer that just answered the goal request, before suspecting it died
/// in the gap and probing directly.
const LIVELINESS_RESOLVE_TIMEOUT: Duration = Duration::from_secs(2);

/// Consecutive token-absent probes required before the producer is declared
/// gone. Two spaced probes ride out a transient routing flap without adding
/// meaningful latency to real-death detection.
const PRODUCER_GONE_CONFIRM_PROBES: usize = 2;

/// `true` once the producer is confirmed absent: every confirmation probe
/// found no token. Any probe that sees the token — or errors, e.g. while the
/// consumer's own session is closing — aborts the confirmation, so a flap
/// or an inconclusive probe never fabricates a producer death.
async fn confirm_producer_gone(messenger: &MessengerHandle, sender: &ActionWireSender) -> bool {
    for _ in 0..PRODUCER_GONE_CONFIRM_PROBES {
        tokio::time::sleep(PRODUCER_GONE_CONFIRM_DELAY).await;
        match messenger
            .probe_action_producer(sender, LIVELINESS_PROBE_TIMEOUT)
            .await
        {
            Ok(false) => continue,
            Ok(true) | Err(_) => return false,
        }
    }
    true
}

/// Background policy task behind [`ProducerGoneWatch`]: turns the raw
/// liveliness event stream into a single confirmed "producer gone" latch.
async fn run_producer_liveliness_watch(
    watch: ActionLivelinessWatch,
    messenger: MessengerHandle,
    sender: ActionWireSender,
    gone: Arc<AtomicBool>,
    notify: Arc<Notify>,
) {
    let declare_gone = || {
        gone.store(true, Ordering::Release);
        notify.notify_waiters();
    };

    // Initial resolve: the producer answered the goal request moments ago,
    // so its token is expected to replay as an immediate `Alive` (the watch
    // subscribes with history). Seeing nothing inside the window means the
    // producer died in the gap — confirm by probing rather than trusting
    // propagation timing.
    match tokio::time::timeout(LIVELINESS_RESOLVE_TIMEOUT, watch.rx.recv_async()).await {
        Ok(Ok(ActionLivelinessEvent::Alive)) => {}
        Ok(Ok(ActionLivelinessEvent::Gone)) | Err(_) => {
            if confirm_producer_gone(&messenger, &sender).await {
                return declare_gone();
            }
        }
        // Watch channel closed: the consumer's own session is going away;
        // the feedback channel reports that as `ActionFeedbackChannelClosed`.
        Ok(Err(_)) => return,
    }

    // Steady state: react to `Gone` transitions, tolerating `Alive`/`Gone`
    // flaps (a router bounce deletes and re-announces tokens).
    loop {
        match watch.rx.recv_async().await {
            Ok(ActionLivelinessEvent::Alive) => {}
            Ok(ActionLivelinessEvent::Gone) => {
                if confirm_producer_gone(&messenger, &sender).await {
                    return declare_gone();
                }
            }
            Err(_) => return,
        }
    }
}

/// Confirmed producer-death latch held by [`ActionGoalHandle`]. A spawned
/// watcher task owns the liveliness event stream and the confirmation
/// probes; the handle observes only the latched outcome, synchronously via
/// [`is_gone`](Self::is_gone) or awaited via [`gone`](Self::gone). Dropping
/// the latch aborts the watcher.
struct ProducerGoneWatch {
    gone: Arc<AtomicBool>,
    notify: Arc<Notify>,
    task: TaskHandle<()>,
}

impl ProducerGoneWatch {
    fn spawn(
        watch: ActionLivelinessWatch,
        messenger: MessengerHandle,
        sender: ActionWireSender,
    ) -> Self {
        let gone = Arc::new(AtomicBool::new(false));
        let notify = Arc::new(Notify::new());
        let task = spawn(run_producer_liveliness_watch(
            watch,
            messenger,
            sender,
            Arc::clone(&gone),
            Arc::clone(&notify),
        ));
        Self { gone, notify, task }
    }

    fn is_gone(&self) -> bool {
        self.gone.load(Ordering::Acquire)
    }

    /// Resolves once the producer is confirmed gone; pends forever while it
    /// stays alive. Cancel-safe: the latch lives in the watcher, not here.
    async fn gone(&self) {
        loop {
            if self.is_gone() {
                return;
            }
            let notified = self.notify.notified();
            tokio::pin!(notified);
            // Register interest before the re-check so a notify between the
            // check and the await cannot be missed.
            notified.as_mut().enable();
            if self.is_gone() {
                return;
            }
            notified.await;
        }
    }
}

impl Drop for ProducerGoneWatch {
    fn drop(&mut self) {
        self.task.abort();
    }
}

pub struct ActionGoalHandle {
    sender: ActionWireSender,
    goal_id: String,
    goal_response: Message,
    feedback: Subscription,
    producer_gone: ProducerGoneWatch,
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

    /// Whether the producer instance this goal is pinned to has been
    /// confirmed gone (its liveliness token disappeared and confirmation
    /// probes found no trace of it).
    pub fn is_producer_gone(&self) -> bool {
        self.producer_gone.is_gone()
    }

    /// Receives the next feedback message.
    ///
    /// Returns `Err(Error::ActionFeedbackChannelClosed)` when the server
    /// publishes the end-of-stream sentinel: the framework emits it when
    /// the server begins handling the result request, accepts a cancel,
    /// or the cancel handler errors.
    ///
    /// Returns `Err(Error::ActionFeedbackProducerGone)` when the producer
    /// instance this goal is pinned to disappears without publishing the
    /// sentinel (process killed, OOM, runtime teardown losing the cleanup
    /// race). Buffered feedback — including a sentinel that did make it
    /// out — is always drained before the producer-gone error surfaces, so
    /// a graceful close keeps reporting `ActionFeedbackChannelClosed`.
    pub async fn on_next_feedback(&mut self) -> Result<Message> {
        let msg = tokio::select! {
            biased;
            msg = self.feedback.on_next_message() => {
                msg.ok_or(Error::ActionFeedbackChannelClosed)?
            }
            _ = self.producer_gone.gone() => {
                return Err(producer_gone_error(&self.sender));
            }
        };
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
            Err(crate::types::TryRecvError::Empty) if self.producer_gone.is_gone() => {
                Err(producer_gone_error(&self.sender))
            }
            Err(crate::types::TryRecvError::Empty) => Ok(None),
            Err(crate::types::TryRecvError::Disconnected) => {
                Err(Error::ActionFeedbackChannelClosed)
            }
        }
    }
}

/// The typed error for a goal whose pinned producer instance disappeared.
fn producer_gone_error(sender: &ActionWireSender) -> Error {
    Error::ActionFeedbackProducerGone {
        instance_id: sender.target_instance_id().map(str::to_string),
        action_name: sender.to_action_name().to_string(),
    }
}

/// Locally synthesized result reply for a goal whose pinned producer
/// disappeared: the engine's retained outcome died with the process, which
/// is exactly the [`ResultStatus::Abandoned`] contract (terminal, empty
/// body).
fn abandoned_reply(sender: &ActionWireSender) -> ActionResultReply {
    ActionResultReply {
        status: ResultStatus::Abandoned,
        body: Payload::new(),
        instance_id: sender.target_instance_id().unwrap_or_default().to_string(),
        core_node: sender.target_core_node().unwrap_or_default().to_string(),
    }
}

// https://docs.ros.org/en/foxy/_images/Action-SingleActionClient.gif
pub struct ActionCreation {
    pub goal_service: ServiceEndpoint,
    pub cancel_service: ServiceEndpoint,
    pub feedback_publisher_factory: ActionFeedbackPublisherFactory,
    pub result_service: ServiceEndpoint,
    /// Liveliness advertisement for this producer instance. Must be held
    /// for the life of the action endpoint: when it drops (explicitly, or
    /// with the session on hard process death) consumers observe the
    /// producer as gone and fail their feedback drains over to
    /// [`Error::ActionFeedbackProducerGone`].
    pub liveliness_token: ActionLivelinessToken,
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

    /// Probe an action service (`target`: `Some` = a full
    /// `(core_node, instance_id)` pin, `None` = any matching producer).
    pub async fn is_reachable(
        messenger: &MessengerHandle,
        bound_core_node: &str,
        as_instance_id: &str,
        to_target: SenderTarget,
        to_action_name: &str,
        target: Option<&ProducerRef>,
    ) -> Result<bool> {
        let sender = ActionWireSender::new(
            bound_core_node,
            as_instance_id,
            target,
            to_target,
            to_action_name,
        )?;
        super::discovery::probe_reachable(messenger, &sender.goal_service()).await
    }

    /// Measure the round-trip latency of a single `Probe`-kind query to an
    /// action's **goal service** (actions are built on the goal/cancel/result
    /// services). As with [`Self::is_reachable`], the probe is auto-handled by
    /// the shared service request loop, so **no `PendingGoal` is created and the
    /// action engine never runs** — this measures only the messaging/routing
    /// path. Clock-independent (single-clock round-trip).
    ///
    /// `request_size`/`response_size` make the probe carry a real-payload-sized
    /// goal body and ask the producer to reply with `response_size` bytes (the
    /// goal request/response sizes), so the round-trip reflects real-sized
    /// messages — still without creating a goal or running the action.
    ///
    /// Returns `(elapsed, response_bytes_received)` on a clean reply; propagates
    /// the error otherwise.
    #[allow(clippy::too_many_arguments)]
    pub async fn probe_latency(
        messenger: &MessengerHandle,
        bound_core_node: &str,
        as_instance_id: &str,
        to_target: SenderTarget,
        to_action_name: &str,
        target: Option<&ProducerRef>,
        response_timeout: Duration,
        request_size: usize,
        response_size: u32,
    ) -> Result<(Duration, usize)> {
        let sender = ActionWireSender::new(
            bound_core_node,
            as_instance_id,
            target,
            to_target,
            to_action_name,
        )?;
        super::discovery::probe_round_trip(
            messenger,
            &sender.goal_service(),
            request_size,
            response_size,
            response_timeout,
        )
        .await
    }

    /// Send a goal to an action server. Generates a fresh `goal_id`,
    /// wraps `user_payload` in the per-goal envelope, subscribes to the
    /// matching feedback topic, and polls the goal service.
    ///
    /// `to_target` must match the [`SenderTarget`] the action server used
    /// in [`Self::expose`].
    ///
    /// `target` is the producer's full `(core_node, instance_id)` wire
    /// address. `Some(target)` — a pinned slot, or a `from_any` slot bound
    /// to exactly one producer — addresses that producer directly: **no
    /// discovery probe is issued and no discovery timeout applies**; the
    /// goal request has the caller's whole `goal_timeout` to itself.
    /// `None` is a genuine wildcard (`from_any`): a discover-then-pin
    /// sequence probes the goal sub-service to identify a single
    /// responding producer, then delivers the real goal pinned to it. The
    /// probe is answered by the transport adapter before the user handler
    /// runs, so non-winning producers never execute the goal handler.
    /// Without that, every matching producer would run the handler
    /// concurrently; for actions with side effects (motor commands, file
    /// writes) that is a real-world safety hazard.
    #[allow(clippy::too_many_arguments)]
    pub async fn send_goal(
        messenger: &MessengerHandle,
        as_core_node: &str,
        as_instance_id: &str,
        to_target: SenderTarget,
        to_action_name: &str,
        target: Option<&ProducerRef>,
        user_payload: Payload,
        feedback_qos: QoSProfile,
        goal_timeout: Duration,
    ) -> Result<ActionGoalHandle> {
        let goal_id = generate_goal_id();
        let goal_payload = wrap_goal_payload(&goal_id, user_payload.as_ref())?;

        // Discover a single producer only when the caller did not pin one.
        // The probe is answered by the transport adapter without invoking
        // the goal handler; only the discovered producer receives the real
        // goal request.
        let started_at = Instant::now();
        let resolved: ProducerRef = match target {
            Some(producer) => producer.clone(),
            None => {
                let probe_sender = ActionWireSender::new(
                    as_core_node,
                    as_instance_id,
                    None,
                    to_target.clone(),
                    to_action_name,
                )?;
                // Cap discovery at DISCOVERY_TIMEOUT or the caller's goal budget,
                // whichever is shorter: a tight `goal_timeout` still fails fast
                // against unreachable producers, while a generous one lets
                // peer-mode gossip discovery settle (see `discover_producer`).
                let discovery_timeout = goal_timeout.min(DISCOVERY_TIMEOUT);
                discover_producer(messenger, &probe_sender.goal_service(), discovery_timeout)
                    .await?
            }
        };

        let sender = ActionWireSender::new(
            as_core_node,
            as_instance_id,
            Some(&resolved),
            to_target,
            to_action_name,
        )?;

        // Feedback subscription is built from the pinned sender, so its
        // wire keyexpr targets only the discovered producer. Losers cannot
        // publish feedback under this goal_id to a slot we are listening on.
        let feedback_subscription = messenger
            .subscribe_action_feedback(&sender, &goal_id, feedback_qos.into())
            .await?;

        // Watch the pinned producer's liveliness for the life of this goal,
        // so a producer that dies without publishing the end-of-stream
        // sentinel surfaces as `ActionFeedbackProducerGone` instead of a
        // feedback drain that blocks forever.
        let liveliness_watch = messenger.watch_action_producer(&sender).await?;
        let producer_gone =
            ProducerGoneWatch::spawn(liveliness_watch, messenger.clone(), sender.clone());

        // Discovery counts against the caller's single end-to-end budget;
        // pass only the remaining slice to `poll_service` so a tight
        // `goal_timeout` can't be silently doubled by a slow probe.
        let remaining_goal_budget = goal_timeout.saturating_sub(started_at.elapsed());
        if remaining_goal_budget.is_zero() {
            return Err(Error::ServiceTimeout {
                instance_id: Some(resolved.instance_id.clone()),
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
            producer_gone,
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
    ) -> Result<ActionResultReply> {
        // Fast path: the goal handle's liveliness watcher already confirmed
        // the producer is gone. Its retained results died with the process,
        // so the goal resolves to a typed `Abandoned` immediately instead of
        // paying the result poll timeout against a dead queryable.
        if action_handle.is_producer_gone() {
            return Ok(abandoned_reply(&action_handle.sender));
        }
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
    ///
    /// Strips the engine's `[status:u8][body]` result-outcome envelope into a
    /// typed [`ActionResultReply`], so callers (Rust generated code, the Python
    /// binding, and direct callers) never re-parse the framing.
    ///
    /// A result poll that fails unreachable / timed out is followed by a
    /// producer-disappearance probe (liveliness keyexpr check): when the
    /// targeted producer's token is gone, its retained results died with the
    /// process and the goal resolves to a typed
    /// [`ResultStatus::Abandoned`] reply instead of the transport error.
    pub async fn request_result_with_sender(
        messenger_handle: &MessengerHandle,
        sender: &ActionWireSender,
        goal_id: &str,
        result_timeout: Duration,
    ) -> Result<ActionResultReply> {
        let action_name = sender.to_action_name().to_string();
        let payload = wrap_goal_payload(goal_id, &[])?;
        let message = match messenger_handle
            .poll_service(
                &sender.result_service(),
                payload,
                ServiceQueryKind::UserRequest,
                result_timeout,
            )
            .await
        {
            Ok(message) => message,
            Err(err) => {
                let err = Self::map_result_error(err, &action_name);
                if matches!(
                    err,
                    Error::ActionResultTimeout { .. } | Error::ActionResultUnreachable { .. }
                ) {
                    // An inconclusive probe (`Err`) must not fabricate an
                    // Abandoned outcome — only a confirmed-absent token does.
                    let alive = messenger_handle
                        .probe_action_producer(sender, LIVELINESS_PROBE_TIMEOUT)
                        .await
                        .unwrap_or(true);
                    if !alive {
                        return Ok(abandoned_reply(sender));
                    }
                }
                return Err(err);
            }
        };
        let instance_id = message.instance_id().to_string();
        let core_node = message.core_node().to_string();
        let wire = message.payload().into_inner();
        let (status, _) = unwrap_result_outcome(wire.as_ref())?;
        // `wire` is non-empty (unwrap_result_outcome guarantees it), so slicing
        // off the 1-byte tag is a zero-copy view of the same `Bytes`.
        Ok(ActionResultReply {
            status,
            body: Payload::from(wire.slice(1..)),
            instance_id,
            core_node,
        })
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

/// The outcome of a cancel request, carried in the cancel reply
/// (`ActionCancelResponse.state`). Replaces the old `accepted` / `error_message`
/// pair: the typed state subsumes both. There is no "no active goal" variant —
/// an unknown `goal_id` is [`Unknown`](CancelState::Unknown), and a goal that
/// already finished is [`AlreadyTerminal`](CancelState::AlreadyTerminal).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum CancelState {
    /// A live (pending) goal received the cancel signal.
    Signalled = 0,
    /// The goal had already reached a terminal state; there was nothing to
    /// cancel. Best-effort: only observable within the result-retention window.
    AlreadyTerminal = 1,
    /// No goal with that `goal_id` is known (never existed, or long evicted).
    Unknown = 2,
}

impl CancelState {
    fn from_tag(tag: u8) -> Option<Self> {
        match tag {
            0 => Some(Self::Signalled),
            1 => Some(Self::AlreadyTerminal),
            2 => Some(Self::Unknown),
            _ => None,
        }
    }
}

/// Encode the fixed cancel-ack response the concurrent-action engine sends in
/// reply to a cancel request. The worker reacts to the cancel *signal*; it never
/// produces this payload, so the bytes are encoded here once and reused for both
/// Rust and Python servers.
///
/// The single `state` field is positionally wire-compatible with the codegen's
/// per-action `CancelResponse` reader — see `schemas/action_cancel.capnp` and
/// `cancel_action_response_format()` in generator-internal.
pub fn encode_cancel_ack(state: CancelState) -> Result<Payload> {
    let mut builder = ::capnp::message::Builder::new_default();
    {
        let mut root =
            builder.init_root::<crate::action_cancel_capnp::action_cancel_response::Builder>();
        root.set_state(state as u8);
    }
    crate::encoding::encode_message(&builder)
}

/// Decode a cancel-ack produced by [`encode_cancel_ack`] into its
/// [`CancelState`]. Used by tests and any caller that needs to read the
/// framework's cancel reply without the generated per-action type.
pub fn decode_cancel_ack(payload: &[u8]) -> Result<CancelState> {
    let reader = crate::encoding::decode_message(payload)?;
    let root = reader
        .get_root::<crate::action_cancel_capnp::action_cancel_response::Reader>()
        .map_err(|e| Error::Deserialization(e.to_string()))?;
    let tag = root.get_state();
    CancelState::from_tag(tag)
        .ok_or_else(|| Error::Deserialization(format!("unknown cancel state tag {tag}")))
}

/// The terminal outcome of a goal, owned by the engine and decoupled from the
/// worker's [`GoalContext`]. Stored in a slot's [`GoalState::Terminal`] and
/// encoded onto the result reply by [`encode_result_outcome`].
#[derive(Clone)]
enum GoalOutcome {
    /// `complete` delivered this result payload (raw user capnp, may be empty).
    Completed(Payload),
    /// `complete_cancelled` delivered this result payload after a cancel.
    Cancelled(Payload),
    /// The context was dropped without delivering (early return, panic, drop).
    Abandoned,
}

impl GoalOutcome {
    fn status(&self) -> ResultStatus {
        match self {
            GoalOutcome::Completed(_) => ResultStatus::Completed,
            GoalOutcome::Cancelled(_) => ResultStatus::Cancelled,
            GoalOutcome::Abandoned => ResultStatus::Abandoned,
        }
    }

    fn body(&self) -> &[u8] {
        match self {
            GoalOutcome::Completed(payload) | GoalOutcome::Cancelled(payload) => payload.as_ref(),
            GoalOutcome::Abandoned => &[],
        }
    }
}

/// Encode a goal outcome as the `[status:u8][body]` result reply.
fn encode_result_outcome(outcome: &GoalOutcome) -> Payload {
    wrap_result_outcome(outcome.status(), outcome.body())
}

/// Lifecycle state of a goal's result, owned by the engine. A goal is `Pending`
/// from accept until it reaches a terminal outcome; result polls park their
/// responders in `Pending` and are drained on the transition to `Terminal`.
enum GoalState {
    /// The worker is running. `waiters` is a `Vec` — not a single slot — because
    /// a relay may poll twice and several clients may race one `goal_id`.
    Pending { waiters: Vec<ServiceResponder> },
    /// Terminal: the result stays fetchable until `evict_at`, after which the
    /// retention sweeper removes the slot and records a tombstone so late polls
    /// resolve to [`ResultStatus::Expired`].
    Terminal {
        outcome: GoalOutcome,
        evict_at: Instant,
    },
}

/// Per-goal routing state held in the registry from accept until the retention
/// sweeper evicts the terminal slot. Cheaply cloneable (all shared state is
/// behind `Arc`), so the loops clone it out from under the `std` registry lock
/// and then work on it without holding that lock across `.await`.
#[derive(Clone)]
struct GoalSlot {
    cancel: CancellationToken,
    state: Arc<TokioMutex<GoalState>>,
    /// Fast `is-terminal?` probe and the race guard that makes the first of
    /// `complete` / `complete_cancelled` / abandon win.
    terminal: Arc<AtomicBool>,
}

/// `goal_id` → live or recently-terminal goal. Guarded by a `std` mutex so the
/// cancel/result loops, `accept`, and the sweeper touch it without holding a
/// lock across `.await`.
type GoalRegistry = Arc<StdMutex<HashMap<String, GoalSlot>>>;

/// A result poll that arrived for a `goal_id` not yet in the registry (it beat
/// `accept`). Held until `accept` adopts it into the new slot or its `deadline`
/// passes (the sweeper drops it; the client then falls back to its own timeout).
struct PendingWaiter {
    responder: ServiceResponder,
    deadline: Instant,
}

/// `goal_id` → result polls parked before the goal was registered. Guarded by a
/// `std` mutex. Lock discipline: `pending_waiters` may be acquired while nothing
/// is held, and the registry may be acquired *while holding* it (the only nested
/// ordering); conversely the registry lock is always released before acquiring
/// `pending_waiters`, so there is no lock cycle.
type PendingWaiters = Arc<StdMutex<HashMap<String, Vec<PendingWaiter>>>>;

/// Upper bound on total parked-before-accept responders, guarding against a
/// flood of polls for never-registered goal_ids amplifying memory. On overflow
/// the responder is dropped (the client falls back to its own timeout).
const PENDING_WAITERS_CAP: usize = 1024;

/// How long a parked-before-accept poll stays parked before the sweeper drops
/// it. The client's own timeout governs when it gives up; this only bounds
/// server-side responder retention for polls that are never adopted.
const PENDING_WAITER_MAX_PARK: Duration = Duration::from_secs(30);

/// How often the retention sweeper runs. Independent of the per-goal retention
/// grace: the grace sets each slot's `evict_at`; the sweeper only needs to run
/// often enough to evict promptly. Cheap (a small map scan under a `std` lock).
const SWEEP_INTERVAL: Duration = Duration::from_millis(250);

/// Maximum number of evicted-goal tombstones retained. Bounds memory; a poll
/// for a goal_id that has aged out of this set parks and falls back to its
/// timeout, exactly as a never-existed goal_id would.
const EXPIRED_TOMBSTONE_CAP: usize = 4096;

/// Bounded, status-only record of goal_ids whose terminal result has been
/// evicted by the retention sweeper. A result poll for one resolves to
/// [`ResultStatus::Expired`] (and a cancel to [`CancelState::AlreadyTerminal`])
/// immediately, rather than parking. Oldest entries fall out once the cap is hit.
struct ExpiredTombstones {
    order: VecDeque<String>,
    set: HashSet<String>,
    cap: usize,
}

impl ExpiredTombstones {
    fn new(cap: usize) -> Self {
        Self {
            order: VecDeque::new(),
            set: HashSet::new(),
            cap,
        }
    }

    fn insert(&mut self, goal_id: String) {
        if self.set.contains(&goal_id) {
            return;
        }
        while self.order.len() >= self.cap {
            if let Some(old) = self.order.pop_front() {
                self.set.remove(&old);
            } else {
                break;
            }
        }
        self.set.insert(goal_id.clone());
        self.order.push_back(goal_id);
    }

    fn contains(&self, goal_id: &str) -> bool {
        self.set.contains(goal_id)
    }
}

/// Shared, lock-guarded [`ExpiredTombstones`].
type Tombstones = Arc<StdMutex<ExpiredTombstones>>;

/// Flip a goal's slot from `Pending` to `Terminal`, waking every parked result
/// poll exactly once with the encoded outcome. Returns `true` iff *this* call
/// performed the transition, so the caller closes the feedback stream once.
///
/// The first of `complete` / `complete_cancelled` / abandon to flip `terminal`
/// wins; later calls are no-ops. Park (in `run_result_loop`) and drain here
/// serialize on the same per-slot `TokioMutex`, so no wakeup is ever lost.
/// `ServiceResponder::respond` is by-value, so each parked responder is consumed
/// exactly once.
async fn transition_terminal(slot: &GoalSlot, outcome: GoalOutcome, retention: Duration) -> bool {
    if slot.terminal.swap(true, Ordering::SeqCst) {
        return false;
    }
    let reply = encode_result_outcome(&outcome);
    let waiters = {
        let mut guard = slot.state.lock().await;
        match std::mem::replace(
            &mut *guard,
            GoalState::Terminal {
                outcome,
                evict_at: Instant::now() + retention,
            },
        ) {
            GoalState::Pending { waiters } => waiters,
            // Unreachable: the `terminal` swap above gates any second transition.
            GoalState::Terminal { .. } => Vec::new(),
        }
    };
    for responder in waiters {
        let _ = responder.respond(reply.clone()).await;
    }
    true
}

/// What `run_result_loop` does after releasing every `std` lock:
/// `Some((responder, payload))` replies to this poll now with the given framed
/// payload; `None` means the poll was parked (in a slot or in `PendingWaiters`)
/// or dropped, with nothing to reply now.
type ResultAct = Option<(ServiceResponder, Payload)>;

/// Extract the `goal_id` carried by a cancel/result request payload (the same
/// length-prefixed envelope goals use, with an empty body).
fn goal_id_from_request(payload: &[u8]) -> Result<String> {
    let (goal_id, _) = unwrap_goal_payload(payload)?;
    Ok(goal_id.to_string())
}

/// Background loop: routes each incoming cancel request to the matching live
/// goal's [`CancellationToken`] and replies with a typed [`CancelState`]. There
/// is no "no active goal" reply: a live goal is `Signalled`, an
/// already-finished goal (still routable, or recently evicted) is
/// `AlreadyTerminal`, and anything else is `Unknown`.
async fn run_cancel_loop(
    mut cancel_service: ServiceEndpoint,
    registry: GoalRegistry,
    tombstones: Tombstones,
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

        let goal_id = match goal_id_from_request(context.message().payload_bytes().as_ref()) {
            Ok(goal_id) => goal_id,
            Err(_) => {
                let _ = responder
                    .respond_error("malformed cancel request payload".to_string())
                    .await;
                continue;
            }
        };

        // Clone the token + terminal flag out under the lock, then fire + respond
        // without holding the registry lock across the network reply.
        let found = registry
            .lock()
            .unwrap()
            .get(&goal_id)
            .map(|s| (s.cancel.clone(), s.terminal.load(Ordering::SeqCst)));
        let state = match found {
            Some((token, false)) => {
                token.cancel();
                CancelState::Signalled
            }
            Some((_, true)) => CancelState::AlreadyTerminal,
            None if tombstones.lock().unwrap().contains(&goal_id) => CancelState::AlreadyTerminal,
            None => CancelState::Unknown,
        };

        match encode_cancel_ack(state) {
            Ok(payload) => {
                let _ = responder.respond(payload).await;
            }
            Err(err) => {
                let _ = responder.respond_error(err.to_string()).await;
            }
        }
    }
}

/// Decide what to do with a result poll against an existing slot: reply with the
/// terminal outcome (or `Expired` if it is past its retention window but the
/// sweeper has not run yet), or park the responder in `Pending`. Holds only the
/// per-slot `TokioMutex`; the caller must release every `std` lock first.
async fn act_on_slot(slot: &GoalSlot, responder: ServiceResponder) -> ResultAct {
    let mut guard = slot.state.lock().await;
    match &mut *guard {
        GoalState::Terminal { outcome, evict_at } if *evict_at > Instant::now() => {
            Some((responder, encode_result_outcome(outcome)))
        }
        GoalState::Terminal { .. } => {
            Some((responder, wrap_result_outcome(ResultStatus::Expired, &[])))
        }
        GoalState::Pending { waiters } => {
            waiters.push(responder);
            None
        }
    }
}

/// Background loop: routes each incoming result request to the matching goal.
/// A `Pending` goal parks the responder until it reaches a terminal state; a
/// `Terminal` goal replies inline with its typed outcome; a poll that arrived
/// before `accept` parks in `PendingWaiters` to be adopted at registration; a
/// poll for an evicted goal replies `Expired`. There is no "no active goal"
/// reply — a poll always parks for a definitive answer or resolves to a typed
/// terminal outcome.
async fn run_result_loop(
    mut result_service: ServiceEndpoint,
    registry: GoalRegistry,
    pending_waiters: PendingWaiters,
    tombstones: Tombstones,
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

        let goal_id = match goal_id_from_request(context.message().payload_bytes().as_ref()) {
            Ok(goal_id) => goal_id,
            Err(_) => {
                let _ = responder
                    .respond_error("malformed result request payload".to_string())
                    .await;
                continue;
            }
        };

        // Clone the slot out under the std lock (dropped at the `;`); never hold
        // the registry lock across `.await`.
        let slot = registry.lock().unwrap().get(&goal_id).cloned();

        let act = match slot {
            Some(slot) => act_on_slot(&slot, responder).await,
            None => {
                // No slot: the poll beat `accept`, is for an evicted goal
                // (tombstone), or for a never-existed goal_id. Decide while
                // holding `pending_waiters`, re-reading the registry under it so
                // a concurrent `accept` that just inserted the slot (and whose
                // adoption pass therefore missed this poll) cannot orphan it.
                let pending = pending_waiters.lock().unwrap();
                let slot_now = registry.lock().unwrap().get(&goal_id).cloned();
                if let Some(slot) = slot_now {
                    drop(pending);
                    act_on_slot(&slot, responder).await
                } else if tombstones.lock().unwrap().contains(&goal_id) {
                    Some((responder, wrap_result_outcome(ResultStatus::Expired, &[])))
                } else {
                    let mut pending = pending;
                    let total: usize = pending.values().map(Vec::len).sum();
                    if total >= PENDING_WAITERS_CAP {
                        // Overflow: drop the responder; the client falls back to
                        // its own timeout.
                        None
                    } else {
                        pending
                            .entry(goal_id.clone())
                            .or_default()
                            .push(PendingWaiter {
                                responder,
                                deadline: Instant::now() + PENDING_WAITER_MAX_PARK,
                            });
                        None
                    }
                }
            }
        };

        if let Some((responder, payload)) = act {
            let _ = responder.respond(payload).await;
        }
    }
}

/// Background loop: evicts terminal slots past their retention window (recording
/// a tombstone so late polls still resolve to `Expired`) and drops parked
/// polls whose deadline has passed.
async fn run_retention_sweeper(
    registry: GoalRegistry,
    pending_waiters: PendingWaiters,
    tombstones: Tombstones,
    stop: CancellationToken,
) {
    loop {
        tokio::select! {
            _ = stop.cancelled() => return,
            _ = tokio::time::sleep(SWEEP_INTERVAL) => {}
        }
        sweep_once(&registry, &pending_waiters, &tombstones, Instant::now());
    }
}

/// One retention-sweeper pass against an explicit `now`: evict terminal slots
/// whose `evict_at <= now` (tombstoning each so late polls resolve to `Expired`)
/// and drop parked-before-accept polls whose `deadline <= now`. The `now` is a
/// parameter rather than read inside, so the eviction logic is unit-testable
/// deterministically without the network or wall-clock waits.
fn sweep_once(
    registry: &GoalRegistry,
    pending_waiters: &PendingWaiters,
    tombstones: &Tombstones,
    now: Instant,
) {
    // Evict terminal slots past their window, tombstoning each. `try_lock`
    // skips a slot whose per-slot mutex is momentarily held (e.g. mid
    // transition or being read by a poll); it is rechecked next tick.
    let mut expired_ids = Vec::new();
    registry
        .lock()
        .unwrap()
        .retain(|goal_id, slot| match slot.state.try_lock() {
            Ok(guard) => match &*guard {
                GoalState::Terminal { evict_at, .. } if *evict_at <= now => {
                    expired_ids.push(goal_id.clone());
                    false
                }
                _ => true,
            },
            Err(_) => true,
        });
    if !expired_ids.is_empty() {
        let mut tomb = tombstones.lock().unwrap();
        for goal_id in expired_ids {
            tomb.insert(goal_id);
        }
    }

    // Drop parked-before-accept polls whose deadline has passed (their
    // responders close, so the client falls back to its own timeout).
    pending_waiters.lock().unwrap().retain(|_, waiters| {
        waiters.retain(|w| w.deadline > now);
        !waiters.is_empty()
    });
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
    pending_waiters: PendingWaiters,
    has_feedback: bool,
    result_retention_grace: Duration,
    stop: CancellationToken,
    cancel_loop: TaskHandle<()>,
    result_loop: TaskHandle<()>,
    sweeper_loop: TaskHandle<()>,
    /// Producer-instance liveliness advertisement, held so consumers see
    /// this producer as alive for exactly as long as the engine can route
    /// goals/cancels/results.
    _liveliness_token: ActionLivelinessToken,
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
            liveliness_token,
        } = creation;
        let registry: GoalRegistry = Arc::new(StdMutex::new(HashMap::new()));
        let pending_waiters: PendingWaiters = Arc::new(StdMutex::new(HashMap::new()));
        let tombstones: Tombstones =
            Arc::new(StdMutex::new(ExpiredTombstones::new(EXPIRED_TOMBSTONE_CAP)));
        let stop = CancellationToken::new();
        let cancel_loop = spawn(run_cancel_loop(
            cancel_service,
            Arc::clone(&registry),
            Arc::clone(&tombstones),
            stop.clone(),
        ));
        let result_loop = spawn(run_result_loop(
            result_service,
            Arc::clone(&registry),
            Arc::clone(&pending_waiters),
            Arc::clone(&tombstones),
            stop.clone(),
        ));
        let sweeper_loop = spawn(run_retention_sweeper(
            Arc::clone(&registry),
            Arc::clone(&pending_waiters),
            tombstones,
            stop.clone(),
        ));
        Self {
            goal_service,
            factory: feedback_publisher_factory,
            registry,
            pending_waiters,
            has_feedback,
            result_retention_grace: RESULT_RETENTION_GRACE,
            stop,
            cancel_loop,
            result_loop,
            sweeper_loop,
            _liveliness_token: liveliness_token,
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
            let (goal_id, request_bytes) = split_goal_envelope(&wire)?;
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
            pending_waiters: Arc::clone(&self.pending_waiters),
            result_retention_grace: self.result_retention_grace,
        }))
    }
}

impl Drop for ConcurrentAction {
    fn drop(&mut self) {
        self.stop.cancel();
        self.cancel_loop.abort();
        self.result_loop.abort();
        self.sweeper_loop.abort();
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
    pending_waiters: PendingWaiters,
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
        let state = Arc::new(TokioMutex::new(GoalState::Pending {
            waiters: Vec::new(),
        }));
        let slot = GoalSlot {
            cancel: CancellationToken::new(),
            state: Arc::clone(&state),
            terminal: Arc::new(AtomicBool::new(false)),
        };
        self.registry
            .lock()
            .unwrap()
            .insert(self.goal_id.clone(), slot.clone());

        // Adopt any result polls that parked before this slot existed. The
        // registry insert above is visible to a concurrent `run_result_loop`
        // before this drain, and that loop re-reads the registry under the
        // `pending_waiters` lock — so an early poll either parked here (and we
        // adopt it now) or sees the slot and parks in it directly. Never orphaned.
        let adopted = self.pending_waiters.lock().unwrap().remove(&self.goal_id);
        if let Some(adopted) = adopted
            && let GoalState::Pending { waiters } = &mut *state.lock().await
        {
            waiters.extend(adopted.into_iter().map(|p| p.responder));
        }

        if let Err(err) = self.responder.respond(response).await {
            // Reply failed (client gone): don't leak the just-registered slot.
            // Any adopted waiters are dropped with it (their polls close cleanly).
            self.registry.lock().unwrap().remove(&self.goal_id);
            return Err(err);
        }

        Ok(GoalContext {
            goal_id: self.goal_id,
            request_bytes: self.request_bytes,
            slot,
            feedback: self.feedback,
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
/// Carries the decoded request bytes, the per-goal feedback publisher, and the
/// shared goal slot (cancel signal + result-delivery state). Cheaply movable
/// into a spawned task.
pub struct GoalContext {
    goal_id: String,
    request_bytes: Bytes,
    /// The shared slot this context drives. `complete` / `complete_cancelled` /
    /// `Drop` transition it to a terminal state; the engine's retention sweeper
    /// owns its eventual eviction, so this context never touches the registry.
    slot: GoalSlot,
    feedback: Option<ActionFeedbackPublisher>,
    /// How long a terminal result stays routable, measured from the terminal
    /// transition; propagated from the [`ConcurrentAction`].
    result_retention_grace: Duration,
    /// Runtime handle captured at accept time so `Drop` can schedule its async
    /// cleanup (marking the goal abandoned, closing the feedback stream) even
    /// when the context is dropped off the runtime. This is the case in the
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

    /// A clone of this goal's feedback publisher, for handing to a feedback
    /// forwarder sub-task that runs alongside the worker. `None` when the action
    /// declares no feedback topic. The publisher is scoped to this goal, so
    /// forwarded messages only reach this goal's stream.
    pub fn feedback_publisher(&self) -> Option<ActionFeedbackPublisher> {
        self.feedback.clone()
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
        self.slot.cancel.cancelled().await;
    }

    /// Whether a cancel has been requested for this goal.
    pub fn is_cancelled(&self) -> bool {
        self.slot.cancel.is_cancelled()
    }

    /// Deliver the final result for this goal. Idempotent: the first terminal
    /// transition (`complete` / `complete_cancelled` / abandon-on-drop) wins;
    /// later calls are no-ops.
    pub async fn complete(&self, result: Payload) -> Result<()> {
        self.deliver(GoalOutcome::Completed(result)).await
    }

    /// Deliver the final result after observing a cancel. Records the outcome as
    /// [`ResultStatus::Cancelled`] (distinct from `complete`); otherwise
    /// identical and equally idempotent.
    pub async fn complete_cancelled(&self, result: Payload) -> Result<()> {
        self.deliver(GoalOutcome::Cancelled(result)).await
    }

    async fn deliver(&self, outcome: GoalOutcome) -> Result<()> {
        // Transition the slot to terminal, waking any parked result polls with
        // the typed outcome. The slot stays routable until the retention sweeper
        // evicts it, so a late `get_result` still finds the result.
        let transitioned =
            transition_terminal(&self.slot, outcome, self.result_retention_grace).await;
        if transitioned {
            // First terminal transition wins: close this goal's feedback stream
            // so the client's drain loop ends. Gated so the sentinel is emitted
            // exactly once even if `complete` races an abandon-on-drop.
            if let Some(publisher) = &self.feedback {
                let _ = publisher.publish_end().await;
            }
        }
        Ok(())
    }
}

/// How long a terminal result stays routable after its goal reaches a terminal
/// state, measured from the terminal transition (not from context drop). Part
/// of the protocol contract: a `get_result` within this window resolves to the
/// typed outcome; after it the retention sweeper evicts the slot and a late poll
/// resolves to [`ResultStatus::Expired`]. Overridable per-server via
/// [`ConcurrentAction::with_result_retention_grace`].
const RESULT_RETENTION_GRACE: Duration = Duration::from_secs(30);

impl Drop for GoalContext {
    fn drop(&mut self) {
        // If the goal already reached a terminal state (`complete` ran), there is
        // nothing to do: the sweeper evicts the terminal slot after its window.
        if self.slot.terminal.load(Ordering::SeqCst) {
            return;
        }
        // Otherwise the goal was abandoned: the context dropped without
        // delivering (early return, a panic in the worker, or simply dropped).
        // Transition the slot to Terminal{Abandoned}, waking any parked polls
        // with a typed `Abandoned` status, and close the feedback stream so a
        // client draining feedback breaks out instead of hanging forever.
        // `transition_terminal` / `publish_end` are async, so run them on the
        // captured runtime handle (works even when `Drop` runs off-runtime, e.g.
        // via Python's GC). The slot is NOT removed here — the sweeper owns
        // eviction — so a poll that arrives between this spawn and its execution
        // still sees `Pending`, parks, and is woken by the transition.
        let slot = self.slot.clone();
        let grace = self.result_retention_grace;
        let feedback = self.feedback.clone();
        self.runtime.spawn(async move {
            let transitioned = transition_terminal(&slot, GoalOutcome::Abandoned, grace).await;
            if transitioned && let Some(publisher) = feedback {
                let _ = publisher.publish_end().await;
            }
        });
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
    fn result_outcome_roundtrips_each_status() {
        for status in [
            ResultStatus::Completed,
            ResultStatus::Cancelled,
            ResultStatus::Abandoned,
            ResultStatus::Expired,
        ] {
            let wire = wrap_result_outcome(status, b"payload");
            let (decoded, body) = unwrap_result_outcome(wire.as_ref()).expect("unwrap outcome");
            assert_eq!(decoded, status);
            assert_eq!(body, b"payload");
        }
    }

    #[test]
    fn result_outcome_with_empty_body() {
        let wire = wrap_result_outcome(ResultStatus::Abandoned, b"");
        let (status, body) = unwrap_result_outcome(wire.as_ref()).expect("unwrap outcome");
        assert_eq!(status, ResultStatus::Abandoned);
        assert!(body.is_empty());
    }

    #[test]
    fn unwrap_result_outcome_rejects_empty_wire() {
        match unwrap_result_outcome(&[]) {
            Err(Error::InternalEncodingError { identifier, .. }) => {
                assert_eq!(identifier, "action_result_outcome_envelope");
            }
            other => panic!("expected InternalEncodingError, got {other:?}"),
        }
    }

    #[test]
    fn unwrap_result_outcome_rejects_unknown_tag() {
        match unwrap_result_outcome(&[0xFF, 1, 2, 3]) {
            Err(Error::InternalEncodingError { identifier, reason }) => {
                assert_eq!(identifier, "action_result_outcome_envelope");
                assert!(reason.contains("unknown result status tag"));
            }
            other => panic!("expected InternalEncodingError, got {other:?}"),
        }
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

    fn empty_registry() -> GoalRegistry {
        Arc::new(StdMutex::new(HashMap::new()))
    }

    fn empty_pending() -> PendingWaiters {
        Arc::new(StdMutex::new(HashMap::new()))
    }

    fn fresh_tombstones() -> Tombstones {
        Arc::new(StdMutex::new(ExpiredTombstones::new(EXPIRED_TOMBSTONE_CAP)))
    }

    /// A terminal goal slot fixture with the given eviction deadline. The
    /// outcome is irrelevant to retention, so the body-less `Abandoned` keeps
    /// the fixture minimal.
    fn terminal_slot(evict_at: Instant) -> GoalSlot {
        GoalSlot {
            cancel: CancellationToken::new(),
            state: Arc::new(TokioMutex::new(GoalState::Terminal {
                outcome: GoalOutcome::Abandoned,
                evict_at,
            })),
            terminal: Arc::new(AtomicBool::new(true)),
        }
    }

    // Determinism: the retention sweeper's eviction logic is exercised against an
    // explicit `now` under tokio's virtual clock, so there is no wall-clock sleep
    // and no race with a real-time sweeper interval. `start_paused` only provides
    // a runtime for `Instant::now()`; the assertions key off the injected `now`.
    #[tokio::test(start_paused = true)]
    async fn sweep_evicts_only_terminal_slots_past_their_window() {
        let now = Instant::now();
        let registry = empty_registry();
        let pending = empty_pending();
        let tombstones = fresh_tombstones();

        registry.lock().unwrap().insert(
            "expired".to_string(),
            terminal_slot(now - Duration::from_millis(1)),
        );
        registry.lock().unwrap().insert(
            "fresh".to_string(),
            terminal_slot(now + Duration::from_secs(10)),
        );

        sweep_once(&registry, &pending, &tombstones, now);

        let registry = registry.lock().unwrap();
        assert!(
            !registry.contains_key("expired"),
            "past-window slot should be evicted"
        );
        assert!(
            registry.contains_key("fresh"),
            "in-window slot should be retained"
        );
        let tombstones = tombstones.lock().unwrap();
        assert!(
            tombstones.contains("expired"),
            "evicted goal should be tombstoned"
        );
        assert!(
            !tombstones.contains("fresh"),
            "retained goal should not be tombstoned"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn sweep_skips_slot_whose_state_is_momentarily_locked() {
        let now = Instant::now();
        let registry = empty_registry();
        let pending = empty_pending();
        let tombstones = fresh_tombstones();

        // Expired, but its per-slot mutex is held (as during a transition or a
        // concurrent poll), so `try_lock` fails and the sweep must leave it for
        // the next tick rather than evicting a slot it cannot inspect.
        let slot = terminal_slot(now - Duration::from_secs(1));
        registry
            .lock()
            .unwrap()
            .insert("busy".to_string(), slot.clone());
        let _held = slot.state.lock().await;

        sweep_once(&registry, &pending, &tombstones, now);

        assert!(
            registry.lock().unwrap().contains_key("busy"),
            "a slot whose state is locked must be skipped and retried next tick"
        );
        assert!(!tombstones.lock().unwrap().contains("busy"));
    }
}
