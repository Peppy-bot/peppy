use super::discovery::{budget_left_after_discovery, discover_producer};
use super::generate_short_id;
use super::topics::Subscription;
use super::{MessengerHandle, ServiceEndpoint, ServiceResponder, TopicPublisher};
use crate::error::{Error, Result};
use crate::messaging::ProducerRef;
use crate::runtime::{CancellationToken, TaskHandle, spawn};
use crate::types::{Message, Payload};
use bytes::{BufMut, Bytes, BytesMut};
use config::failure_kind::PeppyFailureKind;
use pmi::{
    ActionWireReceiver, ActionWireSender, ClientPresence, Failure, FeedbackBound, FeedbackBuffer,
    FeedbackOverflow, LivelinessEvent, LivelinessToken, LivelinessWatch, PublisherQoS, Segment,
    SenderTarget, ServiceQueryKind,
};
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::{Mutex as TokioMutex, Notify};
// tokio's clock so the retention/eviction timeouts run on virtual time under
// `#[tokio::test(start_paused = true)]`; in a normal runtime it reads the real
// monotonic clock.
use tokio::time::{Duration, Instant};
use tracing::warn;

pub struct ActionMessenger;

/// How many unread feedback messages an action client keeps by default. A
/// client that reads the feedback and falls behind by more cancels the goal
/// (see [`ActionMessenger::send_goal`]).
pub const DEFAULT_FEEDBACK_BOUND: FeedbackBound =
    FeedbackBound::new(std::num::NonZeroUsize::new(1024).expect("1024 is not zero"));

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

/// Wrap a goal request: the goal envelope of [`wrap_goal_payload`], whose
/// body starts with the presence generation of the sender. The provider
/// watches the presence token of that generation and cancels the goal when
/// its sender is gone.
///
/// Layout: `[goal_id_len: u8][goal_id][generation_len: u8][generation][user_payload]`.
pub fn wrap_goal_request(
    goal_id: &str,
    sender_generation: &str,
    user_payload: &[u8],
) -> Result<Payload> {
    if sender_generation.is_empty() || sender_generation.len() > u8::MAX as usize {
        return Err(envelope_error(format!(
            "sender generation length {} is outside 1..={}",
            sender_generation.len(),
            u8::MAX
        )));
    }
    let mut body = BytesMut::with_capacity(1 + sender_generation.len() + user_payload.len());
    body.put_u8(sender_generation.len() as u8);
    body.extend_from_slice(sender_generation.as_bytes());
    body.extend_from_slice(user_payload);
    wrap_goal_payload(goal_id, &body)
}

/// A goal request split by [`split_goal_request`].
struct GoalRequestEnvelope {
    goal_id: String,
    sender_generation: String,
    user_payload: Bytes,
}

/// Split a goal request built by [`wrap_goal_request`]. The user payload is
/// a zero-copy slice of `wire`.
fn split_goal_request(wire: &Bytes) -> Result<GoalRequestEnvelope> {
    let (goal_id, body) = split_goal_envelope(wire)?;
    let generation_len = *body
        .first()
        .ok_or_else(|| envelope_error("goal request has no sender generation"))?
        as usize;
    let payload_start = 1 + generation_len;
    if generation_len == 0 || body.len() < payload_start {
        return Err(envelope_error(format!(
            "goal request too short for its sender generation of {generation_len} bytes"
        )));
    }
    let sender_generation = std::str::from_utf8(&body[1..payload_start])
        .map_err(|err| envelope_error(format!("sender generation is not valid UTF-8: {err}")))?
        .to_string();
    Ok(GoalRequestEnvelope {
        goal_id,
        sender_generation,
        user_payload: body.slice(payload_start..),
    })
}

const RESULT_OUTCOME_ENVELOPE: &str = "action_result_outcome_envelope";

fn result_outcome_error(reason: impl Into<String>) -> Error {
    Error::InternalEncodingError {
        identifier: RESULT_OUTCOME_ENVELOPE.to_string(),
        reason: reason.into(),
    }
}

/// The terminal status of a goal, carried as a 1-byte tag prefixing the result
/// reply. Mirrors the engine-level framing of [`wrap_goal_payload`] (no capnp).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum ResultStatus {
    /// The worker delivered a result via `complete`: the goal is a success.
    Completed = 0,
    /// The worker ended the goal via `complete_cancelled`, with a reason and
    /// a result.
    Cancelled = 1,
    /// The worker abandoned the goal: its context dropped without delivering a
    /// result (early return, panic, or simply dropped).
    Abandoned = 2,
    /// The goal reached a terminal state, but its result was retained only for a
    /// bounded window that has since elapsed (or the result was already evicted).
    Expired = 3,
    /// The worker failed the goal via `fail`, with a failure kind and a
    /// message, and no result.
    Failed = 4,
}

impl ResultStatus {
    fn from_tag(tag: u8) -> Option<Self> {
        match tag {
            0 => Some(Self::Completed),
            1 => Some(Self::Cancelled),
            2 => Some(Self::Abandoned),
            3 => Some(Self::Expired),
            4 => Some(Self::Failed),
            _ => None,
        }
    }
}

/// How a goal ended: the terminal status and what it carries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GoalOutcome {
    /// `complete` delivered this result payload (raw user capnp, may be
    /// empty): the goal is a success.
    Completed(Payload),
    /// `complete_cancelled` ended the goal for `reason`, with this result
    /// payload.
    Cancelled { reason: String, result: Payload },
    /// `fail` ended the goal with this failure, and no result.
    Failed(Failure),
    /// The context was dropped without delivering (early return, panic, drop),
    /// or the producer died with the goal.
    Abandoned,
    /// The goal ended, but its result is no longer retained. Never stored by
    /// the engine: it answers a poll that comes after the retention window.
    Expired,
}

impl GoalOutcome {
    pub fn status(&self) -> ResultStatus {
        match self {
            GoalOutcome::Completed(_) => ResultStatus::Completed,
            GoalOutcome::Cancelled { .. } => ResultStatus::Cancelled,
            GoalOutcome::Failed(_) => ResultStatus::Failed,
            GoalOutcome::Abandoned => ResultStatus::Abandoned,
            GoalOutcome::Expired => ResultStatus::Expired,
        }
    }
}

/// Frame a result reply. Layout, by status:
/// - completed: `[0][result]`
/// - cancelled: `[1][reason_len: u32 BE][reason: UTF-8][result]`
/// - abandoned: `[2]`
/// - expired: `[3]`
/// - failed: `[4][failure]` (the wire form of [`Failure`])
///
/// Symmetric counterpart of [`unwrap_result_outcome`].
pub fn wrap_result_outcome(outcome: &GoalOutcome) -> Payload {
    let mut buf = BytesMut::new();
    buf.put_u8(outcome.status() as u8);
    match outcome {
        GoalOutcome::Completed(result) => buf.extend_from_slice(result.as_ref()),
        GoalOutcome::Cancelled { reason, result } => {
            buf.put_u32(reason.len() as u32);
            buf.extend_from_slice(reason.as_bytes());
            buf.extend_from_slice(result.as_ref());
        }
        GoalOutcome::Failed(failure) => failure.encode_into(&mut buf),
        GoalOutcome::Abandoned | GoalOutcome::Expired => {}
    }
    Payload::from(buf.freeze())
}

/// Decode a result reply produced by [`wrap_result_outcome`]. Payloads are
/// zero-copy slices of `wire`. Errors on an empty wire, an unknown tag, or a
/// malformed body.
pub(crate) fn unwrap_result_outcome(wire: &Bytes) -> Result<GoalOutcome> {
    let tag = *wire
        .first()
        .ok_or_else(|| result_outcome_error("wire payload is empty"))?;
    let status = ResultStatus::from_tag(tag)
        .ok_or_else(|| result_outcome_error(format!("unknown result status tag {tag}")))?;
    let body = wire.slice(1..);
    Ok(match status {
        ResultStatus::Completed => GoalOutcome::Completed(Payload::from(body)),
        ResultStatus::Cancelled => {
            let reason_len = body
                .get(..4)
                .map(|bytes| u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as usize)
                .ok_or_else(|| result_outcome_error("cancelled result too short for its reason"))?;
            let result_start = 4 + reason_len;
            let reason = body.get(4..result_start).ok_or_else(|| {
                result_outcome_error(format!(
                    "cancelled result too short for its reason of {reason_len} bytes"
                ))
            })?;
            let reason = std::str::from_utf8(reason)
                .map_err(|err| result_outcome_error(format!("reason is not valid UTF-8: {err}")))?
                .to_string();
            GoalOutcome::Cancelled {
                reason,
                result: Payload::from(body.slice(result_start..)),
            }
        }
        ResultStatus::Failed => GoalOutcome::Failed(
            Failure::decode(&body).map_err(|err| result_outcome_error(err.to_string()))?,
        ),
        ResultStatus::Abandoned => GoalOutcome::Abandoned,
        ResultStatus::Expired => GoalOutcome::Expired,
    })
}

/// The typed reply from [`ActionMessenger::request_result`]: how the goal
/// ended, and the producer instance that answered. Decoding here (in the
/// messenger, mirroring how `send_goal` owns the goal envelope) keeps the
/// framing out of generated code, so Rust and Python decode identically.
#[derive(Debug)]
pub struct ActionResultReply {
    pub outcome: GoalOutcome,
    pub instance_id: String,
    pub core_node: String,
}

const GOAL_ACK_ENVELOPE: &str = "action_goal_ack_envelope";

fn goal_ack_error(reason: impl Into<String>) -> Error {
    Error::InternalEncodingError {
        identifier: GOAL_ACK_ENVELOPE.to_string(),
        reason: reason.into(),
    }
}

/// The provider's answer to a goal request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GoalAnswer {
    /// The provider accepted the goal, with its declared goal response
    /// payload (empty when the action declares none).
    Accepted(Payload),
    /// The provider refused the goal: a failure kind and a message.
    Refused(Failure),
}

/// Frame the answer of an accepted goal as `[1][response]`.
///
/// The admission ack is framework protocol, mirroring the engine-level
/// framing of [`wrap_result_outcome`] (no capnp). Carrying it in the envelope
/// keeps admission out of the declared response schema, so `GoalResponse`
/// contains exactly the fields the contract declares.
///
/// Producers and consumers must run the same peppylib generation: mixed
/// versions are unsupported; upgrade all nodes of a deployment together, as
/// peppy releases require.
pub fn wrap_goal_accepted(response: &[u8]) -> Payload {
    let mut buf = BytesMut::with_capacity(1 + response.len());
    buf.put_u8(1);
    buf.extend_from_slice(response);
    Payload::from(buf.freeze())
}

/// Frame the answer of a refused goal as `[0][failure]` (the wire form of
/// [`Failure`]). A refusal carries no goal response.
pub fn wrap_goal_refused(failure: &Failure) -> Payload {
    let mut buf = BytesMut::with_capacity(1 + failure.encoded_len());
    buf.put_u8(0);
    failure.encode_into(&mut buf);
    Payload::from(buf.freeze())
}

/// Decode a goal answer produced by [`wrap_goal_accepted`] or
/// [`wrap_goal_refused`]. The response is a zero-copy slice of `wire`.
pub(crate) fn unwrap_goal_ack(wire: &Bytes) -> Result<GoalAnswer> {
    match wire.first() {
        Some(1) => Ok(GoalAnswer::Accepted(Payload::from(wire.slice(1..)))),
        Some(0) => Failure::decode(&wire[1..])
            .map(GoalAnswer::Refused)
            .map_err(|err| goal_ack_error(err.to_string())),
        Some(tag) => Err(goal_ack_error(format!("unknown accepted tag {tag}"))),
        None => Err(goal_ack_error("wire payload is empty")),
    }
}

/// The typed goal reply carried by [`ActionGoalHandle`]: the provider's
/// answer, decoded in the messenger so Rust and Python decode identically.
/// `instance_id` / `core_node` identify the producer instance that answered
/// the goal.
#[derive(Debug)]
pub struct ActionGoalReply {
    pub answer: GoalAnswer,
    pub instance_id: String,
    pub core_node: String,
}

impl ActionGoalReply {
    pub fn is_accepted(&self) -> bool {
        matches!(self.answer, GoalAnswer::Accepted(_))
    }

    /// The failure of a refused goal.
    pub fn refusal(&self) -> Option<&Failure> {
        match &self.answer {
            GoalAnswer::Refused(failure) => Some(failure),
            GoalAnswer::Accepted(_) => None,
        }
    }
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

/// Re-exported from `core-node-api` so the payload type that
/// [`ActionFeedbackPublisher::publish`] takes sits beside it in
/// `peppylib::messaging`.
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
/// the per-goal feedback publisher, the embedded `goal_id` and sender
/// generation, and the envelope-stripped user payload ready to be decoded by
/// the goal handler.
pub struct DeclaredFeedback {
    pub publisher: ActionFeedbackPublisher,
    pub goal_id: String,
    pub sender_generation: String,
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
        let request = split_goal_request(&wire)?;
        let publisher = self.declare_for_goal(link_id, &request.goal_id).await?;
        Ok(DeclaredFeedback {
            publisher,
            goal_id: request.goal_id,
            sender_generation: request.sender_generation,
            user_payload: request.user_payload,
        })
    }

