use super::adapters::mock::{MockAdapter, MockPublisher};
use super::error::Result;
use super::wire::format::WireFormat;
use super::wire::{
    ActionWireReceiver, ActionWireSender, ServiceWireReceiver, ServiceWireSender,
    TopicWireReceiver, TopicWireSender,
};
use config::node::QoSProfile;
use std::borrow::Cow;
use std::future::Future;
use std::net::SocketAddr;
#[cfg(feature = "zenoh")]
use zenoh::bytes::ZBytes;

#[cfg(feature = "zenoh")]
use super::adapters::zenoh::{ZenohAdapter, ZenohPublisher};

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

/// Defines the messaging interface.
///
/// All methods take addressing structs from [`crate::wire`] rather than raw
/// keyexpressions. Each adapter is responsible for formatting them into its
/// own wire form internally. Only the per-transport `*_wire.rs` modules are
/// authorized to produce raw keyexpressions.
pub trait MessengerBackend {
    /// Initialize the pubsub session.
    fn start_session(&mut self) -> impl Future<Output = Result<()>> + Send;

    /// Shuts down the pubsub session.
    fn stop_session(self) -> impl Future<Output = Result<()>> + Send;

    // ─── Topics ───────────────────────────────────────────────────────────

    /// Subscribe to a topic.
    fn subscribe_topic(
        &self,
        recv: &TopicWireReceiver,
        qos: SubscriberQoS,
    ) -> impl Future<Output = Result<Subscription>> + Send;

    /// Publish a one-shot topic message.
    fn publish_topic(
        &mut self,
        sender: &TopicWireSender,
        payload: Payload,
        qos: PublisherQoS,
    ) -> impl Future<Output = Result<()>> + Send;

    // ─── Services ─────────────────────────────────────────────────────────

    /// Subscribe to all four broadcast-Cartesian listen patterns for a service.
    fn listen_service(
        &self,
        recv: &ServiceWireReceiver,
    ) -> impl Future<Output = Result<[Subscription; 4]>> + Send;

    /// Send a service request and subscribe to the response correlated by
    /// `request_id`. Returns the response subscription.
    fn open_service_call(
        &mut self,
        sender: &ServiceWireSender,
        request_id: &str,
        payload: Payload,
    ) -> impl Future<Output = Result<Subscription>> + Send;

    /// Server-side response publish. The adapter parses `received_request` to
    /// extract the caller's addressing and request_id, then publishes the
    /// payload back to the caller.
    fn publish_service_response(
        &mut self,
        recv: &ServiceWireReceiver,
        received_request: &str,
        payload: Payload,
    ) -> impl Future<Output = Result<()>> + Send;

    /// Parses a received request keyexpr against the receiver's expected
    /// shape and returns the request_id. Server-side handlers use this to
    /// correlate requests with responses without seeing raw keyexpressions.
    fn parse_service_request_id(
        &self,
        recv: &ServiceWireReceiver,
        received_request: &str,
    ) -> Result<String>;

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

/// Handles message receiving and cleanup
pub struct Subscription {
    // stream of messages; could be tokio::sync::mpsc::Receiver or a Stream
    pub rx: tokio::sync::mpsc::Receiver<TopicMessage>,
    // Handle to abort the background task when subscription is dropped
    abort_handle: tokio::task::AbortHandle,
}

impl Subscription {
    pub fn new(
        rx: tokio::sync::mpsc::Receiver<TopicMessage>,
        abort_handle: tokio::task::AbortHandle,
    ) -> Self {
        Self { rx, abort_handle }
    }

    pub async fn on_next_message(&mut self) -> Option<TopicMessage> {
        self.rx.recv().await
    }
}

impl Drop for Subscription {
    fn drop(&mut self) {
        // Abort the background task to prevent resource leaks
        self.abort_handle.abort();
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
pub struct TopicMessage {
    key_expr: String,
    instance_id: String,
    core_node: String,
    payload: Payload,
}

impl TopicMessage {
    pub fn new(key_expr: &str, payload: impl Into<Payload>) -> Result<Self> {
        let parsed = WireFormat::parse_topic_keyexpr(key_expr)?;
        Ok(Self {
            key_expr: key_expr.to_string(),
            instance_id: parsed.instance_id,
            core_node: parsed.core_node,
            payload: payload.into(),
        })
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

    pub fn payload(&self) -> &Payload {
        &self.payload
    }

    pub fn into_payload(self) -> Payload {
        self.payload
    }

    /// Raw incoming keyexpr. Wire-format-aware code (peppylib's service flow,
    /// adapters) uses this to address responses back to the request; no other
    /// consumer should touch this — the wire format is owned by
    /// `wire::format::WireFormat`. This getter exists for service-flow
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
    /// feedback streams keyed by `goal_id`.
    pub fn declare_action_feedback_publisher(
        &self,
        recv: &ActionWireReceiver,
        goal_id: &str,
        qos: PublisherQoS,
    ) -> Result<MessengerPublisher> {
        match &self.adapter {
            #[cfg(feature = "zenoh")]
            MessengerAdapter::Zenoh(adapter) => Ok(MessengerPublisher::Zenoh(
                adapter.declare_action_feedback_publisher(recv, goal_id, qos)?,
            )),
            MessengerAdapter::Mock(adapter) => Ok(MessengerPublisher::Mock(
                adapter.declare_action_feedback_publisher(recv, goal_id, qos),
            )),
        }
    }
}

/// Per-topic publisher handle that bypasses the central `Messenger` mutex.
/// Construct via [`Messenger::declare_publisher`].
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

    async fn stop_session(self) -> Result<()> {
        dispatch!(self.adapter, stop_session)
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
    ) -> Result<()> {
        dispatch!(&mut self.adapter, publish_topic, sender, payload, qos)
    }

    async fn listen_service(&self, recv: &ServiceWireReceiver) -> Result<[Subscription; 4]> {
        dispatch!(&self.adapter, listen_service, recv)
    }

    async fn open_service_call(
        &mut self,
        sender: &ServiceWireSender,
        request_id: &str,
        payload: Payload,
    ) -> Result<Subscription> {
        dispatch!(
            &mut self.adapter,
            open_service_call,
            sender,
            request_id,
            payload
        )
    }

    async fn publish_service_response(
        &mut self,
        recv: &ServiceWireReceiver,
        received_request: &str,
        payload: Payload,
    ) -> Result<()> {
        dispatch!(
            &mut self.adapter,
            publish_service_response,
            recv,
            received_request,
            payload
        )
    }

    fn parse_service_request_id(
        &self,
        recv: &ServiceWireReceiver,
        received_request: &str,
    ) -> Result<String> {
        dispatch_sync!(
            &self.adapter,
            parse_service_request_id,
            recv,
            received_request
        )
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
