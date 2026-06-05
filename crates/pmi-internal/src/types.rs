use super::adapters::mock::{MockAdapter, MockPublisher};
use super::error::Result;
use super::wire::zenoh_format::{ServiceReplyAttachment, ZenohWireFormat};
use super::wire::{
    ActionWireReceiver, ActionWireSender, ServiceQueryKind, ServiceReplyKind, ServiceWireReceiver,
    ServiceWireSender, TopicWireReceiver, TopicWireSender,
};
use config::node::QoSProfile;
use std::borrow::Cow;
use std::future::Future;
use std::net::SocketAddr;
#[cfg(feature = "zenoh")]
use zenoh::bytes::ZBytes;

#[cfg(feature = "zenoh")]
use super::adapters::zenoh::{ZenohAdapter, ZenohPublisher};
#[cfg(feature = "router")]
use super::zenohd::RouterHealthChecker;

/// QoS settings for publishing messages
#[derive(Default, Debug, Clone, Copy, PartialEq, Eq)]
pub enum PublisherQoS {
    /// Best effort delivery with minimal latency
    /// - Priority: DataLow
    /// - Congestion: Drop
    /// - Express: true
    BestEffort,

    /// Standard reliable delivery for regular messages
    /// - Priority: Data
    /// - Congestion: Drop
    /// - Express: false
    #[default]
    Standard,

    /// Important messages that should be prioritized
    /// - Priority: DataHigh
    /// - Congestion: Block
    /// - Express: false
    Important,

    /// Critical real-time messages (e.g., safety-critical commands)
    /// - Priority: RealTime
    /// - Congestion: Block
    /// - Express: true
    Critical,
}

impl From<QoSProfile> for PublisherQoS {
    fn from(qos: QoSProfile) -> Self {
        match qos {
            QoSProfile::Standard => PublisherQoS::Standard,
            QoSProfile::Reliable => PublisherQoS::Important,
            QoSProfile::SensorData => PublisherQoS::BestEffort,
            QoSProfile::Critical => PublisherQoS::Critical,
        }
    }
}

/// QoS settings for subscribing to messages
#[derive(Default, Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubscriberQoS {
    /// Standard reliable reception for regular messages
    #[default]
    Standard,

    /// High throughput reliable reception (e.g., sensor data streams)
    HighThroughput,
}

impl SubscriberQoS {
    /// Returns the channel buffer size for this QoS setting
    pub fn channel_size(&self) -> usize {
        match self {
            SubscriberQoS::Standard => 128,
            SubscriberQoS::HighThroughput => 1024,
        }
    }
}

impl From<QoSProfile> for SubscriberQoS {
    fn from(qos: QoSProfile) -> Self {
        match qos {
            QoSProfile::SensorData => SubscriberQoS::HighThroughput,
            _ => SubscriberQoS::Standard,
        }
    }
}

/// Sentinel used by adapters when [`MessengerBackend::call_service`] is
/// invoked with `timeout: None`. Zenoh's `get` builder demands a finite
/// `Duration`; one day is far longer than any in-process or interactive
/// test wait, so it stands in for "wait indefinitely" without forcing
/// each adapter to track its own value.
///
/// The window must also cover the slowest legitimate user service handler
/// — once the consumer receives `Ack`, the zenoh query must stay open long
/// enough for the producer's `Response` reply. Shortening this would
/// silently cap how long a service handler may run.
pub(crate) const NO_TIMEOUT_SENTINEL: std::time::Duration = std::time::Duration::from_secs(86_400);

/// Defines the messaging interface.
///
/// All methods take addressing structs from [`crate::wire`] rather than raw
/// keyexpressions. Each adapter is responsible for formatting them into its
/// own wire form internally. Only the per-transport `*_wire.rs` modules are
/// authorized to produce raw keyexpressions.
pub trait MessengerBackend {
    /// Initialize the pubsub session.
    fn start_session(&mut self) -> impl Future<Output = Result<()>> + Send;