    /// Declare the feedback publisher of the goal `goal_id`, under the
    /// link_id its consumer targeted.
    async fn declare_for_goal(
        &self,
        link_id: &str,
        goal_id: &str,
    ) -> Result<ActionFeedbackPublisher> {
        // The goal_id is appended to the feedback topic to scope the publisher
        // per goal cycle. Reject anything that could let a malicious or
        // malformed envelope escape that scope (extra segments, Zenoh
        // wildcards, ...) so the publisher cannot be steered onto a topic the
        // server didn't intend.
        if !is_safe_goal_id(goal_id) {
            return Err(envelope_error(format!(
                "goal_id contains unsafe characters: {goal_id:?}"
            )));
        }
        let inner = self
            .messenger
            .declare_action_feedback_publisher(&self.receiver, link_id, goal_id, self.qos)
            .await?;
        Ok(ActionFeedbackPublisher::new(TopicPublisher::new(Arc::new(
            inner,
        ))))
    }

    /// The messenger the factory declares publishers on.
    fn messenger(&self) -> &MessengerHandle {
        &self.messenger
    }
}

// ---------------------------------------------------------------------------
// Producer-disappearance detection
// ---------------------------------------------------------------------------
//
// The end-of-stream sentinel only reaches the consumer while the producer's
// session is alive to publish it. A producer that dies hard (SIGKILL, OOM,
// runtime teardown winning the race against `GoalContext::Drop`'s spawned
// cleanup) never sends it, and the consumer's feedback subscription (pinned
// to the dead `instance_id`) would otherwise wait forever. Every exposed
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

/// `true` once a liveliness token is confirmed absent: every confirmation
/// probe found no token. Any probe that sees the token (or errors, e.g.
/// while the watcher's own session is closing) aborts the confirmation, so a
/// flap or an inconclusive probe never fabricates a death.
async fn confirm_gone<P, F>(probe: &P) -> bool
where
    P: Fn() -> F,
    F: std::future::Future<Output = Result<bool>>,
{
    for _ in 0..PRODUCER_GONE_CONFIRM_PROBES {
        tokio::time::sleep(PRODUCER_GONE_CONFIRM_DELAY).await;
        match probe().await {
            Ok(false) => continue,
            Ok(true) | Err(_) => return false,
        }
    }
    true
}

/// The liveliness policy shared by the producer watch of a caller and the
/// sender watch of a provider: turns the raw event stream of one token into
/// a confirmed end. Returns `true` once the token is confirmed gone, and
/// `false` when the watch channel closes first (the watcher's own session is
/// going away).
async fn wait_until_confirmed_gone<P, F>(watch: LivelinessWatch, probe: P) -> bool
where
    P: Fn() -> F,
    F: std::future::Future<Output = Result<bool>>,
{
    // Initial resolve: the token is expected to replay as an immediate
    // `Alive` (the watch subscribes with history). Seeing nothing inside the
    // window means the token's owner died in the gap, or its declaration has
    // not reached this session yet: confirm by probing rather than trusting
    // propagation timing.
    match tokio::time::timeout(LIVELINESS_RESOLVE_TIMEOUT, watch.rx.recv_async()).await {
        Ok(Ok(LivelinessEvent::Alive(()))) => {}
        Ok(Ok(LivelinessEvent::Gone(()))) | Err(_) => {
            if confirm_gone(&probe).await {
                return true;
            }
        }
        Ok(Err(_)) => return false,
    }

    // Steady state: react to `Gone` transitions, tolerating `Alive`/`Gone`
    // flaps (a router bounce deletes and re-announces tokens).
    loop {
        match watch.rx.recv_async().await {
            Ok(LivelinessEvent::Alive(())) => {}
            Ok(LivelinessEvent::Gone(())) => {
                if confirm_gone(&probe).await {
                    return true;
                }
            }
            Err(_) => return false,
        }
    }
}

