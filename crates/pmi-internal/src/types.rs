use std::future::Future;

use super::adapters::mock::MockAdapter;
use super::error::Result;

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

    /// Starts the router in background and immediately return for engines that uses a router.
    /// The router should only be started if the lib is intended to connect nodes together
    fn start_router(&mut self) -> impl Future<Output = Result<()>> + Send;

    /// Stops the router
    fn stop_router(&mut self) -> impl Future<Output = Result<()>> + Send;
}

/// Handles message receiving and cleanup
pub struct Subscription {
    // stream of messages; could be tokio::sync::mpsc::Receiver or a Stream
    pub rx: tokio::sync::mpsc::Receiver<Message>,
    // Handle to abort the background task when subscription is dropped
    abort_handle: tokio::task::AbortHandle,
}

impl Subscription {
    pub fn new(
        rx: tokio::sync::mpsc::Receiver<Message>,
        abort_handle: tokio::task::AbortHandle,
    ) -> Self {
        Self { rx, abort_handle }
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
    pub topic: String,
    pub payload: bytes::Bytes,
}

impl Message {
    pub fn new(topic: &str, payload: &[u8]) -> Self {
        Self {
            topic: topic.to_string(),
            payload: bytes::Bytes::from(payload.to_vec()),
        }
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