    /// Gracefully shuts down the pubsub session. For transports with a separate
    /// router process (e.g. Zenoh) this MUST be called while the router is
    /// still reachable — the close handshake undeclares every face on the
    /// router side, and skipping it leaves zenoh logging `Undefined face
    /// context` when the session Drop later tries to close over a dead
    /// transport.
    fn stop_session(&mut self) -> impl Future<Output = Result<()>> + Send;

    // ─── Topics ───────────────────────────────────────────────────────────

    /// Subscribe to a topic.
    fn subscribe_topic(
        &self,
        recv: &TopicWireReceiver,
        qos: SubscriberQoS,
    ) -> impl Future<Output = Result<Subscription>> + Send;

    /// Publish a one-shot topic message. `is_primary` rides on a wire
    /// attachment so subscribers can disambiguate the N publishes a
    /// multi-link_id `emit` produces — see the topic-attachment section
    /// in [`crate::wire::zenoh_format`] for the dedup contract.
    fn publish_topic(
        &mut self,
        sender: &TopicWireSender,
        payload: Payload,
        qos: PublisherQoS,
        is_primary: bool,
    ) -> impl Future<Output = Result<()>> + Send;

    // ─── Services ─────────────────────────────────────────────────────────

    /// Declare a service queryable (one per producer-bound link_id) and return
    /// a fan-in handle for received [`IncomingRequest`]s. Producers with
    /// multiple bound link_ids get one queryable each so Zenoh keyexpr
    /// matching can replace the previous dispatch-time filter.
    fn listen_service(
        &self,
        recv: &ServiceWireReceiver,
    ) -> impl Future<Output = Result<ServiceQueryable>> + Send;

    /// Issue a service `get` and return a stream of replies (ACK + final
    /// response, possibly fanned out across multiple matching producers).
    /// `kind` rides on the query attachment so the producer can
    /// discriminate user requests from discovery probes without inspecting
    /// payload bytes. Query/reply correlation is internal to Zenoh — no
    /// `request_id` threaded through the wire format. Pass `timeout: None`
    /// to wait indefinitely (adapters substitute [`NO_TIMEOUT_SENTINEL`]
    /// since the underlying Zenoh `get` requires a finite value).
    fn call_service(
        &self,
        sender: &ServiceWireSender,
        payload: Payload,
        kind: ServiceQueryKind,
        timeout: Option<std::time::Duration>,
    ) -> impl Future<Output = Result<ReplyStream>> + Send;

    // ─── Actions ──────────────────────────────────────────────────────────

    /// Subscribe to a specific goal's feedback stream.
    fn subscribe_action_feedback(
        &self,
        sender: &ActionWireSender,
        goal_id: &str,
        qos: SubscriberQoS,
    ) -> impl Future<Output = Result<Subscription>> + Send;

    // ─── Router lifecycle ─────────────────────────────────────────────────

    /// Starts the router in background and immediately returns for engines
    /// that use a router. Only relevant when the library is brokering between
    /// nodes; client-only adapters can no-op.
    fn start_router(&mut self) -> impl Future<Output = Result<()>> + Send;

    /// Stops the router.
    fn stop_router(&mut self) -> impl Future<Output = Result<()>> + Send;

    /// Returns the socket address (host and port) of this messenger backend.
    fn get_host(&self) -> SocketAddr;
}

/// Opaque drop-guard returned by adapters. The handle's `Drop` releases the
/// backing messenger entity (zenoh's `Subscriber<()>` / `Queryable<()>`,
/// which undeclare on drop; or [`AbortOnDrop`] for adapters that still drive
/// the messaging surface via a spawned forwarder task).
///
/// Crate-private because only adapter modules construct guards; the [`Any`]
/// bound is an implementation detail used to erase heterogeneous adapter
/// types, not a downcast surface for callers.
pub(crate) type Guard = Box<dyn std::any::Any + Send + Sync>;

/// Wraps a tokio task handle so dropping the wrapper cancels the task.
/// `AbortHandle` alone does not abort on drop — only the explicit `.abort()`
/// call does — so adapters that need that semantics wrap their handle in
/// this type before stashing it in a [`Guard`].
pub struct AbortOnDrop(tokio::task::AbortHandle);