/// Background policy task behind [`ProducerGoneWatch`]: latches "producer
/// gone" once [`wait_until_confirmed_gone`] confirms it. A closed watch
/// channel latches nothing: the feedback channel reports that as
/// `ActionFeedbackChannelClosed`.
async fn run_producer_liveliness_watch(
    watch: LivelinessWatch,
    messenger: MessengerHandle,
    sender: ActionWireSender,
    gone: Arc<AtomicBool>,
    notify: Arc<Notify>,
) {
    let probe = || messenger.probe_action_producer(&sender, LIVELINESS_PROBE_TIMEOUT);
    if wait_until_confirmed_gone(watch, probe).await {
        gone.store(true, Ordering::Release);
        notify.notify_waiters();
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
    fn spawn(watch: LivelinessWatch, messenger: MessengerHandle, sender: ActionWireSender) -> Self {
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
    messenger: MessengerHandle,
    sender: ActionWireSender,
    goal_id: String,
    goal_reply: ActionGoalReply,
    feedback: Subscription,
    feedback_bound: FeedbackBound,
    feedback_overflow: FeedbackOverflow,
    /// Cancels the goal once its feedback overflows, from the first read of
    /// the feedback on (see [`ActionGoalHandle::follow_feedback`]). `None`
    /// before that read, and for a refused goal, which has nothing to
    /// cancel.
    overflow_cancel: Option<OverflowCancel>,
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

/// Whether a goal request that failed with `err` may have started a goal on
/// its provider: the provider acknowledged the request and gave no answer
/// in time. A request that reached no provider (`ServiceUnreachable`), or
/// that the session could not send, started none.
fn request_may_have_started_a_goal(err: &Error) -> bool {
    matches!(err, Error::ServiceTimeout { .. })
}

/// How long a cancel that the client sends on its own (after a feedback
/// overflow, or for a goal whose answer it never read) waits for the
/// provider's answer.
const OWN_CANCEL_TIMEOUT: Duration = Duration::from_secs(5);

/// Sends the provider a cancel of `goal_id` from a task of its own, and logs
/// a cancel that fails. `why` names the cause in that log. Sends nothing
/// outside a runtime, which has no task to send it from.
fn spawn_own_cancel(
    messenger: MessengerHandle,
    sender: ActionWireSender,
    goal_id: String,
    why: &'static str,
) {
    if tokio::runtime::Handle::try_current().is_err() {
        warn!(%goal_id, why, "the cancel of a goal was not sent: no runtime runs");
        return;
    }
    spawn(async move {
        if let Err(err) =
            ActionMessenger::cancel_with_sender(&messenger, &sender, &goal_id, OWN_CANCEL_TIMEOUT)
                .await
        {
            warn!(%err, %goal_id, why, "the cancel of a goal failed");
        }
    });
}

/// The task that cancels a goal once its feedback overflows. Dropping the
/// handle before the overflow aborts the task; after the overflow, the task
/// runs on, so the cancel goes out even when the caller drops its handle at
/// once on the overflow error.
struct OverflowCancel {
    task: TaskHandle<()>,
    overflow: FeedbackOverflow,
}

impl OverflowCancel {
    fn spawn(
        messenger: MessengerHandle,
        sender: ActionWireSender,
        goal_id: String,
        overflow: FeedbackOverflow,
    ) -> Self {
        let raised = overflow.clone();
        let task = spawn(async move {
            raised.raised().await;
            spawn_own_cancel(
                messenger,
                sender,
                goal_id,
                "its caller fell behind its feedback",
            );
        });
        Self { task, overflow }
    }
}

impl Drop for OverflowCancel {
    fn drop(&mut self) {
        if !self.overflow.is_raised() {
            self.task.abort();
        }
    }
}

/// A cancel of a goal that goes out when this guard is dropped armed, from
/// a task of its own: its caller stops following the goal before the goal
/// ended, also when the caller's future is dropped. [`Self::disarm`] keeps
/// the cancel from going out, once the caller read the end of the goal or
/// sent a cancel of its own.
pub struct CancelOnDrop {
    messenger: MessengerHandle,
    sender: ActionWireSender,
    goal_id: String,
    why: &'static str,
    armed: bool,
}

impl CancelOnDrop {
    /// Keeps the cancel from going out.
    pub fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        if self.armed {
            spawn_own_cancel(
                self.messenger.clone(),
                self.sender.clone(),
                std::mem::take(&mut self.goal_id),
                self.why,
            );
        }
    }
}

impl ActionGoalHandle {
    /// The wire sender used to dispatch this goal. Cloned by external wrappers
    /// (e.g. Python bindings) that need to issue cancel/result calls without
    /// holding a lock on the goal handle.
    pub fn sender(&self) -> &ActionWireSender {
        &self.sender
    }

    /// The decoded goal reply: the provider's answer (accepted with its
    /// declared response, or refused with a failure).
    pub fn goal_reply(&self) -> &ActionGoalReply {
        &self.goal_reply
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
    /// Feedback waits for this call in the goal's [`FeedbackBuffer`], which
    /// keeps every message up to the bound of unread messages that
    /// `send_goal` took, and always keeps the end of the stream.
    ///
    /// Returns `Err(Error::ActionFeedbackChannelClosed)` when the server
    /// publishes the end-of-stream sentinel: the framework emits it when
    /// the goal reaches its terminal state (the worker ends the goal, or
    /// drops it without a result).
    ///
    /// Returns `Err(Error::ActionFeedbackOverflow)` once this caller fell
    /// behind by more unread messages than its bound: the buffer dropped
    /// the unread messages, and the handle cancelled the goal (see
    /// [`Self::follow_feedback`]).
    ///
    /// Returns `Err(Error::ActionFeedbackProducerGone)` when the producer
    /// instance this goal is pinned to disappears without publishing the
    /// sentinel (process killed, OOM, runtime teardown losing the cleanup
    /// race). Buffered feedback (including a sentinel that did make it
    /// out) is always drained before the producer-gone error surfaces, so
    /// a graceful close keeps reporting `ActionFeedbackChannelClosed`.
    pub async fn on_next_feedback(&mut self) -> Result<Message> {
        self.follow_feedback();
        if self.feedback_overflow.is_raised() {
            return Err(self.overflow_error());
        }
        let msg = tokio::select! {
            biased;
            _ = self.feedback_overflow.raised() => {
                return Err(self.overflow_error());
            }
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
        self.follow_feedback();
        if self.feedback_overflow.is_raised() {
            return Err(self.overflow_error());
        }
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

    /// The caller reads the feedback, so it follows the goal: from now on
    /// the handle cancels the goal once the feedback overflows, at once,
    /// also when the caller is not reading at that moment. A caller that
    /// never reads the feedback keeps its goal: its buffer stops at its
    /// bound, and the result comes as usual. Outside a runtime, which has
    /// no task to watch the overflow from, the handle cancels nothing.
    fn follow_feedback(&mut self) {
        if self.overflow_cancel.is_some()
            || !self.goal_reply.is_accepted()
            || tokio::runtime::Handle::try_current().is_err()
        {
            return;
        }
        self.overflow_cancel = Some(OverflowCancel::spawn(
            self.messenger.clone(),
            self.sender.clone(),
            self.goal_id.clone(),
            self.feedback_overflow.clone(),
        ));
    }

    /// A guard that cancels this goal when it is dropped armed (see
    /// [`CancelOnDrop`]). `why` names the cause in the log of a cancel that
    /// fails.
    pub fn cancel_on_drop(&self, why: &'static str) -> CancelOnDrop {
        CancelOnDrop {
            messenger: self.messenger.clone(),
            sender: self.sender.clone(),
            goal_id: self.goal_id.clone(),
            why,
            armed: true,
        }
    }

    fn overflow_error(&self) -> Error {
        Error::ActionFeedbackOverflow {
            instance_id: self.sender.target_instance_id().map(str::to_string),
            action_name: self.sender.to_action_name().to_string(),
            bound: self.feedback_bound.get(),
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
/// is exactly the [`GoalOutcome::Abandoned`] contract (terminal, no result).
fn abandoned_reply(sender: &ActionWireSender) -> ActionResultReply {
    ActionResultReply {
        outcome: GoalOutcome::Abandoned,
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
    pub liveliness_token: LivelinessToken,
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
    /// action engine never runs**: this measures only the messaging/routing
    /// path. Clock-independent (single-clock round-trip).
    ///
    /// `request_size`/`response_size` make the probe carry a real-payload-sized
    /// goal body and ask the producer to reply with `response_size` bytes (the
    /// goal request/response sizes), so the round-trip reflects real-sized
    /// messages, still without creating a goal or running the action.
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
    /// The goal's feedback waits in a [`FeedbackBuffer`] until
    /// [`ActionGoalHandle::on_next_feedback`] takes it: every message, up to
    /// `feedback_bound` unread ones. The buffer never makes the session
    /// wait, so feedback left unread never holds up the goal's result or any
    /// other message to this session. A caller that reads the feedback and
    /// falls behind by more than its bound loses the stream: the handle
    /// cancels the goal, and the next read reports
    /// [`Error::ActionFeedbackOverflow`]. A caller that never reads the
    /// feedback keeps its goal.
    ///
    /// The goal carries the presence generation of this messenger as the
    /// sender `(as_core_node, as_instance_id)`: the provider cancels the
    /// goal once that sender is gone (see
    /// [`MessengerHandle::client_presence`]). When the goal request gets no
    /// answer this caller can read (the goal service times out, or its
    /// answer does not decode), the caller sends a cancel of the goal before
    /// it returns the error, so a goal the provider accepted does not run on
    /// with no handle.
    ///
    /// `to_target` must match the [`SenderTarget`] the action server used
    /// in [`Self::expose`].
    ///
    /// `target` is the producer's full `(core_node, instance_id)` wire
    /// address. `Some(target)` (a dep slot bound to exactly one
    /// producer, or an infra caller that already knows the full address)
    /// addresses that producer directly: **no discovery probe is issued
    /// and no discovery timeout applies**; the goal request has the
    /// caller's whole `goal_timeout` to itself.
    /// `None` is a genuine wildcard (core-node infra goals only;
    /// generated dep-slot call sites always pin): a discover-then-pin
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
        feedback_bound: FeedbackBound,
        goal_timeout: Duration,
    ) -> Result<ActionGoalHandle> {
        let goal_id = generate_goal_id();
        // The sender's presence token exists before its goal leaves, so the
        // provider can watch it.
        let presence = messenger
            .client_presence(as_core_node, as_instance_id)
            .await?;
        let goal_payload =
            wrap_goal_request(&goal_id, &presence.generation, user_payload.as_ref())?;

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
                discover_producer(messenger, &probe_sender.goal_service(), Some(goal_timeout))
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
        let feedback_buffer = FeedbackBuffer::new(feedback_bound);
        let feedback_overflow = feedback_buffer.overflow();
        let feedback_subscription = messenger
            .subscribe_action_feedback(&sender, &goal_id, feedback_buffer)
            .await?;

        // Watch the pinned producer's liveliness for the life of this goal,
        // so a producer that dies without publishing the end-of-stream
        // sentinel surfaces as `ActionFeedbackProducerGone` instead of a
        // feedback drain that blocks forever.
        let liveliness_watch = messenger.watch_action_producer(&sender).await?;
        let producer_gone =
            ProducerGoneWatch::spawn(liveliness_watch, messenger.clone(), sender.clone());

        let remaining_goal_budget =
            budget_left_after_discovery(goal_timeout, started_at, &resolved, to_action_name)?;
        // A goal whose answer this caller never reads may still run on the
        // provider, and no handle could cancel it: the caller sends the
        // cancel itself before it reports the error. A goal request that
        // reached no provider started no goal, and gets no cancel (see
        // [`request_may_have_started_a_goal`]).
        let cancel_unread_goal = || {
            spawn_own_cancel(
                messenger.clone(),
                sender.clone(),
                goal_id.clone(),
                "its caller did not read the answer of the goal",
            )
        };
        let goal_response = match messenger
            .poll_service(
                &sender.goal_service(),
                goal_payload,
                ServiceQueryKind::UserRequest,
                remaining_goal_budget,
            )
            .await
        {
            Ok(response) => response,
            Err(err) => {
                if request_may_have_started_a_goal(&err) {
                    cancel_unread_goal();
                }
                return Err(err);
            }
        };

        // Strip the admission ack here (in the messenger, mirroring how the
        // result path decodes its outcome envelope) so generated Rust and
        // Python read the same typed reply.
        let answer = match unwrap_goal_ack(&goal_response.payload().into_inner()) {
            Ok(answer) => answer,
            Err(err) => {
                cancel_unread_goal();
                return Err(err);
            }
        };
        let goal_reply = ActionGoalReply {
            answer,
            instance_id: goal_response.instance_id().to_string(),
            core_node: goal_response.core_node().to_string(),
        };
        Ok(ActionGoalHandle {
            messenger: messenger.clone(),
            sender,
            goal_id,
            goal_reply,
            feedback: Subscription::new(feedback_subscription),
            feedback_bound,
            feedback_overflow,
            overflow_cancel: None,
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

    /// [`request_result`](Self::request_result), reduced to the result of a
    /// completed goal: every other ending becomes a typed error rather than a
    /// payload the caller has to tell apart.
    ///
    /// Lives here rather than in each caller because the interesting part is
    /// the [`GoalOutcome`] match, not the request: `Abandoned` and `Expired`
    /// are easy to lump in with a transport error, and doing so would report
    /// "failed to get result" both for a worker that died and for one whose
    /// result simply aged out. Callers that need the result of a cancelled
    /// goal, or want the producer identity, keep using
    /// [`request_result`](Self::request_result).
    pub async fn request_result_body(
        messenger_handle: &MessengerHandle,
        action_handle: &ActionGoalHandle,
        result_timeout: Duration,
    ) -> Result<Payload> {
        let action_name = action_handle.sender.to_action_name().to_string();
        let reply = Self::request_result(messenger_handle, action_handle, result_timeout).await?;
        let instance_id = Some(reply.instance_id.clone()).filter(|id| !id.is_empty());
        match reply.outcome {
            GoalOutcome::Completed(result) => Ok(result),
            GoalOutcome::Cancelled { reason, .. } => Err(Error::ActionGoalCancelled {
                instance_id,
                action_name,
                reason,
            }),
            GoalOutcome::Failed(failure) => Err(Error::ActionGoalFailed {
                instance_id,
                action_name,
                failure,
            }),
            GoalOutcome::Abandoned => Err(Error::ActionGoalAbandoned {
                instance_id,
                action_name,
            }),
            GoalOutcome::Expired => Err(Error::ActionResultExpired {
                instance_id,
                action_name,
            }),
        }
    }

    /// Like [`request_result`](Self::request_result) but takes a cloned sender
    /// and `goal_id` directly. Mirrors [`cancel_with_sender`](Self::cancel_with_sender);
    /// the `goal_id` rides in the result request payload for server-side routing.
    ///
    /// Decodes the engine's result-outcome envelope into a typed
    /// [`ActionResultReply`], so callers (Rust generated code, the Python
    /// binding, and direct callers) never re-parse the framing.
    ///
    /// A result poll that fails unreachable / timed out is followed by a
    /// producer-disappearance probe (liveliness keyexpr check): when the
    /// targeted producer's token is gone, its retained results died with the
    /// process and the goal resolves to a typed
    /// [`GoalOutcome::Abandoned`] reply instead of the transport error.
    pub async fn request_result_with_sender(
        messenger_handle: &MessengerHandle,
        sender: &ActionWireSender,
        goal_id: &str,
        result_timeout: Duration,
    ) -> Result<ActionResultReply> {
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
                let err = Self::map_result_error(err, sender.to_action_name());
                return Self::resolve_failed_result_poll(messenger_handle, sender, err).await;
            }
        };
        Ok(ActionResultReply {
            outcome: unwrap_result_outcome(&message.payload().into_inner())?,
            instance_id: message.instance_id().to_string(),
            core_node: message.core_node().to_string(),
        })
    }

    /// Resolves a result poll that failed with `err`. A transport miss is
    /// followed by the producer-disappearance probe: a producer whose token
    /// is confirmed gone resolves the goal to a typed Abandoned reply, and
    /// every other failure stays `err`.
    async fn resolve_failed_result_poll(
        messenger_handle: &MessengerHandle,
        sender: &ActionWireSender,
        err: Error,
    ) -> Result<ActionResultReply> {
        let is_transport_miss = matches!(
            err,
            Error::ActionResultTimeout { .. } | Error::ActionResultUnreachable { .. }
        );
        if !is_transport_miss {
            return Err(err);
        }
        // An inconclusive probe (`Err`) must not fabricate an Abandoned
        // outcome: only a confirmed-absent token does.
        let is_alive = messenger_handle
            .probe_action_producer(sender, LIVELINESS_PROBE_TIMEOUT)
            .await
            .unwrap_or(true);
        if is_alive {
            return Err(err);
        }
        Ok(abandoned_reply(sender))
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
/// (`ActionCancelResponse.state`). There is no "no active goal" variant: an
/// unknown `goal_id` is [`Unknown`](CancelState::Unknown), and a goal that
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
/// per-action `CancelResponse` reader. See `schemas/action_cancel.capnp` and
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

/// Lifecycle state of a goal's result, owned by the engine. A goal is `Pending`
/// from accept until it reaches a terminal outcome; result polls park their
/// responders in `Pending` and are drained on the transition to `Terminal`.
enum GoalState {
    /// The worker is running. `waiters` is a `Vec`, not a single slot, because
    /// a relay may poll twice and several clients may race one `goal_id`.
    Pending { waiters: Vec<ServiceResponder> },
    /// Terminal: the result stays fetchable until `evict_at`, after which the
    /// retention sweeper removes the slot and records a tombstone so late polls
    /// resolve to [`GoalOutcome::Expired`].
    Terminal {
        outcome: GoalOutcome,
        evict_at: Instant,
    },
}

/// Why a goal received its cancel signal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CancelCause {
    /// A cancel request of the goal's caller.
    Caller,
    /// The sender of the goal is gone, and the sender grace of the action
    /// passed.
    SenderGone,
}

impl CancelCause {
    /// A reason for the cancelled result, in the words of the cause.
    pub fn reason(self) -> &'static str {
        match self {
            CancelCause::Caller => "cancelled by its caller",
            CancelCause::SenderGone => "its sender is gone",
        }
    }
}

/// Per-goal routing state held in the registry from accept until the retention
/// sweeper evicts the terminal slot. Cheaply cloneable (all shared state is
/// behind `Arc`), so the loops clone it out from under the `std` registry lock
/// and then work on it without holding that lock across `.await`.
#[derive(Clone)]
struct GoalSlot {
    cancel: CancellationToken,
    /// The cause of the first cancel signal; later ones keep it.
    cancel_cause: Arc<OnceLock<CancelCause>>,
    /// The tokens that the cancel signal cancels with `cancel`, each until
    /// its link drops (see [`GoalContext::cancel_at_signal`]).
    linked: Arc<StdMutex<LinkedTokens>>,
    state: Arc<TokioMutex<GoalState>>,
    /// Fast `is-terminal?` probe and the race guard that makes the first of
    /// `complete` / `fail` / `complete_cancelled` / abandon win.
    terminal: Arc<AtomicBool>,
}

/// The tokens linked to the cancel signal of one goal, each under the id of
/// its [`CancelLink`].
#[derive(Default)]
struct LinkedTokens {
    next_id: u64,
    tokens: Vec<(u64, CancellationToken)>,
}

impl GoalSlot {
    fn new() -> Self {
        Self {
            cancel: CancellationToken::new(),
            cancel_cause: Arc::new(OnceLock::new()),
            linked: Arc::new(StdMutex::new(LinkedTokens::default())),
            state: Arc::new(TokioMutex::new(GoalState::Pending {
                waiters: Vec::new(),
            })),
            terminal: Arc::new(AtomicBool::new(false)),
        }
    }

    fn is_terminal(&self) -> bool {
        self.terminal.load(Ordering::SeqCst)
    }

    /// Delivers the cancel signal for `cause`, unless the goal already
    /// received one, and cancels each token linked to it.
    fn signal_cancel(&self, cause: CancelCause) {
        let _ = self.cancel_cause.set(cause);
        self.cancel.cancel();
        // `link` reads `cancel` under this lock: a token linked meanwhile is
        // taken here, or `link` finds `cancel` cancelled and cancels it.
        let linked = std::mem::take(&mut self.linked.lock().unwrap().tokens);
        for (_, token) in linked {
            token.cancel();
        }
    }

    /// Links `token` to the cancel signal until the returned link drops; a
    /// goal that received its signal already cancels `token` at once.
    fn link(&self, token: CancellationToken) -> CancelLink {
        let mut linked = self.linked.lock().unwrap();
        let id = linked.next_id;
        linked.next_id += 1;
        if self.cancel.is_cancelled() {
            drop(linked);
            token.cancel();
        } else {
            linked.tokens.push((id, token));
        }
        CancelLink {
            linked: Arc::clone(&self.linked),
            id,
        }
    }

    fn cancel_cause(&self) -> Option<CancelCause> {
        self.cancel_cause.get().copied()
    }
}

/// The link of a token to the cancel signal of a goal, from
/// [`GoalContext::cancel_at_signal`]. The link ends when it drops: a later
/// signal leaves the token as it is.
#[must_use = "the link ends when it drops"]
pub struct CancelLink {
    linked: Arc<StdMutex<LinkedTokens>>,
    id: u64,
}

impl Drop for CancelLink {
    fn drop(&mut self) {
        self.linked
            .lock()
            .unwrap()
            .tokens
            .retain(|(id, _)| *id != self.id);
    }
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

/// `goal_id` → the instant a cancel request for a goal that is not registered
/// yet stops being remembered. A client whose goal request got no answer it
/// could read cancels the goal at once (see [`ActionMessenger::send_goal`]);
/// a provider that admits that goal later finds the cancel here at accept.
/// Lock discipline as for [`PendingWaiters`]: the registry may be acquired
/// while holding it, never the reverse.
type EarlyCancels = Arc<StdMutex<HashMap<String, Instant>>>;

/// Upper bound on the remembered early cancels, guarding against a flood of
/// cancels for goal ids that never arrive. Past it, a cancel for an unknown
/// goal is answered `Unknown` and forgotten.
const EARLY_CANCELS_CAP: usize = 1024;

/// Remembers a cancel for `goal_id`, which has no slot, unless `accept`
/// registered the goal meanwhile: the registry is read again under the
/// early-cancels lock, so the cancel either reaches the slot here or `accept`
/// finds it.
fn remember_early_cancel(
    goal_id: &str,
    registry: &GoalRegistry,
    early_cancels: &EarlyCancels,
) -> CancelState {
    let mut early = early_cancels.lock().unwrap();
    if let Some(slot) = registry.lock().unwrap().get(goal_id) {
        if slot.is_terminal() {
            return CancelState::AlreadyTerminal;
        }
        slot.signal_cancel(CancelCause::Caller);
        return CancelState::Signalled;
    }
    if early.len() < EARLY_CANCELS_CAP {
        early.insert(
            goal_id.to_string(),
            Instant::now() + PENDING_WAITER_MAX_PARK,
        );
    }
    CancelState::Unknown
}

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
/// [`GoalOutcome::Expired`] (and a cancel to [`CancelState::AlreadyTerminal`])
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

/// The live goals of one sender, and the task that watches its presence.
struct SenderWatch {
    goals: HashSet<String>,
    task: TaskHandle<()>,
}

/// Sender → the watch of its presence. A watch exists while its sender has a
/// goal that is not terminal: the retention sweeper ends a watch whose goals
/// all ended.
type SenderWatches = Arc<StdMutex<HashMap<ClientPresence, SenderWatch>>>;

/// The sender grace of an action: how long its engine keeps the goals of a
/// sender that is gone. Clones share one value, so a setter never waits for
/// the engine, even while the engine waits for its next goal. Each sender
/// watch reads it once its sender is confirmed gone.
#[derive(Debug, Clone, Default)]
pub struct SenderGrace(Arc<StdMutex<Duration>>);

impl SenderGrace {
    pub fn set(&self, grace: Duration) {
        *self.0.lock().unwrap() = grace;
    }

    pub fn get(&self) -> Duration {
        *self.0.lock().unwrap()
    }
}

/// Background task: watches the presence of one sender. Once the sender is
/// confirmed gone and the sender grace passed, it sends the cancel signal to
/// each goal of the sender that did not end. A sender whose token came back
/// during the grace, as after a router bounce, keeps its goals, and the
/// watch goes on.
async fn run_sender_watch(
    messenger: MessengerHandle,
    sender: ClientPresence,
    registry: GoalRegistry,
    senders: SenderWatches,
    sender_grace: SenderGrace,
) {
    // Removing the entry of the sender does not abort this task: a
    // `TaskHandle` drop detaches it.
    let forget_sender = || senders.lock().unwrap().remove(&sender);
    loop {
        let watch = match messenger.watch_client_presence(&sender).await {
            Ok(watch) => watch,
            Err(err) => {
                warn!(%err, ?sender, "cannot watch the presence of the sender of a goal");
                forget_sender();
                return;
            }
        };
        let probe = || messenger.probe_client_presence(&sender, LIVELINESS_PROBE_TIMEOUT);
        if !wait_until_confirmed_gone(watch, probe).await {
            // The watch closed: this engine's own session is going away.
            forget_sender();
            return;
        }
        let grace = sender_grace.get();
        if !grace.is_zero() {
            tokio::time::sleep(grace).await;
            if !matches!(probe().await, Ok(false)) {
                continue;
            }
        }
        let goals = forget_sender().map(|watch| watch.goals).unwrap_or_default();
        let slots: Vec<GoalSlot> = {
            let registry = registry.lock().unwrap();
            goals
                .iter()
                .filter_map(|goal_id| registry.get(goal_id).cloned())
                .collect()
        };
        for slot in slots.iter().filter(|slot| !slot.is_terminal()) {
            slot.signal_cancel(CancelCause::SenderGone);
        }
        return;
    }
}

/// Flip a goal's slot from `Pending` to `Terminal`, waking every parked result
/// poll exactly once with the encoded outcome. Returns `true` iff *this* call
/// performed the transition, so the caller closes the feedback stream once.
///
/// The first of `complete` / `fail` / `complete_cancelled` / abandon to flip
/// `terminal` wins; later calls are no-ops. Park (in `run_result_loop`) and drain here
/// serialize on the same per-slot `TokioMutex`, so no wakeup is ever lost.
/// `ServiceResponder::respond` is by-value, so each parked responder is consumed
/// exactly once.
async fn transition_terminal(slot: &GoalSlot, outcome: GoalOutcome, retention: Duration) -> bool {
    if slot.terminal.swap(true, Ordering::SeqCst) {
        return false;
    }
    let reply = wrap_result_outcome(&outcome);
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

/// The goal sub-service a background loop of [`ConcurrentAction`] serves.
#[derive(Clone, Copy)]
enum GoalRequestService {
    Cancel,
    Result,
}

/// Waits for the next request on `service` that carries a `goal_id`, and
/// returns that `goal_id` with the request's responder. A request without a
/// goal envelope is answered with an error and skipped. `None` ends the
/// calling loop: `stop` fired, the request stream closed, or receiving failed
/// (logged).
async fn next_goal_request(
    service: &mut ServiceEndpoint,
    stop: &CancellationToken,
    kind: GoalRequestService,
) -> Option<(String, ServiceResponder)> {
    loop {
        let next = tokio::select! {
            _ = stop.cancelled() => return None,
            next = service.recv_next_request() => next,
        };
        let (context, responder) = match next {
            Ok(Some(pair)) => pair,
            Ok(None) => return None,
            Err(err) => {
                match kind {
                    GoalRequestService::Cancel => warn!(%err, "action cancel loop stopped"),
                    GoalRequestService::Result => warn!(%err, "action result loop stopped"),
                }
                return None;
            }
        };

        let Ok(goal_id) = goal_id_from_request(context.message().payload_bytes().as_ref()) else {
            let message = match kind {
                GoalRequestService::Cancel => "malformed cancel request payload",
                GoalRequestService::Result => "malformed result request payload",
            };
            let _ = responder
                .respond_failure(Failure::new(PeppyFailureKind::Rejected, message))
                .await;
            continue;
        };
        return Some((goal_id, responder));
    }
}

/// Background loop: routes each incoming cancel request to the matching live
/// goal's [`CancellationToken`] and replies with a typed [`CancelState`]. There
/// is no "no active goal" reply: a live goal is `Signalled`, an
/// already-finished goal (still routable, or recently evicted) is
/// `AlreadyTerminal`, and anything else is `Unknown`. An `Unknown` cancel is
/// remembered for a while, so a goal that its provider admits after its
/// caller gave up on it starts cancelled.
async fn run_cancel_loop(
    mut cancel_service: ServiceEndpoint,
    registry: GoalRegistry,
    tombstones: Tombstones,
    early_cancels: EarlyCancels,
    stop: CancellationToken,
) {
    while let Some((goal_id, responder)) =
        next_goal_request(&mut cancel_service, &stop, GoalRequestService::Cancel).await
    {
        // Clone the token + terminal flag out under the lock, then fire + respond
        // without holding the registry lock across the network reply.
        let found = registry.lock().unwrap().get(&goal_id).cloned();
        let state = match found {
            Some(slot) if !slot.is_terminal() => {
                slot.signal_cancel(CancelCause::Caller);
                CancelState::Signalled
            }
            Some(_) => CancelState::AlreadyTerminal,
            None if tombstones.lock().unwrap().contains(&goal_id) => CancelState::AlreadyTerminal,
            None => remember_early_cancel(&goal_id, &registry, &early_cancels),
        };

        match encode_cancel_ack(state) {
            Ok(payload) => {
                let _ = responder.respond(payload).await;
            }
            Err(err) => {
                let _ = responder
                    .respond_failure(Failure::new(
                        PeppyFailureKind::AttemptFailed,
                        err.to_string(),
                    ))
                    .await;
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
            Some((responder, wrap_result_outcome(outcome)))
        }
        GoalState::Terminal { .. } => Some((responder, wrap_result_outcome(&GoalOutcome::Expired))),
        GoalState::Pending { waiters } => {
            waiters.push(responder);
            None
        }
    }
}

/// Where [`route_result_poll`] sends a result poll.
enum ResultPollRoute {
    /// The goal has a slot: [`act_on_slot`] answers or parks the poll.
    Slot(GoalSlot, ServiceResponder),
    /// The goal was evicted: the poll gets `Expired` now.
    Expired(ServiceResponder),
    /// The poll waits in `PendingWaiters` until `accept` adopts it, or was
    /// dropped because they are full.
    ParkedOrDropped,
}

/// Routes a result poll for `goal_id` under the `std` locks. It is
/// synchronous, so the caller can only await on the route after every `std`
/// lock is released.
fn route_result_poll(
    goal_id: &str,
    responder: ServiceResponder,
    registry: &GoalRegistry,
    pending_waiters: &PendingWaiters,
    tombstones: &Tombstones,
) -> ResultPollRoute {
    // Clone the slot out under the std lock (dropped at the `;`): the registry
    // lock is released before `pending_waiters` is taken below.
    let slot = registry.lock().unwrap().get(goal_id).cloned();
    if let Some(slot) = slot {
        return ResultPollRoute::Slot(slot, responder);
    }

    // No slot: the poll beat `accept`, is for an evicted goal (tombstone), or
    // for a never-existed goal_id. Decide while holding `pending_waiters`,
    // re-reading the registry under it so a concurrent `accept` that just
    // inserted the slot (and whose adoption pass therefore missed this poll)
    // cannot orphan it.
    let mut pending = pending_waiters.lock().unwrap();
    let slot_now = registry.lock().unwrap().get(goal_id).cloned();
    if let Some(slot) = slot_now {
        return ResultPollRoute::Slot(slot, responder);
    }
    if tombstones.lock().unwrap().contains(goal_id) {
        return ResultPollRoute::Expired(responder);
    }
    let total: usize = pending.values().map(Vec::len).sum();
    if total >= PENDING_WAITERS_CAP {
        // Overflow: drop the responder; the client falls back to its own
        // timeout.
        return ResultPollRoute::ParkedOrDropped;
    }
    pending
        .entry(goal_id.to_string())
        .or_default()
        .push(PendingWaiter {
            responder,
            deadline: Instant::now() + PENDING_WAITER_MAX_PARK,
        });
    ResultPollRoute::ParkedOrDropped
}

/// Background loop: routes each incoming result request to the matching goal.
/// A `Pending` goal parks the responder until it reaches a terminal state; a
/// `Terminal` goal replies inline with its typed outcome; a poll that arrived
/// before `accept` parks in `PendingWaiters` to be adopted at registration; a
/// poll for an evicted goal replies `Expired`. There is no "no active goal"
/// reply: a poll always parks for a definitive answer or resolves to a typed
/// terminal outcome.
async fn run_result_loop(
    mut result_service: ServiceEndpoint,
    registry: GoalRegistry,
    pending_waiters: PendingWaiters,
    tombstones: Tombstones,
    stop: CancellationToken,
) {
    while let Some((goal_id, responder)) =
        next_goal_request(&mut result_service, &stop, GoalRequestService::Result).await
    {
        let route = route_result_poll(
            &goal_id,
            responder,
            &registry,
            &pending_waiters,
            &tombstones,
        );
        let act = match route {
            ResultPollRoute::Slot(slot, responder) => act_on_slot(&slot, responder).await,
            ResultPollRoute::Expired(responder) => {
                Some((responder, wrap_result_outcome(&GoalOutcome::Expired)))
            }
            ResultPollRoute::ParkedOrDropped => None,
        };

        if let Some((responder, payload)) = act {
            let _ = responder.respond(payload).await;
        }
    }
}

/// Background loop: evicts terminal slots past their retention window (recording
/// a tombstone so late polls still resolve to `Expired`), drops parked polls
/// whose deadline has passed, and ends the sender watches whose goals all
/// ended.
async fn run_retention_sweeper(
    registry: GoalRegistry,
    pending_waiters: PendingWaiters,
    tombstones: Tombstones,
    senders: SenderWatches,
    early_cancels: EarlyCancels,
    stop: CancellationToken,
) {
    loop {
        tokio::select! {
            _ = stop.cancelled() => return,
            _ = tokio::time::sleep(SWEEP_INTERVAL) => {}
        }
        let now = Instant::now();
        sweep_once(&registry, &pending_waiters, &tombstones, now);
        end_idle_sender_watches(&registry, &senders);
        early_cancels
            .lock()
            .unwrap()
            .retain(|_, forget_at| *forget_at > now);
    }
}

/// Ends the watch of each sender whose goals all ended: a terminal or
/// evicted goal needs no cancel when its sender goes. The senders lock is
/// held while the registry is read, so a goal that `accept` tracks during
/// the pass either joins a watch this pass keeps or starts a watch of its
/// own after it.
fn end_idle_sender_watches(registry: &GoalRegistry, senders: &SenderWatches) {
    let mut senders = senders.lock().unwrap();
    let registry = registry.lock().unwrap();
    senders.retain(|_, watch| {
        watch.goals.retain(|goal_id| {
            registry
                .get(goal_id)
                .is_some_and(|slot| !slot.is_terminal())
        });
        if watch.goals.is_empty() {
            watch.task.abort();
            return false;
        }
        true
    });
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
/// [`PendingGoal::accept`]/[`PendingGoal::refuse`], so server behavior is
/// identical across languages.
///
/// The engine watches the presence of the sender of each goal. Once a
/// sender is gone and the sender grace of the action passed (zero unless
/// [`Self::set_sender_grace`] sets it), each of its goals that did not end
/// receives its cancel signal, with the cause [`CancelCause::SenderGone`].
pub struct ConcurrentAction {
    goal_service: ServiceEndpoint,
    factory: ActionFeedbackPublisherFactory,
    registry: GoalRegistry,
    pending_waiters: PendingWaiters,
    early_cancels: EarlyCancels,
    senders: SenderWatches,
    sender_grace: SenderGrace,
    has_feedback: bool,
    result_retention_grace: Duration,
    stop: CancellationToken,
    cancel_loop: TaskHandle<()>,
    result_loop: TaskHandle<()>,
    sweeper_loop: TaskHandle<()>,
    /// Producer-instance liveliness advertisement, held so consumers see
    /// this producer as alive for exactly as long as the engine can route
    /// goals/cancels/results.
    _liveliness_token: LivelinessToken,
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
        let senders: SenderWatches = Arc::new(StdMutex::new(HashMap::new()));
        let early_cancels: EarlyCancels = Arc::new(StdMutex::new(HashMap::new()));
        let stop = CancellationToken::new();
        let cancel_loop = spawn(run_cancel_loop(
            cancel_service,
            Arc::clone(&registry),
            Arc::clone(&tombstones),
            Arc::clone(&early_cancels),
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
            Arc::clone(&senders),
            Arc::clone(&early_cancels),
            stop.clone(),
        ));
        Self {
            goal_service,
            factory: feedback_publisher_factory,
            registry,
            pending_waiters,
            early_cancels,
            senders,
            sender_grace: SenderGrace::default(),
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

    /// Set the sender grace of the action: how long the engine keeps the
    /// goals of a sender that is gone before it sends them their cancel
    /// signal. Zero by default. It applies to each sender that is confirmed
    /// gone after the call.
    pub fn set_sender_grace(&self, grace: Duration) {
        self.sender_grace.set(grace);
    }

    /// The sender grace of the action, shared with the engine: a binding that
    /// holds the engine behind a lock sets the grace through it without that
    /// lock.
    pub fn sender_grace(&self) -> SenderGrace {
        self.sender_grace.clone()
    }

    /// Wait for the next goal request. Returns a [`PendingGoal`] the caller
    /// inspects and then [`accept`](PendingGoal::accept)s or
    /// [`refuse`](PendingGoal::refuse)s. Returns `Ok(None)` when the goal
    /// service stream has closed.
    ///
    /// A request that is not a well-formed goal request is refused with
    /// `rejected`, and a goal whose feedback publisher cannot be declared is
    /// refused with `attempt_failed`; the engine then waits for the next
    /// request, so one bad request never ends the caller's goal loop.
    pub async fn recv_next_goal(&mut self) -> Result<Option<PendingGoal>> {
        loop {
            let Some((context, responder)) = self.goal_service.recv_next_request().await? else {
                return Ok(None);
            };
            let (request, sender) = match parse_goal_request(&context) {
                Ok(parsed) => parsed,
                Err(err) => {
                    refuse_unadmitted_goal(responder, PeppyFailureKind::Rejected, err).await;
                    continue;
                }
            };
            let feedback = if self.has_feedback {
                match self
                    .factory
                    .declare_for_goal(context.link_id(), &request.goal_id)
                    .await
                {
                    Ok(publisher) => Some(publisher),
                    Err(err) => {
                        refuse_unadmitted_goal(responder, PeppyFailureKind::AttemptFailed, err)
                            .await;
                        continue;
                    }
                }
            } else {
                None
            };
            return Ok(Some(
                self.pending_goal(request, sender, responder, feedback),
            ));
        }
    }

    fn pending_goal(
        &self,
        request: GoalRequestEnvelope,
        sender: ClientPresence,
        responder: ServiceResponder,
        feedback: Option<ActionFeedbackPublisher>,
    ) -> PendingGoal {
        PendingGoal {
            goal_id: request.goal_id,
            sender,
            request_bytes: request.user_payload,
            responder,
            feedback,
            engine: EngineShared {
                messenger: self.factory.messenger().clone(),
                registry: Arc::clone(&self.registry),
                pending_waiters: Arc::clone(&self.pending_waiters),
                early_cancels: Arc::clone(&self.early_cancels),
                senders: Arc::clone(&self.senders),
                sender_grace: self.sender_grace.clone(),
                result_retention_grace: self.result_retention_grace,
            },
        }
    }
}

/// The goal envelope and the sender of a goal request. A goal id that is
/// not a single safe segment is refused here, so it never reaches a feedback
/// topic.
fn parse_goal_request(
    context: &super::ServiceRequestContext,
) -> Result<(GoalRequestEnvelope, ClientPresence)> {
    let wire = context.message().payload().into_inner();
    let request = split_goal_request(&wire)?;
    if !is_safe_goal_id(&request.goal_id) {
        return Err(envelope_error(format!(
            "goal_id contains unsafe characters: {:?}",
            request.goal_id
        )));
    }
    let segment = |value: &str| -> Result<Segment> {
        Segment::try_from(value).map_err(|err| pmi::SenderTargetError::from(err).into())
    };
    let sender = ClientPresence {
        core_node: segment(context.message().core_node())?,
        instance_id: segment(context.message().instance_id())?,
        generation: segment(&request.sender_generation)?,
    };
    Ok((request, sender))
}

/// Refuses a goal request that the engine cannot admit, and logs why.
async fn refuse_unadmitted_goal(responder: ServiceResponder, kind: PeppyFailureKind, err: Error) {
    warn!(%err, "refusing a goal request the engine cannot admit");
    let failure = Failure::new(kind, format!("the goal request cannot be admitted: {err}"));
    if let Err(err) = responder.respond(wrap_goal_refused(&failure)).await {
        warn!(%err, "the refusal of a goal request did not reach its sender");
    }
}

impl Drop for ConcurrentAction {
    fn drop(&mut self) {
        self.stop.cancel();
        self.cancel_loop.abort();
        self.result_loop.abort();
        self.sweeper_loop.abort();
        for (_, watch) in self.senders.lock().unwrap().drain() {
            watch.task.abort();
        }
    }
}

/// The engine state a [`PendingGoal`] needs to register its goal.
struct EngineShared {
    messenger: MessengerHandle,
    registry: GoalRegistry,
    pending_waiters: PendingWaiters,
    early_cancels: EarlyCancels,
    senders: SenderWatches,
    sender_grace: SenderGrace,
    result_retention_grace: Duration,
}

impl EngineShared {
    /// Records `goal_id` as a goal of `sender`, and starts the watch of the
    /// sender's presence at its first goal.
    fn track_sender_goal(&self, sender: &ClientPresence, goal_id: &str) {
        let mut senders = self.senders.lock().unwrap();
        if let Some(watch) = senders.get_mut(sender) {
            watch.goals.insert(goal_id.to_string());
            return;
        }
        let task = spawn(run_sender_watch(
            self.messenger.clone(),
            sender.clone(),
            Arc::clone(&self.registry),
            Arc::clone(&self.senders),
            self.sender_grace.clone(),
        ));
        senders.insert(
            sender.clone(),
            SenderWatch {
                goals: HashSet::from([goal_id.to_string()]),
                task,
            },
        );
    }
}

/// A goal that has been received but not yet accepted or refused. The caller
/// decodes [`request_bytes`](Self::request_bytes), decides (this is where
/// per-resource concurrency limits are enforced), and calls
/// [`accept`](Self::accept) with the encoded declared `GoalResponse` payload
/// (empty when the action declares none), or [`refuse`](Self::refuse) with a
/// failure. Both wrap the reply in the framework admission ack (see
/// [`wrap_goal_accepted`] and [`wrap_goal_refused`]) before it reaches the wire.
pub struct PendingGoal {
    goal_id: String,
    sender: ClientPresence,
    request_bytes: Bytes,
    responder: ServiceResponder,
    feedback: Option<ActionFeedbackPublisher>,
    engine: EngineShared,
}

impl PendingGoal {
    /// The client-generated correlation id for this goal.
    pub fn goal_id(&self) -> &str {
        &self.goal_id
    }

    /// The core node of the client that sent this goal.
    pub fn core_node(&self) -> &str {
        self.sender.core_node.as_str()
    }

    /// The instance id of the client that sent this goal.
    pub fn instance_id(&self) -> &str {
        self.sender.instance_id.as_str()
    }

    /// The process that sent this goal.
    pub fn sender(&self) -> &ClientPresence {
        &self.sender
    }

    /// The envelope-stripped goal request payload, ready to decode.
    pub fn request_bytes(&self) -> &[u8] {
        self.request_bytes.as_ref()
    }

    /// Accept the goal: register it for cancel/result routing and for the
    /// watch of its sender, reply to the client with `response` wrapped in an
    /// accepted admission ack, and hand back the [`GoalContext`] that drives
    /// it to its end. The slot is registered **before** the reply is sent so
    /// a cancel/result request the client fires immediately after
    /// `fire_goal` returns always finds the goal.
    pub async fn accept(self, response: Payload) -> Result<GoalContext> {
        let response = wrap_goal_accepted(response.as_ref());
        let slot = GoalSlot::new();
        self.engine
            .registry
            .lock()
            .unwrap()
            .insert(self.goal_id.clone(), slot.clone());

        // A cancel that arrived before this slot existed applies now: its
        // caller gave up on the goal before it read the answer. The cancel
        // loop reads the registry again under this lock, so no cancel falls
        // between the two.
        if self
            .engine
            .early_cancels
            .lock()
            .unwrap()
            .remove(&self.goal_id)
            .is_some()
        {
            slot.signal_cancel(CancelCause::Caller);
        }

        // Adopt any result polls that parked before this slot existed. The
        // registry insert above is visible to a concurrent `run_result_loop`
        // before this drain, and that loop re-reads the registry under the
        // `pending_waiters` lock, so an early poll either parked here (and we
        // adopt it now) or sees the slot and parks in it directly. Never orphaned.
        let adopted = self
            .engine
            .pending_waiters
            .lock()
            .unwrap()
            .remove(&self.goal_id);
        if let Some(adopted) = adopted
            && let GoalState::Pending { waiters } = &mut *slot.state.lock().await
        {
            waiters.extend(adopted.into_iter().map(|p| p.responder));
        }

        if let Err(err) = self.responder.respond(response).await {
            // Reply failed (client gone): don't leak the just-registered slot.
            // Any adopted waiters are dropped with it (their polls close cleanly).
            self.engine.registry.lock().unwrap().remove(&self.goal_id);
            return Err(err);
        }
        self.engine.track_sender_goal(&self.sender, &self.goal_id);

        Ok(GoalContext {
            goal_id: self.goal_id,
            sender: self.sender,
            request_bytes: self.request_bytes,
            slot,
            feedback: self.feedback,
            result_retention_grace: self.engine.result_retention_grace,
            // `accept` is always awaited on the runtime, so a handle is
            // available here; `Drop` reuses it to spawn cleanup from any thread.
            runtime: tokio::runtime::Handle::current(),
            close_on_drop: AtomicBool::new(true),
        })
    }

    /// Refuse the goal: reply with `failure` in a refused admission ack, and
    /// register nothing. No [`GoalContext`] is produced, so the goal cannot
    /// be cancelled or ended.
    pub async fn refuse(self, failure: Failure) -> Result<()> {
        self.responder.respond(wrap_goal_refused(&failure)).await
    }
}

/// The per-goal handle owned by user code for the life of an accepted goal.
/// Carries the decoded request bytes, the per-goal feedback publisher, and the
/// shared goal slot (cancel signal + result-delivery state). Cheaply movable
/// into a spawned task.
pub struct GoalContext {
    goal_id: String,
    sender: ClientPresence,
    request_bytes: Bytes,
    /// The shared slot this context drives. `complete` / `fail` /
    /// `complete_cancelled` / `Drop` transition it to a terminal state; the
    /// engine's retention sweeper owns its eventual eviction, so this context
    /// never touches the registry.
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
    /// Whether `Drop` performs its abandon-on-drop cleanup. Always `true` in
    /// production; cleared only through the `testing`-gated
    /// [`disarm_close_on_drop`](Self::disarm_close_on_drop). The field itself
    /// is unconditional (a feature-dependent struct layout would be worse);
    /// only the setter is gated.
    close_on_drop: AtomicBool,
}

impl GoalContext {
    /// The client-generated correlation id for this goal.
    pub fn goal_id(&self) -> &str {
        &self.goal_id
    }

    /// The process that sent this goal.
    pub fn sender(&self) -> &ClientPresence {
        &self.sender
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

    /// Resolves when this goal receives its cancel signal: a cancel request
    /// of its caller, or the end of its sender (see [`Self::cancel_cause`]).
    /// Pair it with the goal's work in a `select!` and react by calling
    /// [`complete_cancelled`](Self::complete_cancelled).
    pub async fn cancel_signal(&self) {
        self.slot.cancel.cancelled().await;
    }

    /// Whether this goal received its cancel signal.
    pub fn is_cancelled(&self) -> bool {
        self.slot.cancel.is_cancelled()
    }

    /// A token that the cancel signal of this goal cancels, at the signal and
    /// before the engine answers the cancel request. Hand it to work that
    /// runs off the goal's task, such as a blocking thread, which then sees
    /// the cancel at its next check.
    pub fn cancellation(&self) -> CancellationToken {
        self.slot.cancel.child_token()
    }

    /// Links `token` to the cancel signal of this goal until the returned
    /// link drops: the signal cancels `token` as it cancels
    /// [`Self::cancellation`], at the signal and before the engine answers
    /// the cancel request, and a goal that received its signal already
    /// cancels `token` at once. `token` keeps its own parent, so work whose
    /// token another source cancels too reads both causes from it, the
    /// reads of a poll in progress at the answer included.
    pub fn cancel_at_signal(&self, token: impl Into<CancellationToken>) -> CancelLink {
        self.slot.link(token.into())
    }

    /// Why this goal received its cancel signal; `None` before it did.
    pub fn cancel_cause(&self) -> Option<CancelCause> {
        self.slot.cancel_cause()
    }

    /// End the goal with the status completed and its result: the goal is a
    /// success. Idempotent: the first terminal transition (`complete` /
    /// `fail` / `complete_cancelled` / abandon-on-drop) wins; later calls are
    /// no-ops.
    pub async fn complete(&self, result: Payload) -> Result<()> {
        self.deliver(GoalOutcome::Completed(result)).await
    }

    /// End the goal with the status failed: a failure kind and a message,
    /// and no result. Equally idempotent.
    pub async fn fail(&self, failure: Failure) -> Result<()> {
        self.deliver(GoalOutcome::Failed(failure)).await
    }

    /// End the goal with the status cancelled, for `reason`, with its result.
    /// A worker calls it after a cancel signal, and a member that ends the
    /// goals of other clients (such as a stop) calls it with its own reason.
    /// Equally idempotent.
    pub async fn complete_cancelled(
        &self,
        reason: impl Into<String>,
        result: Payload,
    ) -> Result<()> {
        self.deliver(GoalOutcome::Cancelled {
            reason: reason.into(),
            result,
        })
        .await
    }

    /// Disarm the drop-time close: after this call, dropping the context
    /// performs no `Abandoned` transition and emits no feedback end sentinel.
    ///
    /// This exists for exactly one consumer: mock action servers
    /// ([`crate::testing::MockActionServerCore`]) simulating producer loss.
    /// Stopping a mock mid-goal must let the consumer observe the producer's
    /// liveliness token going absent
    /// ([`crate::PeppyError::ActionFeedbackProducerGone`]); an armed drop
    /// would race a clean feedback-end sentinel against that latch (the
    /// consumer's feedback drain polls `biased`), making producer-loss
    /// nondeterministic. Gated behind the `testing` feature on purpose: a
    /// production node calling this would silently break the `Abandoned`
    /// transition its consumers rely on.
    #[cfg(feature = "testing")]
    pub fn disarm_close_on_drop(&self) {
        self.close_on_drop.store(false, Ordering::SeqCst);
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
/// resolves to [`GoalOutcome::Expired`]. Overridable per-server via
/// [`ConcurrentAction::with_result_retention_grace`].
const RESULT_RETENTION_GRACE: Duration = Duration::from_secs(30);

impl Drop for GoalContext {
    fn drop(&mut self) {
        // Disarmed by `disarm_close_on_drop` (testing only): drop silently, so
        // a stopped mock's producer-loss signal is not raced by a clean close.
        if !self.close_on_drop.load(Ordering::SeqCst) {
            return;
        }
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
        // via Python's GC). The slot is NOT removed here (the sweeper owns
        // eviction), so a poll that arrives between this spawn and its execution
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

    /// Asserts that `result` is the `InternalEncodingError` of the envelope
    /// `expected_identifier`, with a reason that contains
    /// `expected_reason_fragment`.
    fn assert_encoding_error<T>(
        result: Result<T>,
        expected_identifier: &str,
        expected_reason_fragment: &str,
    ) {
        match result {
            Err(Error::InternalEncodingError { identifier, reason }) => {
                assert_eq!(identifier, expected_identifier);
                assert!(
                    reason.contains(expected_reason_fragment),
                    "reason {reason:?} should contain {expected_reason_fragment:?}"
                );
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

    fn failure(kind: &str, message: &str) -> Failure {
        Failure::new(
            config::failure_kind::FailureKindName::parse(kind).unwrap(),
            message,
        )
    }

    fn roundtrip_outcome(outcome: GoalOutcome) -> GoalOutcome {
        let wire = wrap_result_outcome(&outcome).into_inner();
        unwrap_result_outcome(&wire).expect("unwrap outcome")
    }

    #[test]
    fn result_outcome_roundtrips_each_ending() {
        for outcome in [
            GoalOutcome::Completed(Payload::from_static(b"payload")),
            GoalOutcome::Completed(Payload::new()),
            GoalOutcome::Cancelled {
                reason: "stopped: the operator pressed stop".to_string(),
                result: Payload::from_static(b"measured"),
            },
            GoalOutcome::Cancelled {
                reason: String::new(),
                result: Payload::new(),
            },
            GoalOutcome::Failed(failure("unreachable", "no plan to the target")),
            GoalOutcome::Failed(Failure::new(PeppyFailureKind::Timeout, "")),
            GoalOutcome::Abandoned,
            GoalOutcome::Expired,
        ] {
            assert_eq!(roundtrip_outcome(outcome.clone()), outcome);
        }
    }

    #[test]
    fn each_ending_carries_its_status_tag() {
        for (outcome, status) in [
            (
                GoalOutcome::Completed(Payload::new()),
                ResultStatus::Completed,
            ),
            (
                GoalOutcome::Cancelled {
                    reason: "r".to_string(),
                    result: Payload::new(),
                },
                ResultStatus::Cancelled,
            ),
            (GoalOutcome::Abandoned, ResultStatus::Abandoned),
            (GoalOutcome::Expired, ResultStatus::Expired),
            (
                GoalOutcome::Failed(Failure::new(PeppyFailureKind::Rejected, "")),
                ResultStatus::Failed,
            ),
        ] {
            assert_eq!(outcome.status(), status);
            assert_eq!(wrap_result_outcome(&outcome).as_ref()[0], status as u8);
        }
    }

    /// A failed goal gives a failure and no result: its wire form is the tag
    /// and the failure, nothing else.
    #[test]
    fn a_failed_outcome_carries_the_failure_and_no_result() {
        let wire = wrap_result_outcome(&GoalOutcome::Failed(failure("not_found", "no red cup")));
        assert_eq!(wire.as_ref(), b"\x04\x09not_foundno red cup");
    }

    fn assert_result_outcome_unwrap_error(wire: &'static [u8], expected_reason_fragment: &str) {
        assert_encoding_error(
            unwrap_result_outcome(&Bytes::from_static(wire)),
            "action_result_outcome_envelope",
            expected_reason_fragment,
        );
    }

    #[test]
    fn unwrap_result_outcome_rejects_a_cancelled_result_too_short_for_its_reason_length() {
        assert_result_outcome_unwrap_error(&[0x01, 0, 0], "too short for its reason");
    }

    #[test]
    fn unwrap_result_outcome_rejects_a_truncated_reason() {
        // Declares a 4-byte reason but only provides 1 byte.
        assert_result_outcome_unwrap_error(&[0x01, 0, 0, 0, 4, b'r'], "reason of 4 bytes");
    }

    #[test]
    fn unwrap_result_outcome_rejects_a_non_utf8_reason() {
        assert_result_outcome_unwrap_error(&[0x01, 0, 0, 0, 2, 0xFF, 0xFE], "not valid UTF-8");
    }

    #[test]
    fn unwrap_result_outcome_rejects_a_failed_result_with_an_invalid_kind() {
        assert_result_outcome_unwrap_error(b"\x04\x07Timeout", "invalid failure kind name");
        assert_result_outcome_unwrap_error(&[0x04], "the failure is empty");
    }

    #[test]
    fn goal_ack_roundtrips_an_accepted_goal_with_its_response() {
        let wire = wrap_goal_accepted(b"response").into_inner();
        assert_eq!(
            unwrap_goal_ack(&wire).expect("unwrap ack"),
            GoalAnswer::Accepted(Payload::from_static(b"response"))
        );
    }

    #[test]
    fn goal_ack_roundtrips_an_accepted_goal_with_no_response() {
        let wire = wrap_goal_accepted(b"").into_inner();
        assert_eq!(
            unwrap_goal_ack(&wire).expect("unwrap ack"),
            GoalAnswer::Accepted(Payload::new())
        );
    }

    #[test]
    fn goal_ack_roundtrips_a_refused_goal_with_its_failure() {
        let refusal = failure("leader_streaming", "a leader streams left_arm");
        let wire = wrap_goal_refused(&refusal).into_inner();
        assert_eq!(
            unwrap_goal_ack(&wire).expect("unwrap ack"),
            GoalAnswer::Refused(refusal)
        );
    }

    #[test]
    fn a_goal_reply_tells_an_acceptance_from_a_refusal() {
        let refusal = Failure::new(PeppyFailureKind::Rejected, "busy");
        let refused = ActionGoalReply {
            answer: GoalAnswer::Refused(refusal.clone()),
            instance_id: "p".to_string(),
            core_node: "c".to_string(),
        };
        assert!(!refused.is_accepted());
        assert_eq!(refused.refusal(), Some(&refusal));
        let accepted = ActionGoalReply {
            answer: GoalAnswer::Accepted(Payload::new()),
            instance_id: "p".to_string(),
            core_node: "c".to_string(),
        };
        assert!(accepted.is_accepted());
        assert_eq!(accepted.refusal(), None);
    }

    fn assert_goal_ack_unwrap_error(wire: &'static [u8], expected_reason_fragment: &str) {
        assert_encoding_error(
            unwrap_goal_ack(&Bytes::from_static(wire)),
            "action_goal_ack_envelope",
            expected_reason_fragment,
        );
    }

    #[test]
    fn unwrap_goal_ack_rejects_empty_wire() {
        assert_goal_ack_unwrap_error(&[], "wire payload is empty");
    }

    #[test]
    fn unwrap_goal_ack_rejects_unknown_tag() {
        assert_goal_ack_unwrap_error(&[0x02, 0, 0], "unknown accepted tag");
    }

    #[test]
    fn unwrap_goal_ack_rejects_a_refusal_with_no_failure() {
        assert_goal_ack_unwrap_error(&[0x00], "the failure is empty");
    }

    #[test]
    fn unwrap_goal_ack_rejects_a_refusal_with_a_truncated_kind() {
        assert_goal_ack_unwrap_error(&[0x00, 9, b'r'], "too short for its kind");
    }

    #[test]
    fn unwrap_result_outcome_rejects_empty_wire() {
        assert_encoding_error(
            unwrap_result_outcome(&Bytes::new()),
            "action_result_outcome_envelope",
            "wire payload is empty",
        );
    }

    #[test]
    fn unwrap_result_outcome_rejects_unknown_tag() {
        assert_result_outcome_unwrap_error(&[0xFF, 1, 2, 3], "unknown result status tag");
    }

    #[test]
    fn a_goal_request_carries_the_generation_of_its_sender() {
        let wire = wrap_goal_request("goal-abc", "gen42", b"hello")
            .expect("wrap should succeed")
            .into_inner();
        let request = split_goal_request(&wire).expect("split should succeed");
        assert_eq!(request.goal_id, "goal-abc");
        assert_eq!(request.sender_generation, "gen42");
        assert_eq!(request.user_payload.as_ref(), b"hello");
    }

    #[test]
    fn a_goal_request_needs_a_sender_generation() {
        assert_encoding_error(
            wrap_goal_request("goal-abc", "", b"hello"),
            "action_goal_envelope",
            "sender generation length 0",
        );
        let oversized = "g".repeat(256);
        assert_encoding_error(
            wrap_goal_request("goal-abc", &oversized, b""),
            "action_goal_envelope",
            "sender generation length 256",
        );
    }

    #[test]
    fn split_goal_request_rejects_a_request_without_its_generation() {
        // A goal envelope whose body is empty: no generation length.
        let wire = wrap_goal_payload("goal-abc", b"").unwrap().into_inner();
        assert_encoding_error(
            split_goal_request(&wire),
            "action_goal_envelope",
            "no sender generation",
        );
        // A generation that declares 5 bytes but holds 2.
        let wire = wrap_goal_payload("goal-abc", &[5, b'g', b'e'])
            .unwrap()
            .into_inner();
        assert_encoding_error(
            split_goal_request(&wire),
            "action_goal_envelope",
            "too short for its sender generation",
        );
    }

    #[test]
    fn wrap_rejects_empty_goal_id() {
        assert_encoding_error(
            wrap_goal_payload("", b"payload"),
            "action_goal_envelope",
            "goal_id must be non-empty",
        );
    }

    #[test]
    fn wrap_rejects_goal_id_over_255_bytes() {
        let oversized = "a".repeat(256);
        assert_encoding_error(
            wrap_goal_payload(&oversized, b"payload"),
            "action_goal_envelope",
            "exceeds wire limit",
        );
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
        assert_encoding_error(
            unwrap_goal_payload(&[]),
            "action_goal_envelope",
            "wire payload is empty",
        );
    }

    #[test]
    fn unwrap_rejects_zero_length_prefix() {
        assert_encoding_error(
            unwrap_goal_payload(&[0x00]),
            "action_goal_envelope",
            "goal_id is empty",
        );
    }

    #[test]
    fn unwrap_rejects_truncated_wire() {
        // Declares a 5-byte goal_id but only provides 1 byte after the length prefix.
        assert_encoding_error(
            unwrap_goal_payload(&[0x05, b'a']),
            "action_goal_envelope",
            "too short for declared goal_id_len",
        );
    }

    #[test]
    fn unwrap_rejects_non_utf8_goal_id() {
        // 0xFF / 0xFE form an invalid UTF-8 sequence.
        assert_encoding_error(
            unwrap_goal_payload(&[0x02, 0xFF, 0xFE, b'p']),
            "action_goal_envelope",
            "not valid UTF-8",
        );
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
            state: Arc::new(TokioMutex::new(GoalState::Terminal {
                outcome: GoalOutcome::Abandoned,
                evict_at,
            })),
            terminal: Arc::new(AtomicBool::new(true)),
            ..GoalSlot::new()
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

    fn client(instance_id: &str) -> ClientPresence {
        ClientPresence {
            core_node: Segment::try_from("core").unwrap(),
            instance_id: Segment::try_from(instance_id).unwrap(),
            generation: Segment::try_from("gen").unwrap(),
        }
    }

    /// A watch of a sender whose goals all ended has nothing to cancel: the
    /// sweep ends it and aborts its task. A sender with a live goal keeps its
    /// watch.
    #[tokio::test(start_paused = true)]
    async fn the_sweep_ends_the_watch_of_a_sender_whose_goals_all_ended() {
        let registry = empty_registry();
        registry
            .lock()
            .unwrap()
            .insert("live".to_string(), GoalSlot::new());
        registry.lock().unwrap().insert(
            "ended".to_string(),
            terminal_slot(Instant::now() + Duration::from_secs(10)),
        );
        let senders: SenderWatches = Arc::new(StdMutex::new(HashMap::new()));
        let watch = |goals: &[&str]| SenderWatch {
            goals: goals.iter().map(|goal| goal.to_string()).collect(),
            task: spawn(std::future::pending::<()>()),
        };
        senders
            .lock()
            .unwrap()
            .insert(client("busy"), watch(&["live", "ended"]));
        senders
            .lock()
            .unwrap()
            .insert(client("idle"), watch(&["ended", "evicted"]));

        end_idle_sender_watches(&registry, &senders);

        let mut senders = senders.lock().unwrap();
        let idle_task = senders.remove(&client("idle"));
        assert!(idle_task.is_none(), "the idle sender's watch ends");
        let busy = senders
            .get(&client("busy"))
            .expect("the busy sender keeps its watch");
        assert_eq!(busy.goals, HashSet::from(["live".to_string()]));
        assert!(!busy.task.is_finished());
    }
}

#[cfg(test)]
mod engine_tests {
    use super::*;
    use futures::poll;
    use pmi::{Messenger, MessengerAdapter, MessengerBackend, MockAdapter};

    const CORE: &str = "engine_core";
    const PRODUCER: &str = "engine_producer";
    const ACTION: &str = "engine_action";

    fn target() -> SenderTarget {
        SenderTarget::node("engine_node", "v1").unwrap()
    }

    const CALLER: &str = "engine_caller";

    /// A [`ConcurrentAction`] on a mock session, with a caller-side sender
    /// pinned to it.
    async fn expose_mock_action() -> (MessengerHandle, ConcurrentAction, ActionWireSender) {
        let mut messenger = Messenger::new(MessengerAdapter::Mock(MockAdapter::default()));
        messenger.start_session().await.unwrap();
        let handle = MessengerHandle::from_shared(Arc::new(TokioMutex::new(messenger)));
        let action = ConcurrentAction::expose(&handle, CORE, PRODUCER, target(), ACTION, false)
            .await
            .unwrap();
        let sender = ActionWireSender::new(
            CORE,
            CALLER,
            Some(&ProducerRef::new(CORE, PRODUCER)),
            target(),
            ACTION,
        )
        .unwrap();
        (handle, action, sender)
    }

    /// The goal request of `goal_id`, sent as the caller of
    /// [`expose_mock_action`]: its presence token exists, so the engine finds
    /// its sender alive.
    async fn goal_request(handle: &MessengerHandle, goal_id: &str) -> Payload {
        let presence = handle.client_presence(CORE, CALLER).await.unwrap();
        wrap_goal_request(goal_id, &presence.generation, &[]).unwrap()
    }

    async fn send_malformed_request(
        handle: &MessengerHandle,
        service: &pmi::ServiceWireSender,
    ) -> Error {
        handle
            .poll_service(service, Payload::new(), ServiceQueryKind::UserRequest, None)
            .await
            .unwrap_err()
    }

    /// A result poll that misses against a producer that is still alive stays
    /// an error: the follow-up liveliness probe finds the producer's token, so
    /// no Abandoned outcome is synthesized for a goal that is still running.
    /// The mock delivers the ACK of the poll, so the miss is a timeout.
    #[tokio::test(start_paused = true)]
    async fn a_result_poll_that_misses_against_a_live_producer_stays_a_timeout() {
        let (handle, mut action, sender) = expose_mock_action().await;
        let goal_service = sender.goal_service();
        let goal_id = "running_goal";
        let request = goal_request(&handle, goal_id).await;
        let (goal_reply, context) = tokio::join!(
            handle.poll_service(&goal_service, request, ServiceQueryKind::UserRequest, None),
            async {
                let pending = action.recv_next_goal().await.unwrap().unwrap();
                pending.accept(Payload::new()).await.unwrap()
            },
        );
        goal_reply.unwrap();

        // The goal never completes while `context` lives, so the poll parks
        // until its budget runs out.
        let result = ActionMessenger::request_result_with_sender(
            &handle,
            &sender,
            goal_id,
            Duration::from_millis(300),
        )
        .await;
        match result {
            Err(Error::ActionResultTimeout {
                instance_id,
                action_name,
            }) => {
                assert_eq!(instance_id.as_deref(), Some(PRODUCER));
                assert_eq!(action_name, ACTION);
            }
            Err(other) => panic!("expected a result timeout, got {other:?}"),
            Ok(reply) => panic!(
                "a live producer must not resolve to a reply, got {:?}",
                reply.outcome
            ),
        }
        drop(context);
    }

    #[tokio::test]
    async fn malformed_cancel_and_result_requests_get_an_error_and_the_loops_keep_serving() {
        let (handle, _action, sender) = expose_mock_action().await;
        for (service, expected_reason) in [
            (sender.cancel_service(), "malformed cancel request payload"),
            (sender.result_service(), "malformed result request payload"),
        ] {
            // The second request is answered only if the loop kept serving
            // after it answered the first.
            for _ in 0..2 {
                match send_malformed_request(&handle, &service).await {
                    Error::ServiceFailed { failure, .. } => {
                        assert_eq!(failure.kind().as_str(), "rejected");
                        assert_eq!(failure.message(), expected_reason);
                    }
                    other => panic!("expected the malformed-request error, got {other:?}"),
                }
            }
        }
    }

    /// How long the engine takes to confirm that a token it watched is gone:
    /// the confirmation probes, spaced by their delay.
    const CONFIRM_GONE: Duration = Duration::from_millis(
        PRODUCER_GONE_CONFIRM_DELAY.as_millis() as u64 * PRODUCER_GONE_CONFIRM_PROBES as u64,
    );

    /// A margin far below every delay the engine waits for, so that "just
    /// before" and "after" a deadline are distinct instants of the paused
    /// clock.
    const MARGIN: Duration = Duration::from_millis(5);

    /// A mock session that several handles share, as the processes of one
    /// stack share a router.
    async fn mock_session() -> Arc<TokioMutex<Messenger>> {
        let mut messenger = Messenger::new(MessengerAdapter::Mock(MockAdapter::default()));
        messenger.start_session().await.unwrap();
        Arc::new(TokioMutex::new(messenger))
    }

    /// A handle of its own on `session`: a process of its own, with its own
    /// presence generation.
    fn process_on(session: &Arc<TokioMutex<Messenger>>) -> MessengerHandle {
        MessengerHandle::from_shared(Arc::clone(session))
    }

    async fn expose_on(provider: &MessengerHandle) -> ConcurrentAction {
        ConcurrentAction::expose(provider, CORE, PRODUCER, target(), ACTION, true)
            .await
            .unwrap()
    }

    async fn send_from(caller: &MessengerHandle) -> Result<ActionGoalHandle> {
        ActionMessenger::send_goal(
            caller,
            CORE,
            CALLER,
            target(),
            ACTION,
            Some(&ProducerRef::new(CORE, PRODUCER)),
            Payload::from_static(b"goal"),
            DEFAULT_FEEDBACK_BOUND,
            Duration::from_secs(5),
        )
        .await
    }

    /// Sends a goal as `CALLER` from `caller` while `action` accepts it.
    async fn send_and_accept(
        caller: &MessengerHandle,
        action: &mut ConcurrentAction,
    ) -> (ActionGoalHandle, GoalContext) {
        let (goal, context) = tokio::join!(send_from(caller), async {
            action
                .recv_next_goal()
                .await
                .unwrap()
                .unwrap()
                .accept(Payload::new())
                .await
                .unwrap()
        });
        (goal.unwrap(), context)
    }

    /// Whether `context` receives its cancel signal within `wait` of the
    /// paused clock.
    async fn cancelled_within(context: &GoalContext, wait: Duration) -> bool {
        tokio::time::timeout(wait, context.cancel_signal())
            .await
            .is_ok()
    }

    /// Only a goal request that its provider acknowledged and did not
    /// answer in time may have started a goal: the caller cancels the goal
    /// of that request alone.
    #[test]
    fn only_a_request_that_timed_out_may_have_started_a_goal() {
        let service = || ("goal_service".to_owned(), Some("producer".to_owned()));
        let (service_name, instance_id) = service();
        assert!(request_may_have_started_a_goal(&Error::ServiceTimeout {
            instance_id,
            service_name,
        }));
        let (service_name, instance_id) = service();
        assert!(!request_may_have_started_a_goal(
            &Error::ServiceUnreachable {
                instance_id,
                service_name,
            }
        ));
    }

    fn unreachable_failure() -> Failure {
        Failure::new(
            config::failure_kind::FailureKindName::parse("unreachable").unwrap(),
            "no plan to the target",
        )
    }

    #[tokio::test(start_paused = true)]
    async fn the_goal_of_a_sender_that_is_gone_receives_its_cancel_signal() {
        let session = mock_session().await;
        let mut action = expose_on(&process_on(&session)).await;
        let caller = process_on(&session);
        let (goal, context) = send_and_accept(&caller, &mut action).await;
        assert_eq!(context.sender().instance_id.as_str(), CALLER);

        drop((goal, caller));

        assert!(
            !cancelled_within(&context, CONFIRM_GONE - MARGIN).await,
            "the engine confirms the end of a sender before it cancels"
        );
        assert!(cancelled_within(&context, MARGIN * 2).await);
        assert_eq!(context.cancel_cause(), Some(CancelCause::SenderGone));
    }

    #[tokio::test(start_paused = true)]
    async fn the_sender_grace_delays_the_cancel_of_a_gone_sender() {
        let grace = Duration::from_secs(3);
        let session = mock_session().await;
        let mut action = expose_on(&process_on(&session)).await;
        action.set_sender_grace(grace);
        assert_eq!(action.sender_grace().get(), grace);
        let caller = process_on(&session);
        let (goal, context) = send_and_accept(&caller, &mut action).await;

        drop((goal, caller));

        assert!(
            !cancelled_within(&context, CONFIRM_GONE + grace - MARGIN).await,
            "the goal is kept for the sender grace"
        );
        assert!(cancelled_within(&context, MARGIN * 2).await);
        assert_eq!(context.cancel_cause(), Some(CancelCause::SenderGone));
    }

    /// A sender whose token goes and comes back during the sender grace, as
    /// after a router bounce, keeps its goals.
    #[tokio::test(start_paused = true)]
    async fn a_sender_back_within_the_grace_keeps_its_goals() {
        let grace = Duration::from_secs(3);
        let session = mock_session().await;
        let provider = process_on(&session);
        let mut action = expose_on(&provider).await;
        action.set_sender_grace(grace);
        let client = ClientPresence {
            core_node: Segment::try_from(CORE).unwrap(),
            instance_id: Segment::try_from(CALLER).unwrap(),
            generation: Segment::try_from("bouncing_process").unwrap(),
        };
        let token = session
            .lock()
            .await
            .declare_client_presence(&client)
            .await
            .unwrap();
        let sender = ActionWireSender::new(
            CORE,
            CALLER,
            Some(&ProducerRef::new(CORE, PRODUCER)),
            target(),
            ACTION,
        )
        .unwrap();
        let request = wrap_goal_request("bouncing_goal", "bouncing_process", b"goal").unwrap();
        let goal_service = sender.goal_service();
        let (reply, context) = tokio::join!(
            provider.poll_service(&goal_service, request, ServiceQueryKind::UserRequest, None),
            async {
                action
                    .recv_next_goal()
                    .await
                    .unwrap()
                    .unwrap()
                    .accept(Payload::new())
                    .await
                    .unwrap()
            }
        );
        reply.unwrap();

        drop(token);
        tokio::time::sleep(CONFIRM_GONE + grace / 2).await;
        let _back = session
            .lock()
            .await
            .declare_client_presence(&client)
            .await
            .unwrap();

        assert!(!cancelled_within(&context, Duration::from_secs(60)).await);
        assert_eq!(context.cancel_cause(), None);
    }

    /// One request that is not a goal request, such as one from a peppylib
    /// that puts no sender generation in it, is refused with `rejected`, and
    /// the engine goes on to the next goal.
    #[tokio::test(start_paused = true)]
    async fn a_malformed_goal_request_is_refused_and_the_engine_serves_the_next_goal() {
        let (handle, mut action, sender) = expose_mock_action().await;
        let goal_service = sender.goal_service();
        let without_generation = wrap_goal_payload("old_goal", b"goal").unwrap();
        let ((refusal, accepted), next_goal_id) = tokio::join!(
            async {
                let refusal = handle
                    .poll_service(
                        &goal_service,
                        without_generation,
                        ServiceQueryKind::UserRequest,
                        None,
                    )
                    .await
                    .unwrap();
                let request = goal_request(&handle, "next_goal").await;
                let accepted = handle
                    .poll_service(&goal_service, request, ServiceQueryKind::UserRequest, None)
                    .await
                    .unwrap();
                (refusal, accepted)
            },
            async {
                let pending = action.recv_next_goal().await.unwrap().unwrap();
                let goal_id = pending.goal_id().to_string();
                let _context = pending.accept(Payload::new()).await.unwrap();
                goal_id
            }
        );
        match unwrap_goal_ack(&refusal.payload().into_inner()).unwrap() {
            GoalAnswer::Refused(failure) => assert_eq!(failure.kind().as_str(), "rejected"),
            GoalAnswer::Accepted(_) => panic!("a malformed request must be refused"),
        }
        assert!(matches!(
            unwrap_goal_ack(&accepted.payload().into_inner()).unwrap(),
            GoalAnswer::Accepted(_)
        ));
        assert_eq!(next_goal_id, "next_goal");
    }

    /// A cancel that arrives before its goal is admitted, as the cancel a
    /// caller sends when it gave up on the answer, cancels the goal at accept.
    #[tokio::test(start_paused = true)]
    async fn a_cancel_that_arrives_before_accept_cancels_the_goal_at_accept() {
        let (handle, mut action, sender) = expose_mock_action().await;
        let goal_service = sender.goal_service();
        let goal_id = "early_cancelled_goal";
        let request = goal_request(&handle, goal_id).await;
        let (reply, context) = tokio::join!(
            handle.poll_service(&goal_service, request, ServiceQueryKind::UserRequest, None),
            async {
                let pending = action.recv_next_goal().await.unwrap().unwrap();
                let cancel = ActionMessenger::cancel_with_sender(
                    &handle,
                    &sender,
                    goal_id,
                    Duration::from_secs(5),
                )
                .await
                .unwrap();
                assert_eq!(
                    decode_cancel_ack(cancel.payload_bytes().as_ref()).unwrap(),
                    CancelState::Unknown
                );
                pending.accept(Payload::new()).await.unwrap()
            }
        );
        reply.unwrap();
        assert!(context.is_cancelled());
        assert_eq!(context.cancel_cause(), Some(CancelCause::Caller));
    }

    /// Sends a goal as `CALLER` from `caller`, whose handle keeps one
    /// unread feedback message, while `action` accepts it, and publishes two
    /// feedback messages: the feedback of the handle overflows.
    async fn accept_and_overflow_one_message(
        caller: &MessengerHandle,
        action: &mut ConcurrentAction,
    ) -> (ActionGoalHandle, GoalContext) {
        let one_message = FeedbackBound::new(std::num::NonZeroUsize::new(1).unwrap());
        let producer = ProducerRef::new(CORE, PRODUCER);
        let (goal, context) = tokio::join!(
            ActionMessenger::send_goal(
                caller,
                CORE,
                CALLER,
                target(),
                ACTION,
                Some(&producer),
                Payload::from_static(b"goal"),
                one_message,
                Duration::from_secs(5),
            ),
            async {
                action
                    .recv_next_goal()
                    .await
                    .unwrap()
                    .unwrap()
                    .accept(Payload::new())
                    .await
                    .unwrap()
            }
        );
        for message in [b"first".as_slice(), b"second"] {
            context
                .publish_feedback(
                    NonEmptyPayload::try_new(Payload::from(message.to_vec())).unwrap(),
                )
                .await
                .unwrap();
        }
        (goal.unwrap(), context)
    }

    /// A caller that drops its handle on the overflow error still cancels
    /// the goal.
    #[tokio::test(start_paused = true)]
    async fn a_handle_dropped_on_its_overflow_still_cancels_the_goal() {
        let session = mock_session().await;
        let mut action = expose_on(&process_on(&session)).await;
        let caller = process_on(&session);
        let (mut goal, context) = accept_and_overflow_one_message(&caller, &mut action).await;

        assert!(matches!(
            goal.try_next_feedback(),
            Err(Error::ActionFeedbackOverflow { bound: 1, .. })
        ));
        drop(goal);

        assert!(cancelled_within(&context, Duration::from_secs(5)).await);
        assert_eq!(context.cancel_cause(), Some(CancelCause::Caller));
    }

    /// A caller that never reads the feedback of its goal keeps the goal
    /// when the feedback overflows, and reads its result.
    #[tokio::test(start_paused = true)]
    async fn a_caller_that_reads_no_feedback_keeps_its_goal_past_the_bound() {
        let session = mock_session().await;
        let mut action = expose_on(&process_on(&session)).await;
        let caller = process_on(&session);
        let (goal, context) = accept_and_overflow_one_message(&caller, &mut action).await;

        assert!(
            !cancelled_within(&context, Duration::from_secs(60)).await,
            "an overflow that no read finds cancels nothing"
        );
        context
            .complete(Payload::from_static(b"done"))
            .await
            .unwrap();
        let reply = ActionMessenger::request_result(&caller, &goal, Duration::from_secs(5))
            .await
            .unwrap();
        assert_eq!(
            reply.outcome,
            GoalOutcome::Completed(Payload::from_static(b"done"))
        );
    }

    #[tokio::test(start_paused = true)]
    async fn the_goal_of_a_sender_that_stays_is_never_cancelled() {
        let session = mock_session().await;
        let mut action = expose_on(&process_on(&session)).await;
        let caller = process_on(&session);
        let (_goal, context) = send_and_accept(&caller, &mut action).await;

        assert!(!cancelled_within(&context, Duration::from_secs(60)).await);
        assert_eq!(context.cancel_cause(), None);
    }

    /// Two processes that send as the same `(core_node, instance_id)`, such
    /// as a node that stopped and started again, are two senders: the end of
    /// one cancels its goals only.
    #[tokio::test(start_paused = true)]
    async fn two_processes_with_one_identity_are_two_senders() {
        let session = mock_session().await;
        let mut action = expose_on(&process_on(&session)).await;
        let first = process_on(&session);
        let second = process_on(&session);
        let (first_goal, first_context) = send_and_accept(&first, &mut action).await;
        let (_second_goal, second_context) = send_and_accept(&second, &mut action).await;
        assert_eq!(
            first_context.sender().instance_id,
            second_context.sender().instance_id
        );
        assert_ne!(
            first_context.sender().generation,
            second_context.sender().generation
        );

        drop((first_goal, first));

        assert!(cancelled_within(&first_context, CONFIRM_GONE + MARGIN).await);
        assert_eq!(first_context.cancel_cause(), Some(CancelCause::SenderGone));
        assert!(!cancelled_within(&second_context, Duration::from_secs(60)).await);
    }

    /// A goal whose sender declared no presence token (a sender that died
    /// before its goal arrived) is cancelled once the engine gave the token
    /// its time to appear and confirmed it absent.
    #[tokio::test(start_paused = true)]
    async fn the_goal_of_a_sender_with_no_presence_is_cancelled() {
        let (handle, mut action, sender) = expose_mock_action().await;
        let request = wrap_goal_request("orphan_goal", "no_such_process", &[]).unwrap();
        let goal_service = sender.goal_service();
        let (goal_reply, context) = tokio::join!(
            handle.poll_service(&goal_service, request, ServiceQueryKind::UserRequest, None,),
            async {
                let pending = action.recv_next_goal().await.unwrap().unwrap();
                pending.accept(Payload::new()).await.unwrap()
            },
        );
        goal_reply.unwrap();

        let wait = LIVELINESS_RESOLVE_TIMEOUT + CONFIRM_GONE;
        assert!(!cancelled_within(&context, wait - MARGIN).await);
        assert!(cancelled_within(&context, MARGIN * 2).await);
        assert_eq!(context.cancel_cause(), Some(CancelCause::SenderGone));
    }

    #[tokio::test(start_paused = true)]
    async fn a_refused_goal_gives_its_caller_the_failure_kind_and_message() {
        let session = mock_session().await;
        let mut action = expose_on(&process_on(&session)).await;
        let caller = process_on(&session);
        let (goal, refused) = tokio::join!(send_from(&caller), async {
            let pending = action.recv_next_goal().await.unwrap().unwrap();
            pending.refuse(unreachable_failure()).await
        });
        refused.unwrap();
        let goal = goal.unwrap();
        assert_eq!(
            goal.goal_reply().answer,
            GoalAnswer::Refused(unreachable_failure())
        );
        assert!(!goal.goal_reply().is_accepted());
    }

    #[tokio::test(start_paused = true)]
    async fn a_failed_goal_gives_its_kind_and_message_and_no_result() {
        let session = mock_session().await;
        let mut action = expose_on(&process_on(&session)).await;
        let caller = process_on(&session);
        let (goal, context) = send_and_accept(&caller, &mut action).await;

        context.fail(unreachable_failure()).await.unwrap();

        let reply = ActionMessenger::request_result(&caller, &goal, Duration::from_secs(5))
            .await
            .unwrap();
        assert_eq!(reply.outcome, GoalOutcome::Failed(unreachable_failure()));
        match ActionMessenger::request_result_body(&caller, &goal, Duration::from_secs(5)).await {
            Err(Error::ActionGoalFailed {
                instance_id,
                action_name,
                failure,
            }) => {
                assert_eq!(instance_id.as_deref(), Some(PRODUCER));
                assert_eq!(action_name, ACTION);
                assert_eq!(failure, unreachable_failure());
            }
            other => panic!("expected the failed goal, got {other:?}"),
        }
    }

    /// The token of a goal's cancellation is cancelled once the engine
    /// answers the cancel request: work off the goal's task sees the cancel
    /// at its next check, with no poll of the goal's task in between.
    #[tokio::test(start_paused = true)]
    async fn the_cancellation_of_a_goal_is_cancelled_when_the_cancel_is_answered() {
        let session = mock_session().await;
        let mut action = expose_on(&process_on(&session)).await;
        let caller = process_on(&session);
        let (goal, context) = send_and_accept(&caller, &mut action).await;
        let cancellation = context.cancellation();
        assert!(!cancellation.is_cancelled());

        ActionMessenger::cancel_goal(&caller, &goal, Duration::from_secs(5))
            .await
            .unwrap();
        assert!(cancellation.is_cancelled());
    }

    /// A token linked to a goal is cancelled once the engine answers the
    /// cancel request, with no poll of the goal's task in between, and the
    /// signal reaches no parent of the token; a token whose link dropped is
    /// left as it is.
    #[tokio::test(start_paused = true)]
    async fn a_linked_token_is_cancelled_when_the_cancel_is_answered() {
        let session = mock_session().await;
        let mut action = expose_on(&process_on(&session)).await;
        let caller = process_on(&session);
        let (goal, context) = send_and_accept(&caller, &mut action).await;
        let parent = CancellationToken::new();
        let linked = parent.child_token();
        let unlinked = CancellationToken::new();
        let _link = context.cancel_at_signal(linked.clone());
        drop(context.cancel_at_signal(unlinked.clone()));

        ActionMessenger::cancel_goal(&caller, &goal, Duration::from_secs(5))
            .await
            .unwrap();
        assert!(linked.is_cancelled(), "the answer cancels the linked token");
        assert!(
            !unlinked.is_cancelled(),
            "a token whose link dropped is left as it is"
        );
        assert!(!parent.is_cancelled(), "the signal reaches no parent");
    }

    /// A token linked to a goal that received its cancel signal already is
    /// cancelled at once.
    #[tokio::test(start_paused = true)]
    async fn a_token_linked_after_the_signal_is_cancelled_at_once() {
        let session = mock_session().await;
        let mut action = expose_on(&process_on(&session)).await;
        let caller = process_on(&session);
        let (goal, context) = send_and_accept(&caller, &mut action).await;
        ActionMessenger::cancel_goal(&caller, &goal, Duration::from_secs(5))
            .await
            .unwrap();

        let token = CancellationToken::new();
        let _link = context.cancel_at_signal(token.clone());
        assert!(token.is_cancelled());
    }

    /// A cancel of the caller reaches the worker with the cause `Caller`; the
    /// worker ends the goal cancelled, with its reason and result, and the
    /// caller reads both.
    #[tokio::test(start_paused = true)]
    async fn a_cancelled_goal_gives_its_reason_and_result() {
        let session = mock_session().await;
        let mut action = expose_on(&process_on(&session)).await;
        let caller = process_on(&session);
        let (goal, context) = send_and_accept(&caller, &mut action).await;

        let ack = ActionMessenger::cancel_goal(&caller, &goal, Duration::from_secs(5))
            .await
            .unwrap();
        assert_eq!(
            decode_cancel_ack(ack.payload_bytes().as_ref()).unwrap(),
            CancelState::Signalled
        );
        context.cancel_signal().await;
        assert_eq!(context.cancel_cause(), Some(CancelCause::Caller));
        context
            .complete_cancelled(
                "stopped: the operator pressed stop",
                Payload::from_static(b"held"),
            )
            .await
            .unwrap();

        let reply = ActionMessenger::request_result(&caller, &goal, Duration::from_secs(5))
            .await
            .unwrap();
        assert_eq!(
            reply.outcome,
            GoalOutcome::Cancelled {
                reason: "stopped: the operator pressed stop".to_string(),
                result: Payload::from_static(b"held"),
            }
        );
        match ActionMessenger::request_result_body(&caller, &goal, Duration::from_secs(5)).await {
            Err(Error::ActionGoalCancelled { reason, .. }) => {
                assert_eq!(reason, "stopped: the operator pressed stop");
            }
            other => panic!("expected the cancelled goal, got {other:?}"),
        }
    }

    /// The first ending of a goal is its ending: a later `fail` changes
    /// nothing.
    #[tokio::test(start_paused = true)]
    async fn the_first_ending_of_a_goal_wins() {
        let session = mock_session().await;
        let mut action = expose_on(&process_on(&session)).await;
        let caller = process_on(&session);
        let (goal, context) = send_and_accept(&caller, &mut action).await;

        context
            .complete(Payload::from_static(b"done"))
            .await
            .unwrap();
        context.fail(unreachable_failure()).await.unwrap();

        let body = ActionMessenger::request_result_body(&caller, &goal, Duration::from_secs(5))
            .await
            .unwrap();
        assert_eq!(body, Payload::from_static(b"done"));
    }

    #[tokio::test]
    async fn a_result_poll_that_arrives_before_accept_resolves_with_the_goal() {
        let (handle, mut action, sender) = expose_mock_action().await;
        let result_service = sender.result_service();
        let goal_service = sender.goal_service();
        let goal_id = "early_goal";
        let mut early_poll = Box::pin(handle.poll_service(
            &result_service,
            wrap_goal_payload(goal_id, &[]).unwrap(),
            ServiceQueryKind::UserRequest,
            None,
        ));
        assert!(poll!(early_poll.as_mut()).is_pending());
        // The result loop routes requests in order, so the answer to a later
        // request proves that it has routed the early poll.
        send_malformed_request(&handle, &result_service).await;
        assert!(action.pending_waiters.lock().unwrap().contains_key(goal_id));

        let request = goal_request(&handle, goal_id).await;
        let mut goal_call = Box::pin(handle.poll_service(
            &goal_service,
            request,
            ServiceQueryKind::UserRequest,
            None,
        ));
        assert!(poll!(goal_call.as_mut()).is_pending());
        let pending = action.recv_next_goal().await.unwrap().unwrap();
        assert_eq!(pending.goal_id(), goal_id);
        let context = pending.accept(Payload::new()).await.unwrap();
        assert!(!action.pending_waiters.lock().unwrap().contains_key(goal_id));
        goal_call.await.unwrap();
        context
            .complete(Payload::from_static(b"early result"))
            .await
            .unwrap();

        let reply = early_poll.await.unwrap();
        assert_eq!(
            unwrap_result_outcome(&reply.payload().into_inner()).unwrap(),
            GoalOutcome::Completed(Payload::from_static(b"early result"))
        );
    }
}
