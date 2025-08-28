use std::fmt;
use std::future::Future;
use std::path::PathBuf;
use std::thread::{self, JoinHandle};

#[cfg(test)]
use super::adapters::mock::MockAdapter;
use super::adapters::zenoh::ZenohAdapter;
use crate::commands::serve::types::{CommandContext, ServeAsyncCommand};
use crate::{Error, Result};

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

pub trait MessengerBackend {
    /// Starts the router in background and immediately return
    fn init(&mut self) -> impl Future<Output = Result<()>> + Send; // async equivalent for trait

    /// Shuts down the router instance
    fn shutdown(&mut self) -> impl Future<Output = Result<()>> + Send; // async equivalent for trait

    /// Publish a message to a topic
    fn publish(&mut self, message: Message) -> impl Future<Output = Result<()>> + Send; // async equivalent for trait

    /// Subscribes to a topic
    fn subscribe(
        &self,
        topic: &str,
        throughput_mode: ThroughputMode,
    ) -> impl Future<Output = Result<Subscription>> + Send; // async equivalent for trait
}

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

#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Engine {
    Zenoh {
        config: Option<PathBuf>,
    },
    #[cfg(test)]
    Mock,
}

impl fmt::Display for Engine {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Engine::Zenoh { .. } => write!(f, "zenoh"),
            #[cfg(test)]
            Engine::Mock => write!(f, "mock"),
        }
    }
}

impl Engine {
    pub fn from_context(context: &CommandContext) -> Result<Self> {
        match context.engine.as_str() {
            "zenoh" => Ok(Engine::Zenoh {
                config: context.config_path.clone(),
            }),
            #[cfg(test)]
            "mock" => Ok(Engine::Mock),
            _ => Err(Error::UnsupportedEngine),
        }
    }
}

pub struct Messenger {
    adapter: MessengerAdapter,
    context: CommandContext,
}

#[allow(clippy::large_enum_variant)]
enum MessengerAdapter {
    Zenoh(ZenohAdapter),
    #[cfg(test)]
    Mock(MockAdapter),
}

impl Messenger {
    pub fn new(context: CommandContext) -> Result<Self> {
        let engine = Engine::from_context(&context)?;
        let adapter = match engine {
            Engine::Zenoh { config } => MessengerAdapter::Zenoh(ZenohAdapter::new(config)?),
            #[cfg(test)]
            Engine::Mock => MessengerAdapter::Mock(MockAdapter::default()),
        };
        Ok(Self { adapter, context })
    }

    #[tokio::main]
    async fn run_router(mut self) -> Result<()> {
        self.init().await?;
        Ok(())
    }
}

impl ServeAsyncCommand for Messenger {
    fn execute_async(&self) -> Result<JoinHandle<Result<()>>> {
        let context = self.context.clone();

        let handle = thread::spawn(move || {
            let messenger = Messenger::new(context)?;
            messenger.run_router()
        });

        Ok(handle)
    }
}

macro_rules! dispatch {
    ($adapter:expr, $method:ident $(, $arg:expr)*) => {
        match $adapter {
            MessengerAdapter::Zenoh(adapter) => adapter.$method($($arg),*).await,
            #[cfg(test)]
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

    async fn shutdown(&mut self) -> Result<()> {
        dispatch!(&mut self.adapter, shutdown)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn test_only_zenoh_engine_allowed() {
        // Create a temporary directory for test config
        let temp_dir = tempdir().unwrap();
        let config_path = temp_dir.path().join("test_config.json5");

        // Test that zenoh engine is accepted with config
        let context = CommandContext::new("zenoh".to_string(), Some(config_path.clone()));
        let result = Engine::from_context(&context);
        assert!(result.is_ok());
        assert_eq!(
            result.unwrap(),
            Engine::Zenoh {
                config: Some(config_path)
            }
        );

        // Test that mock engine is allowed in test mode
        let context = CommandContext::new("mock".to_string(), None);
        let result = Engine::from_context(&context);
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), Engine::Mock);

        // Test that any other engine is rejected
        let context = CommandContext::new(
            "rabbitmq".to_string(),
            Some(PathBuf::from("some/config.json")),
        );
        let result = Engine::from_context(&context);
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), Error::UnsupportedEngine));
    }
}