impl AbortOnDrop {
    pub fn new(handle: tokio::task::AbortHandle) -> Self {
        Self(handle)
    }
}

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Caller-side handle for a topic subscription. The adapter-owned `_guard`
/// keeps the underlying messenger entity (zenoh `Subscriber<()>`, mock
/// registration token, etc.) alive for the lifetime of this struct and
/// releases it cleanly on drop.
///
/// The channel is `flume::bounded` rather than `tokio::sync::mpsc` because
/// zenoh's reception callback runs synchronously inside zenoh's tokio
/// runtime; `tokio::sync::mpsc::Sender::blocking_send` panics from inside a
/// runtime, whereas `flume::Sender::send` just blocks the OS thread when
/// the buffer is full — preserving end-to-end backpressure for Reliable
/// QoS topics.
pub struct Subscription {
    pub rx: flume::Receiver<TopicMessage>,
    _guard: Guard,
}

impl Subscription {
    pub(crate) fn new(rx: flume::Receiver<TopicMessage>, guard: Guard) -> Self {
        Self { rx, _guard: guard }
    }

    pub async fn on_next_message(&mut self) -> Option<TopicMessage> {
        self.rx.recv_async().await.ok()
    }
}

/// Server-side handle yielded by [`MessengerBackend::listen_service`] for a
/// service or action sub-service. The adapter-owned `_guards` keep one
/// queryable (Zenoh `Queryable<()>` or mock equivalent) per producer-bound
/// link_id alive, fanned into a single [`IncomingRequest`] channel.
/// Dropping the handle drops every guard, which undeclares the underlying
/// queryables.
///
/// Like [`Subscription`], the channel uses `flume` so the zenoh reception
/// callback can `send` synchronously with backpressure rather than dropping
/// inbound requests when the consumer falls behind.
pub struct ServiceQueryable {
    pub rx: flume::Receiver<IncomingRequest>,
    _guards: Vec<Guard>,
}

impl ServiceQueryable {
    pub(crate) fn new(rx: flume::Receiver<IncomingRequest>, guards: Vec<Guard>) -> Self {
        Self {
            rx,
            _guards: guards,
        }
    }
}

/// One reply delivered by [`MessengerBackend::call_service`]. Pairs the
/// topic-shape reply [`TopicMessage`] (caller-visible payload + responder
/// identity, parsed from the reply keyexpr) with the [`ServiceReplyKind`]
/// the producer set on the reply attachment. The consumer's poll loop
/// matches on `kind` to skip ACKs, return regular responses, and surface
/// handler errors — without inspecting payload bytes for legacy sentinels.
pub struct ServiceReply {
    message: TopicMessage,
    kind: ServiceReplyKind,
}

impl ServiceReply {
    pub fn new(message: TopicMessage, kind: ServiceReplyKind) -> Self {
        Self { message, kind }
    }

    pub fn message(&self) -> &TopicMessage {
        &self.message
    }

    pub fn into_message(self) -> TopicMessage {
        self.message
    }

    pub fn kind(&self) -> ServiceReplyKind {
        self.kind
    }
}

/// Caller-side reply stream returned by [`MessengerBackend::call_service`]. A
/// single in-flight service call may receive multiple replies — at minimum an
/// ACK followed by the real response, plus additional replies from sibling
/// producers in `from_any` / broadcast scenarios. Each [`ServiceReply`]
/// carries the producer-set kind on its attachment so peppylib can
/// discriminate without inspecting payload bytes.
pub struct ReplyStream {
    pub rx: tokio::sync::mpsc::Receiver<ServiceReply>,
    _guard: Option<Guard>,
}

impl ReplyStream {
    pub(crate) fn new(rx: tokio::sync::mpsc::Receiver<ServiceReply>, guard: Option<Guard>) -> Self {
        Self { rx, _guard: guard }
    }
}

