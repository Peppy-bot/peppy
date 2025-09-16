use std::fmt;
use std::future::Future;
use std::path::PathBuf;

use super::adapters::mock::MockAdapter;
use super::error::{Error, Result};

#[cfg(feature = "zenoh")]
use super::adapters::zenoh::ZenohAdapter;

#[derive(Clone)]
pub struct MessagingEngineContext {
    pub engine: String,
    // Specific to Zenoh
    pub zenohd_config_path: Option<PathBuf>,
}

impl MessagingEngineContext {
    pub fn new(engine: String, zenohd_config_path: Option<PathBuf>) -> Self {
        Self {
            engine,
            zenohd_config_path,
        }
    }
}

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

/// Lists the different messaging backends
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Engine {
    #[cfg(feature = "zenoh")]
    Zenoh,
    Mock,
}

impl fmt::Display for Engine {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            #[cfg(feature = "zenoh")]
            Engine::Zenoh => write!(f, "zenoh"),
            Engine::Mock => write!(f, "mock"),
        }
    }
}

impl Engine {
    pub fn from_context(context: &MessagingEngineContext) -> Result<Self> {
        match context.engine.as_str() {
            #[cfg(feature = "zenoh")]
            "zenoh" => Ok(Engine::Zenoh),
            "mock" => Ok(Engine::Mock),
            _ => Err(Error::UnsupportedEngine),
        }
    }
}

/// Main messaging implementation
pub struct Messenger {
    adapter: MessengerAdapter,
    pub context: MessagingEngineContext,
}

impl Messenger {
    pub fn new(context: MessagingEngineContext) -> Result<Self> {
        let engine = Engine::from_context(&context)?;
        let adapter = match engine {
            #[cfg(feature = "zenoh")]
            Engine::Zenoh => {
                MessengerAdapter::Zenoh(ZenohAdapter::new(context.zenohd_config_path.clone())?)
            }
            Engine::Mock => MessengerAdapter::Mock(MockAdapter::default()),
        };
        Ok(Self { adapter, context })
    }
}

/// Dispatches the Messenger calls to the appropriate backend without using the heap
#[allow(clippy::large_enum_variant)]
enum MessengerAdapter {
    #[cfg(feature = "zenoh")]
    Zenoh(ZenohAdapter),
    Mock(MockAdapter),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_mock_engine() {
        // Test that mock engine is allowed in test mode
        let context = MessagingEngineContext::new("mock".to_string(), None);
        let result = Engine::from_context(&context);
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), Engine::Mock);

        // Test that any other engine is rejected
        let context = MessagingEngineContext::new("rabbitmq".to_string(), None);
        let result = Engine::from_context(&context);
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), Error::UnsupportedEngine));
    }

    #[cfg(feature = "zenoh")]
    #[test]
    fn test_zenoh_engine() {
        // Test that zenoh engine is accepted with config
        let context = MessagingEngineContext::new("zenoh".to_string(), None);
        let result = Engine::from_context(&context);
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), Engine::Zenoh {});
    }
}
