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
    pub config_path: Option<PathBuf>,
}

impl MessagingEngineContext {
    pub fn new(engine: String, config_path: Option<PathBuf>) -> Self {
        Self {
            engine,
            config_path,
        }
    }
}

/// Configuration for channel buffer sizing based on expected throughput
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThroughputMode {
    /// Low throughput mode with smaller buffer (32 messages)
    /// Suitable for control messages or low-frequency updates
    LowThroughput,
    /// High throughput mode with larger buffer (1024 messages)
    /// Suitable for data streaming or high-frequency sensor data
    HighThroughput,
}

impl ThroughputMode {
    /// Returns the channel buffer size for this throughput mode
    pub fn channel_size(&self) -> usize {
        match self {
            ThroughputMode::LowThroughput => 32,
            ThroughputMode::HighThroughput => 1024,
        }
    }
}

/// Defines the messaging interface
pub trait MessengerBackend {
    /// Starts the router in background and immediately return
    fn init(&mut self) -> impl Future<Output = Result<()>> + Send; // async equivalent for trait

    /// Shuts down the router instance
    fn shutdown(self) -> impl Future<Output = Result<()>> + Send; // async equivalent for trait

    /// Publish a message to a topic
    fn publish(&mut self, message: Message) -> impl Future<Output = Result<()>> + Send; // async equivalent for trait

    /// Subscribes to a topic
    fn subscribe(
        &self,
        topic: &str,
        throughput_mode: ThroughputMode,
    ) -> impl Future<Output = Result<Subscription>> + Send; // async equivalent for trait
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
    Zenoh {
        config: Option<PathBuf>,
    },
    Mock,
}

impl fmt::Display for Engine {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            #[cfg(feature = "zenoh")]
            Engine::Zenoh { .. } => write!(f, "zenoh"),
            Engine::Mock => write!(f, "mock"),
        }
    }
}

impl Engine {
    pub fn from_context(context: &MessagingEngineContext) -> Result<Self> {
        match context.engine.as_str() {
            #[cfg(feature = "zenoh")]
            "zenoh" => Ok(Engine::Zenoh {
                config: context.config_path.clone(),
            }),
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
            Engine::Zenoh { config } => MessengerAdapter::Zenoh(ZenohAdapter::new(config)?),
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
    async fn init(&mut self) -> Result<()> {
        dispatch!(&mut self.adapter, init)
    }

    async fn publish(&mut self, message: Message) -> Result<()> {
        dispatch!(&mut self.adapter, publish, message)
    }

    async fn subscribe(
        &self,
        topic: &str,
        throughput_mode: ThroughputMode,
    ) -> Result<Subscription> {
        dispatch!(&self.adapter, subscribe, topic, throughput_mode)
    }

    async fn shutdown(self) -> Result<()> {
        dispatch!(self.adapter, shutdown)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn test_mock_engine() {
        // Test that mock engine is allowed in test mode
        let context = MessagingEngineContext::new("mock".to_string(), None);
        let result = Engine::from_context(&context);
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), Engine::Mock);

        // Test that any other engine is rejected
        let context = MessagingEngineContext::new(
            "rabbitmq".to_string(),
            Some(PathBuf::from("some/config.json")),
        );
        let result = Engine::from_context(&context);
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), Error::UnsupportedEngine));
    }

    #[cfg(feature = "zenoh")]
    #[test]
    fn test_zenoh_engine() {
        // Create a temporary directory for test config
        let temp_dir = tempdir().unwrap();
        let config_path = temp_dir.path().join("test_config.json5");

        // Test that zenoh engine is accepted with config
        let context = MessagingEngineContext::new("zenoh".to_string(), Some(config_path.clone()));
        let result = Engine::from_context(&context);
        assert!(result.is_ok());
        assert_eq!(
            result.unwrap(),
            Engine::Zenoh {
                config: Some(config_path)
            }
        );
    }
}