/// A single service request delivered to the responder. `token` is the
/// only legal way to send replies — peppylib calls `respond_ack` first
/// (lets the consumer distinguish ServiceUnreachable from ServiceTimeout),
/// then exactly one terminal `respond_response` / `respond_handler_error`.
pub struct IncomingRequest {
    pub payload: Payload,
    /// Whether this is a user request (handler should run) or a discovery
    /// probe (the framework auto-replies with [`ServiceReplyKind::Response`]
    /// and an empty payload, never invoking the user handler). Decoded
    /// from the mandatory query attachment.
    pub kind: ServiceQueryKind,
    /// The producer-side link_id that received this request — set by the
    /// adapter to whichever bound link_id's queryable yielded the query.
    /// Surfaced to action goal handlers so per-goal feedback addresses the
    /// link_id the consumer targeted.
    pub link_id: String,
    /// Caller's `core_node` segment, parsed from the inbound query selector.
    pub caller_core: String,
    /// Caller's `instance_id` segment, parsed from the inbound query selector.
    pub caller_inst: String,
    pub token: ResponseToken,
}

/// Opaque handle that lets the responder send replies back to a specific
/// in-flight caller. One token per [`IncomingRequest`]. Constructed inside
/// the adapter; peppylib stores it on a `ServiceResponder`.
///
/// The reply protocol is two-step: `respond_ack` is non-consuming and runs
/// first, then exactly one terminal `respond_response` or
/// `respond_handler_error` consumes the token. Consuming the token drops
/// the underlying Zenoh `Query` (or mock reply channel), closing the
/// consumer's reply stream after the final reply is observed.
pub enum ResponseToken {
    #[cfg(feature = "zenoh")]
    Zenoh(ZenohResponseToken),
    Mock(MockResponseToken),
}

impl ResponseToken {
    /// Sends the ACK reply (empty payload, `ServiceReplyKind::Ack` on the
    /// reply attachment). Non-consuming so the same token can deliver the
    /// terminal response afterward. The consumer's poll loop uses the ACK
    /// to distinguish `ServiceUnreachable` (no ACK at all) from
    /// `ServiceTimeout` (ACK received but handler didn't reply in time).
    pub async fn respond_ack(&self) -> Result<()> {
        let empty = Payload::from_bytes(bytes::Bytes::new());
        match self {
            #[cfg(feature = "zenoh")]
            ResponseToken::Zenoh(t) => t.respond_with_kind(empty, ServiceReplyKind::Ack).await,
            ResponseToken::Mock(t) => t.respond_with_kind(empty, ServiceReplyKind::Ack).await,
        }
    }

    /// Sends the terminal user response (`ServiceReplyKind::Response`).
    /// Consumes the token so no further replies can be sent for this
    /// request. Also used for the producer's transparent reply to a
    /// `ServiceQueryKind::Probe` query, with an empty payload — probes
    /// must NOT use [`Self::respond_ack`] because the consumer's poll
    /// loop would otherwise skip the only reply the probe path produces.
    pub async fn respond_response(self, payload: Payload) -> Result<()> {
        match self {
            #[cfg(feature = "zenoh")]
            ResponseToken::Zenoh(t) => {
                t.respond_with_kind(payload, ServiceReplyKind::Response)
                    .await
            }
            ResponseToken::Mock(t) => {
                t.respond_with_kind(payload, ServiceReplyKind::Response)
                    .await
            }
        }
    }

    /// Sends a handler-error reply: the reason rides in the payload as
    /// UTF-8, with `ServiceReplyKind::HandlerError` on the attachment so
    /// the consumer's poll loop surfaces `Error::ServiceError { reason }`
    /// instead of returning the bytes as a normal response. Consumes the
    /// token.
    pub async fn respond_handler_error(self, reason: String) -> Result<()> {
        let payload = Payload::from_bytes(bytes::Bytes::from(reason.into_bytes()));
        match self {
            #[cfg(feature = "zenoh")]
            ResponseToken::Zenoh(t) => {
                t.respond_with_kind(payload, ServiceReplyKind::HandlerError)
                    .await
            }
            ResponseToken::Mock(t) => {
                t.respond_with_kind(payload, ServiceReplyKind::HandlerError)
                    .await
            }
        }
    }
}

