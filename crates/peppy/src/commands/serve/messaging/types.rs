use std::fmt;
use std::future::Future;
use std::path::PathBuf;
use std::thread::{self, JoinHandle};

#[cfg(test)]
use super::adapters::mock::MockAdapter;
use super::adapters::zenoh::ZenohAdapter;
use crate::commands::serve::types::{CommandContext, ServeAsyncCommand};
use crate::{Error, Result};

pub trait MessengerBackend {
    /// Starts the router in background and immediately return
    fn start_router(&mut self) -> Result<()>;

    /// Connects to the router started with `start_router`
    fn connect(&mut self) -> Result<()>;

    /// Shuts down the router instance
    fn shutdown(&mut self) -> Result<()>;

    /// Publish a message to a topic
    fn publish(&self, message: Message) -> impl Future<Output = Result<()>> + Send; // async equivalent for trait

    /// Subscribes to a topic
    fn subscribe(&self, topic: &str) -> impl Future<Output = Result<Subscription>> + Send; // async equivalent for trait
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
        config: PathBuf,
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
                let config = context
                    .config_path
                    .clone()
                    .ok_or(Error::MissingEngineConfig)?;
                Ok(Engine::Zenoh { config })
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
            Engine::Zenoh { config } => MessengerAdapter::Zenoh(ZenohAdapter::new(Some(config))?),
            #[cfg(test)]
            Engine::Mock => MessengerAdapter::Mock(MockAdapter::default()),
        };
        Ok(Self { adapter, context })
    }

    #[tokio::main]
    async fn run_router(mut self) -> Result<()> {
        self.start_router()?;
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
    fn start_router(&mut self) -> Result<()> {
        match &mut self.adapter {
            MessengerAdapter::Zenoh(adapter) => adapter.start_router(),
            #[cfg(test)]
            MessengerAdapter::Mock(adapter) => adapter.start_router(),
        }
    }

    fn connect(&mut self) -> Result<()> {
        match &mut self.adapter {
            MessengerAdapter::Zenoh(adapter) => adapter.connect(),
            #[cfg(test)]
            MessengerAdapter::Mock(adapter) => adapter.connect(),
        }
    }

    async fn publish(&self, message: Message) -> Result<()> {
        dispatch!(&self.adapter, publish, message)
    }

    async fn subscribe(&self, topic: &str) -> Result<Subscription> {
        dispatch!(&self.adapter, subscribe, topic)
    }

    fn shutdown(&mut self) -> Result<()> {
        match &mut self.adapter {
            MessengerAdapter::Zenoh(adapter) => adapter.shutdown(),
            #[cfg(test)]
            MessengerAdapter::Mock(adapter) => adapter.shutdown(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::adapters::zenoh::{Protocol, ZenohConfigTemplate};

    use super::*;
    use askama::Template;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn test_only_zenoh_engine_allowed() {
        // Create a temporary directory for test config
        let temp_dir = tempdir().unwrap();
        let config_path = temp_dir.path().join("test_config.json5");

        // Render the template with test values
        let template = ZenohConfigTemplate {
            host: "localhost".to_string(),
            port: 7447,
            protocol: Protocol::Tcp,
        };
        let rendered_config = template.render().unwrap();

        // Write the rendered config
        fs::write(&config_path, rendered_config).unwrap();

        // Test that zenoh engine is accepted with config
        let context = CommandContext::new("zenoh".to_string(), Some(config_path.clone()));
        let result = Engine::from_context(&context);
        assert!(result.is_ok());
        assert_eq!(
            result.unwrap(),
            Engine::Zenoh {
                config: config_path
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

    #[test]
    fn test_zenoh_requires_config() {
        // Test that zenoh requires a config path
        let context = CommandContext::new("zenoh".to_string(), None);
        let result = Engine::from_context(&context);
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), Error::MissingEngineConfig));

        // Test that zenoh with config path succeeds
        let temp_dir = tempdir().unwrap();
        let config_path = temp_dir.path().join("test_config.json5");

        // Render the template
        let template = ZenohConfigTemplate {
            host: "localhost".to_string(),
            port: 7447,
            protocol: Protocol::Tcp,
        };
        let rendered_config = template.render().unwrap();
        fs::write(&config_path, rendered_config).unwrap();

        let context = CommandContext::new("zenoh".to_string(), Some(config_path));
        let result = Engine::from_context(&context);
        assert!(result.is_ok());
    }
}
