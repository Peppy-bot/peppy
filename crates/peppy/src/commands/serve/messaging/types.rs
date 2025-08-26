use std::fmt;
use std::thread::{self, JoinHandle};

#[cfg(test)]
use super::adapters::mock::MockAdapter;
use super::adapters::zenoh::ZenohAdapter;
use crate::commands::serve::types::{CommandContext, ServeAsyncCommand};
use crate::{Error, Result};

pub trait MessengerBackend: Send + Sync {
    async fn start_router(&mut self) -> Result<()>;
    async fn connect(&mut self) -> Result<()>;
    async fn publish(&self, message: Message) -> Result<()>;
    async fn subscribe(&self, topic: &str) -> Result<Subscription>;
    async fn shutdown(&mut self) -> Result<()>;
}

pub struct Subscription {
    // stream of messages; could be tokio::sync::mpsc::Receiver or a Stream
    pub rx: tokio::sync::mpsc::Receiver<Message>,
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
        host: String,
        port: u16,
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
            "zenoh" => {
                let host = context.host.clone().ok_or(Error::MissingEngineConfig)?;
                let port = context.port.ok_or(Error::MissingEngineConfig)?;
                Ok(Engine::Zenoh { host, port })
            }
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

enum MessengerAdapter {
    Zenoh(ZenohAdapter),
    #[cfg(test)]
    Mock(MockAdapter),
}

impl Messenger {
    pub fn new(context: CommandContext) -> Result<Self> {
        let engine = Engine::from_context(&context)?;
        let adapter = match engine {
            Engine::Zenoh { host, port } => MessengerAdapter::Zenoh(ZenohAdapter::new(host, port)),
            #[cfg(test)]
            Engine::Mock => MessengerAdapter::Mock(MockAdapter::default()),
        };
        Ok(Self { adapter, context })
    }

    #[tokio::main]
    async fn run_router(mut self) -> Result<()> {
        self.start_router().await?;
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
    async fn start_router(&mut self) -> Result<()> {
        dispatch!(&mut self.adapter, start_router)
    }

    async fn connect(&mut self) -> Result<()> {
        dispatch!(&mut self.adapter, connect)
    }

    async fn publish(&self, message: Message) -> Result<()> {
        dispatch!(&self.adapter, publish, message)
    }

    async fn subscribe(&self, topic: &str) -> Result<Subscription> {
        dispatch!(&self.adapter, subscribe, topic)
    }

    async fn shutdown(&mut self) -> Result<()> {
        dispatch!(&mut self.adapter, shutdown)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_only_zenoh_engine_allowed() {
        // Test that zenoh engine is accepted
        let context = CommandContext::new(
            "zenoh".to_string(),
            Some("localhost".to_string()),
            Some(7447),
        );
        let result = Engine::from_context(&context);
        assert!(result.is_ok());
        assert_eq!(
            result.unwrap(),
            Engine::Zenoh {
                host: "localhost".to_string(),
                port: 7447
            }
        );

        // Test that mock engine is allowed in test mode
        let context = CommandContext::new("mock".to_string(), None, None);
        let result = Engine::from_context(&context);
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), Engine::Mock);

        // Test that any other engine is rejected
        let context = CommandContext::new(
            "rabbitmq".to_string(),
            Some("localhost".to_string()),
            Some(5672),
        );
        let result = Engine::from_context(&context);
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), Error::UnsupportedEngine));
    }

    #[test]
    fn test_zenoh_requires_config() {
        // Test that zenoh requires host and port
        let context = CommandContext::new("zenoh".to_string(), None, Some(8080));
        let result = Engine::from_context(&context);
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), Error::MissingEngineConfig));

        let context = CommandContext::new("zenoh".to_string(), Some("localhost".to_string()), None);
        let result = Engine::from_context(&context);
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), Error::MissingEngineConfig));

        let context = CommandContext::new("zenoh".to_string(), None, None);
        let result = Engine::from_context(&context);
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), Error::MissingEngineConfig));
    }
}