/// Zenoh-backed variant of [`ResponseToken`]. Carries the inbound `Query`
/// plus a precomputed concrete reply keyexpr (topic-shape, with caller and
/// responder identities filled in) so `parse_topic_keyexpr` on the caller
/// side surfaces the responder's `(core_node, instance_id)` to the user.
#[cfg(feature = "zenoh")]
pub struct ZenohResponseToken {
    query: zenoh::query::Query,
    reply_keyexpr: String,
}

#[cfg(feature = "zenoh")]
impl ZenohResponseToken {
    pub fn new(query: zenoh::query::Query, reply_keyexpr: String) -> Self {
        Self {
            query,
            reply_keyexpr,
        }
    }

    async fn respond_with_kind(&self, payload: Payload, kind: ServiceReplyKind) -> Result<()> {
        let attachment = ServiceReplyAttachment { kind }.encode();
        self.query
            .reply(self.reply_keyexpr.as_str(), payload.into_zbytes())
            .attachment(attachment.to_vec())
            .await
            .map_err(|e| crate::error::Error::BackendError(e.to_string()))?;
        Ok(())
    }
}

/// In-process variant of [`ResponseToken`]. Holds the reply channel half
/// the mock's `get_keyexpr` is reading from, so each reply pushes one
/// [`ServiceReply`] back to the caller.
pub struct MockResponseToken {
    reply_tx: tokio::sync::mpsc::Sender<ServiceReply>,
    reply_keyexpr: String,
}

impl MockResponseToken {
    pub fn new(reply_tx: tokio::sync::mpsc::Sender<ServiceReply>, reply_keyexpr: String) -> Self {
        Self {
            reply_tx,
            reply_keyexpr,
        }
    }

    async fn respond_with_kind(&self, payload: Payload, kind: ServiceReplyKind) -> Result<()> {
        let message = TopicMessage::new(&self.reply_keyexpr, payload)?;
        self.reply_tx
            .send(ServiceReply::new(message, kind))
            .await
            .map_err(|_| crate::error::Error::BackendError("mock reply channel closed".into()))?;
        Ok(())
    }
}

/// Core message structure
#[derive(Clone)]
pub struct Message {
    identifier: String,
    payload: bytes::Bytes,
}

impl Message {
    pub fn new(identifier: &str, payload: impl AsRef<[u8]>) -> Self {
        Self {
            identifier: identifier.to_string(),
            payload: bytes::Bytes::copy_from_slice(payload.as_ref()),
        }
    }

    pub fn identifier(&self) -> &str {
        &self.identifier
    }

    pub fn payload(&self) -> &bytes::Bytes {
        &self.payload
    }
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Payload {
    inner: PayloadInner,
}

#[derive(Clone, PartialEq, Eq, Debug)]
enum PayloadInner {
    Bytes(bytes::Bytes),
    #[cfg(feature = "zenoh")]
    Zenoh(ZBytes),
}

/// This payload struct avoids copy/cloning until it's needed
impl Payload {
    pub fn from_bytes(bytes: bytes::Bytes) -> Self {
        Self {
            inner: PayloadInner::Bytes(bytes),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn len(&self) -> usize {
        match &self.inner {
            PayloadInner::Bytes(b) => b.len(),
            #[cfg(feature = "zenoh")]
            PayloadInner::Zenoh(z) => z.len(),
        }
    }

    /// Returns a borrowed view of the payload when it is contiguous, or an owned buffer otherwise.
    pub fn as_bytes(&self) -> Cow<'_, [u8]> {
        match &self.inner {
            PayloadInner::Bytes(b) => Cow::Borrowed(b.as_ref()),
            #[cfg(feature = "zenoh")]
            PayloadInner::Zenoh(z) => z.to_bytes(),
        }
    }

    /// Returns the payload as a contiguous buffer if possible.
    /// This may allocate when the underlying storage is non-contiguous (e.g. Zenoh multi-slice).
    pub fn to_bytes(&self) -> bytes::Bytes {
        match &self.inner {
            PayloadInner::Bytes(b) => b.clone(),
            #[cfg(feature = "zenoh")]
            PayloadInner::Zenoh(z) => match z.to_bytes() {
                Cow::Borrowed(slice) => bytes::Bytes::copy_from_slice(slice),
                Cow::Owned(vec) => bytes::Bytes::from(vec),
            },
        }
    }

    /// Converts the payload into a contiguous buffer, consuming self.
    /// This is zero-copy when the payload was already backed by `bytes::Bytes`.
    pub fn into_bytes(self) -> bytes::Bytes {
        match self.inner {
            PayloadInner::Bytes(b) => b,
            #[cfg(feature = "zenoh")]
            PayloadInner::Zenoh(z) => match z.to_bytes() {
                Cow::Borrowed(slice) => bytes::Bytes::copy_from_slice(slice),
                Cow::Owned(vec) => bytes::Bytes::from(vec),
            },
        }
    }

    /// Iterate over the underlying slices without forcing contiguity.
    pub fn slices(&self) -> PayloadSlices<'_> {
        match &self.inner {
            PayloadInner::Bytes(b) => PayloadSlices::BytesSlice(std::iter::once(b.as_ref())),
            #[cfg(feature = "zenoh")]
            PayloadInner::Zenoh(z) => PayloadSlices::ZenohSlices(z.slices()),
        }
    }

