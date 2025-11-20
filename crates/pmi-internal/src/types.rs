use super::adapters::mock::MockAdapter;
use super::error::Result;
use regex::Regex;
#[cfg(feature = "zenoh")]
use std::borrow::Cow;
use std::future::Future;
#[cfg(feature = "zenoh")]
use zenoh::bytes::ZBytes;

#[cfg(feature = "zenoh")]
use super::adapters::zenoh::ZenohAdapter;

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

/// Defines the messaging interface
pub trait MessengerBackend {
    /// Initialize the pubsub session
    fn start_session(&mut self) -> impl Future<Output = Result<()>> + Send; // async equivalent for trait

    /// Shuts down the pubsub session
    fn stop_session(self) -> impl Future<Output = Result<()>> + Send; // async equivalent for trait

    /// Publish a message to a topic
    fn publish(
        &mut self,
        message: Message,
        qos: PublisherQoS,
    ) -> impl Future<Output = Result<()>> + Send; // async equivalent for trait

    /// Subscribes to a topic
    fn subscribe(
        &self,
        topic: &str,
        qos: SubscriberQoS,
    ) -> impl Future<Output = Result<Subscription>> + Send; // async equivalent for trait

    /// Returns whether there are active subscribers matching the provided topic.
    fn has_matching_subscribers(&self, topic: &str) -> impl Future<Output = Result<bool>> + Send;

    /// Starts the router in background and immediately return for engines that uses a router.
    /// The router should only be started if the lib is intended to connect nodes together
    fn start_router(&mut self) -> impl Future<Output = Result<()>> + Send;

    /// Stops the router
    fn stop_router(&mut self) -> impl Future<Output = Result<()>> + Send;
}

/// Handles message receiving and cleanup
pub struct Subscription {
    // stream of messages; could be tokio::sync::mpsc::Receiver or a Stream
    pub rx: tokio::sync::mpsc::Receiver<RawMessage>,
    // Handle to abort the background task when subscription is dropped
    abort_handle: tokio::task::AbortHandle,
}

impl Subscription {
    pub fn new(
        rx: tokio::sync::mpsc::Receiver<RawMessage>,
        abort_handle: tokio::task::AbortHandle,
    ) -> Self {
        Self { rx, abort_handle }
    }

    pub async fn on_next_message(&mut self) -> Option<RawMessage> {
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

pub struct RawMessage {
    key_expr: String,
    instance_id: Option<String>,
    payload: Payload,
}

impl RawMessage {
    pub fn new(key_expr: &str, payload: impl Into<Payload>) -> Self {
        let instance_id = RawMessage::extract_instance_id(key_expr);
        Self {
            key_expr: key_expr.to_string(),
            instance_id: instance_id,
            payload: payload.into(),
        }
    }

    fn extract_instance_id(key_expr: &str) -> Option<String> {
        let re = Regex::new(r"<INSTANCE_ID:([^>]+)>").ok()?;
        re.captures_iter(key_expr)
            .last()
            .and_then(|captures| captures.get(1))
            .map(|value| value.as_str().to_string())
    }

    pub fn instance_id(&self) -> Option<&str> {
        self.instance_id.as_deref()
    }

    pub fn payload(&self) -> &Payload {
        &self.payload
    }

    pub fn into_payload(self) -> Payload {
        self.payload
    }

    pub fn into_parts(self) -> (Option<String>, Payload) {
        (self.instance_id, self.payload)
    }

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

impl MessengerBackend for Messenger {
    async fn start_session(&mut self) -> Result<()> {
        dispatch!(&mut self.adapter, start_session)
    }

    async fn publish(&mut self, message: Message, qos: PublisherQoS) -> Result<()> {
        dispatch!(&mut self.adapter, publish, message, qos)
    }

    async fn subscribe(&self, topic: &str, qos: SubscriberQoS) -> Result<Subscription> {
        dispatch!(&self.adapter, subscribe, topic, qos)
    }

    async fn has_matching_subscribers(&self, topic: &str) -> Result<bool> {
        dispatch!(&self.adapter, has_matching_subscribers, topic)
    }

    async fn stop_session(self) -> Result<()> {
        dispatch!(self.adapter, stop_session)
    }

    async fn start_router(&mut self) -> Result<()> {
        dispatch!(&mut self.adapter, start_router)
    }

    async fn stop_router(&mut self) -> Result<()> {
        dispatch!(&mut self.adapter, stop_router)
    }
}
