use std::fmt;

#[cfg(test)]
use super::adapters::mock::MockAdapter;
use super::adapters::zenoh::ZenohAdapter;
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
    pub fn from_str_with_config(s: &str, host: Option<String>, port: Option<u16>) -> Result<Self> {
        match s {
            "zenoh" => {
                let host = host.ok_or(Error::MissingEngineConfig)?;
                let port = port.ok_or(Error::MissingEngineConfig)?;
                Ok(Engine::Zenoh { host, port })
            }
            #[cfg(test)]
            "mock" => Ok(Engine::Mock),
            _ => Err(Error::UnsupportedEngine),
        }
    }
}

pub enum Messenger {
    Zenoh(ZenohAdapter),
    #[cfg(test)]
    Mock(MockAdapter),
}

macro_rules! dispatch {
    ($self:expr, $method:ident $(, $arg:expr)*) => {
        match $self {
            Messenger::Zenoh(adapter) => adapter.$method($($arg),*).await,
            #[cfg(test)]
            Messenger::Mock(adapter) => adapter.$method($($arg),*).await,
        }
    };
}

impl MessengerBackend for Messenger {
    async fn start_router(&mut self) -> Result<()> {
        dispatch!(self, start_router)
    }

    async fn connect(&mut self) -> Result<()> {
        dispatch!(self, connect)
    }

    async fn publish(&self, message: Message) -> Result<()> {
        dispatch!(self, publish, message)
    }

    async fn subscribe(&self, topic: &str) -> Result<Subscription> {
        dispatch!(self, subscribe, topic)
    }

    async fn shutdown(&mut self) -> Result<()> {
        dispatch!(self, shutdown)
    }
}

impl Messenger {
    pub fn from_engine(engine: Engine) -> Self {
        match engine {
            Engine::Zenoh { host, port } => Messenger::Zenoh(ZenohAdapter::new(host, port)),
            #[cfg(test)]
            Engine::Mock => Messenger::Mock(MockAdapter::default()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_only_zenoh_engine_allowed() {
        // Test that zenoh engine is accepted
        let result =
            Engine::from_str_with_config("zenoh", Some("localhost".to_string()), Some(7447));
        assert!(result.is_ok());
        assert_eq!(
            result.unwrap(),
            Engine::Zenoh {
                host: "localhost".to_string(),
                port: 7447
            }
        );

        // Test that mock engine is allowed in test mode
        let result = Engine::from_str_with_config("mock", None, None);
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), Engine::Mock);

        // Test that any other engine is rejected
        let result =
            Engine::from_str_with_config("rabbitmq", Some("localhost".to_string()), Some(5672));
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), Error::UnsupportedEngine));
    }

    #[test]
    fn test_zenoh_requires_config() {
        // Test that zenoh requires host and port
        let result = Engine::from_str_with_config("zenoh", None, Some(8080));
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), Error::MissingEngineConfig));

        let result = Engine::from_str_with_config("zenoh", Some("localhost".to_string()), None);
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), Error::MissingEngineConfig));

        let result = Engine::from_str_with_config("zenoh", None, None);
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), Error::MissingEngineConfig));
    }
}