    #[cfg(feature = "zenoh")]
    pub fn from_zbytes(zbytes: ZBytes) -> Self {
        Self {
            inner: PayloadInner::Zenoh(zbytes),
        }
    }

    #[cfg(feature = "zenoh")]
    pub fn into_zbytes(self) -> ZBytes {
        match self.inner {
            PayloadInner::Bytes(b) => ZBytes::from(b),
            PayloadInner::Zenoh(z) => z,
        }
    }
}

impl From<bytes::Bytes> for Payload {
    fn from(value: bytes::Bytes) -> Self {
        Payload::from_bytes(value)
    }
}

pub enum PayloadSlices<'a> {
    BytesSlice(std::iter::Once<&'a [u8]>),
    #[cfg(feature = "zenoh")]
    ZenohSlices(zenoh::bytes::ZBytesSliceIterator<'a>),
}

impl<'a> Iterator for PayloadSlices<'a> {
    type Item = &'a [u8];

    fn next(&mut self) -> Option<Self::Item> {
        match self {
            PayloadSlices::BytesSlice(iter) => iter.next(),
            #[cfg(feature = "zenoh")]
            PayloadSlices::ZenohSlices(iter) => iter.next(),
        }
    }
}

impl PartialEq<bytes::Bytes> for Payload {
    fn eq(&self, other: &bytes::Bytes) -> bool {
        match &self.inner {
            PayloadInner::Bytes(b) => b == other,
            #[cfg(feature = "zenoh")]
            PayloadInner::Zenoh(z) => z.to_bytes().as_ref() == other.as_ref(),
        }
    }
}

impl PartialEq<Payload> for bytes::Bytes {
    fn eq(&self, other: &Payload) -> bool {
        other == self
    }
}

/// Envelope for messages travelling through the transport layer.
#[derive(Clone)]
pub struct TopicMessage {
    key_expr: String,
    instance_id: String,
    core_node: String,
    link_id: String,
    payload: Payload,
}

impl TopicMessage {
    pub fn new(key_expr: &str, payload: impl Into<Payload>) -> Result<Self> {
        let parsed = ZenohWireFormat::parse_topic_keyexpr(key_expr)?;
        Ok(Self {
            key_expr: key_expr.to_string(),
            instance_id: parsed.instance_id,
            core_node: parsed.core_node,
            link_id: parsed.link_id,
            payload: payload.into(),
        })
    }

    /// Build a `TopicMessage` from already-parsed sender identity. Used for
    /// messages whose source isn't a topic keyexpr — currently the service
    /// request path, where the caller's `core_node` and `instance_id` come
    /// out of the Zenoh queryable selector and round-tripping them through
    /// a synthetic keyexpr only to re-parse would be wasted work. The
    /// internal `key_expr` and `link_id` fields are left empty since no
    /// consumer reads them for this path.
    pub fn from_parts(core_node: String, instance_id: String, payload: impl Into<Payload>) -> Self {
        Self {
            key_expr: String::new(),
            instance_id,
            core_node,
            link_id: String::new(),
            payload: payload.into(),
        }
    }

    #[cfg(feature = "zenoh")]
    pub fn from_zbytes(key_expr: &str, zbytes: ZBytes) -> Result<Self> {
        Self::new(key_expr, Payload::from_zbytes(zbytes))
    }

    pub fn instance_id(&self) -> &str {
        &self.instance_id
    }

    pub fn core_node(&self) -> &str {
        &self.core_node
    }

    /// Producer's bound link_id, parsed out of the inbound keyexpr at
    /// segment 8 of the topic publish format. Used by the consumer-side
    /// filter that drops messages whose producer link_id is already claimed
    /// by a sibling pinned subscription on the same `(name, tag)`. Empty
    /// when the message arrived via a non-topic path (e.g. service replies
    /// constructed via [`Self::from_parts`]).
    pub fn link_id(&self) -> &str {
        &self.link_id
    }

    pub fn payload(&self) -> &Payload {
        &self.payload
    }

    pub fn into_payload(self) -> Payload {
        self.payload
    }

    /// Raw incoming keyexpr. Wire-format-aware code (peppylib's service flow,
    /// adapters) uses this to address responses back to the request; no other
    /// consumer should touch this — the wire format is owned by
    /// `wire::zenoh_format::ZenohWireFormat`. This getter exists for service-flow
    /// plumbing only and may be removed when that flow stops threading raw
    /// keyexprs.
    pub fn key_expr(&self) -> &str {
        &self.key_expr
    }
}

/// Dispatches the Messenger calls to the appropriate backend without using the heap
#[allow(clippy::large_enum_variant)]
pub enum MessengerAdapter {
    #[cfg(feature = "zenoh")]
    Zenoh(ZenohAdapter),
    Mock(MockAdapter),
}

/// Main messaging implementation
pub struct Messenger {
    pub adapter: MessengerAdapter,
}

impl Messenger {
    pub fn new(adapter: MessengerAdapter) -> Self {
        Self { adapter }
    }

    pub fn messaging_port(&self) -> u16 {
        self.get_host().port()
    }

    /// Returns a lock-free [`RouterHealthChecker`] for the router watchdog, or
    /// `None` for backends without a restartable router (e.g. the mock).
    #[cfg(feature = "router")]
    pub fn router_health_checker(&self) -> Option<RouterHealthChecker> {
        match &self.adapter {
            MessengerAdapter::Zenoh(adapter) => Some(adapter.router_health_checker()),
            MessengerAdapter::Mock(_) => None,
        }
    }

    /// Pre-bind a per-topic publisher. The returned [`MessengerPublisher`]
    /// publishes to the same wire keyexpr as [`MessengerBackend::publish_topic`]
    /// for the same `sender`, but skips the central `Arc<Mutex<Messenger>>`
    /// lock that all other operations contend on — useful for periodic /
    /// per-frame publish loops.
    pub fn declare_topic_publisher(
        &self,
        sender: &TopicWireSender,
        qos: PublisherQoS,
    ) -> Result<MessengerPublisher> {
        match &self.adapter {
            #[cfg(feature = "zenoh")]
            MessengerAdapter::Zenoh(adapter) => Ok(MessengerPublisher::Zenoh(
                adapter.declare_topic_publisher(sender, qos)?,
            )),
            MessengerAdapter::Mock(adapter) => Ok(MessengerPublisher::Mock(
                adapter.declare_topic_publisher(sender, qos),
            )),
        }
    }

    /// Pre-bind a per-goal action-feedback publisher. Mirrors
    /// [`declare_topic_publisher`](Self::declare_topic_publisher) for action
    /// feedback streams keyed by `goal_id`. `link_id` is the link_id parsed
    /// from the goal's request keyexpr; feedback for a goal must publish
    /// under the same wire identity the consumer subscribed for, even when
    /// the producer is bound to multiple link_ids.
    pub fn declare_action_feedback_publisher(
        &self,
        recv: &ActionWireReceiver,
        link_id: &str,
        goal_id: &str,
        qos: PublisherQoS,
    ) -> Result<MessengerPublisher> {
        match &self.adapter {
            #[cfg(feature = "zenoh")]
            MessengerAdapter::Zenoh(adapter) => Ok(MessengerPublisher::Zenoh(
                adapter.declare_action_feedback_publisher(recv, link_id, goal_id, qos)?,
            )),
            MessengerAdapter::Mock(adapter) => Ok(MessengerPublisher::Mock(
                adapter.declare_action_feedback_publisher(recv, link_id, goal_id, qos),
            )),
        }
    }
}

/// Per-topic publisher handle that bypasses the central `Messenger` mutex.
/// Construct via [`Messenger::declare_topic_publisher`] (or
/// [`Messenger::declare_action_feedback_publisher`] for action feedback).
pub enum MessengerPublisher {
    #[cfg(feature = "zenoh")]
    Zenoh(ZenohPublisher),
    Mock(MockPublisher),
}

impl MessengerPublisher {
    pub async fn publish(&self, payload: bytes::Bytes) -> Result<()> {
        match self {
            #[cfg(feature = "zenoh")]
            MessengerPublisher::Zenoh(p) => p.publish(payload).await,
            MessengerPublisher::Mock(p) => p.publish(payload).await,
        }
    }
}

macro_rules! dispatch {
    ($adapter:expr, $method:ident $(, $arg:expr)*) => {
        match $adapter {
            #[cfg(feature = "zenoh")]
            MessengerAdapter::Zenoh(adapter) => adapter.$method($($arg),*).await,
            MessengerAdapter::Mock(adapter) => adapter.$method($($arg),*).await,
        }
    };
}

macro_rules! dispatch_sync {
    ($adapter:expr, $method:ident $(, $arg:expr)*) => {
        match $adapter {
            #[cfg(feature = "zenoh")]
            MessengerAdapter::Zenoh(adapter) => adapter.$method($($arg),*),
            MessengerAdapter::Mock(adapter) => adapter.$method($($arg),*),
        }
    };
}

impl MessengerBackend for Messenger {
    async fn start_session(&mut self) -> Result<()> {
        dispatch!(&mut self.adapter, start_session)
    }

    async fn stop_session(&mut self) -> Result<()> {
        dispatch!(&mut self.adapter, stop_session)
    }

    async fn subscribe_topic(
        &self,
        recv: &TopicWireReceiver,
        qos: SubscriberQoS,
    ) -> Result<Subscription> {
        dispatch!(&self.adapter, subscribe_topic, recv, qos)
    }

    async fn publish_topic(
        &mut self,
        sender: &TopicWireSender,
        payload: Payload,
        qos: PublisherQoS,
        is_primary: bool,
    ) -> Result<()> {
        dispatch!(
            &mut self.adapter,
            publish_topic,
            sender,
            payload,
            qos,
            is_primary
        )
    }

    async fn listen_service(&self, recv: &ServiceWireReceiver) -> Result<ServiceQueryable> {
        dispatch!(&self.adapter, listen_service, recv)
    }

    async fn call_service(
        &self,
        sender: &ServiceWireSender,
        payload: Payload,
        kind: ServiceQueryKind,
        timeout: Option<std::time::Duration>,
    ) -> Result<ReplyStream> {
        dispatch!(&self.adapter, call_service, sender, payload, kind, timeout)
    }

    async fn subscribe_action_feedback(
        &self,
        sender: &ActionWireSender,
        goal_id: &str,
        qos: SubscriberQoS,
    ) -> Result<Subscription> {
        dispatch!(
            &self.adapter,
            subscribe_action_feedback,
            sender,
            goal_id,
            qos
        )
    }

    async fn start_router(&mut self) -> Result<()> {
        dispatch!(&mut self.adapter, start_router)
    }

    async fn stop_router(&mut self) -> Result<()> {
        dispatch!(&mut self.adapter, stop_router)
    }

    fn get_host(&self) -> SocketAddr {
        dispatch_sync!(&self.adapter, get_host)
    }
}
