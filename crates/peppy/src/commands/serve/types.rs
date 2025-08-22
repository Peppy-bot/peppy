use std::fmt;
use std::str::FromStr;

use super::messaging::adapters::{mock::MockAdapter, zenoh::ZenohAdapter};
use super::messaging::{Message, MessengerBackend, Subscription};
use crate::{Error, Result};
use async_trait::async_trait;

pub struct MessagingConfiguration {
    pub engine: Engine,
    pub host: String,
    pub port: u16,
}

impl MessagingConfiguration {
    pub fn new(host: &str, port: u16) -> Self {
        Self {
            engine: Engine::Zenoh, // Default engine
            host: host.to_string(),
            port,
        }
    }

    pub fn with_engine(mut self, engine: Engine) -> Self {
        self.engine = engine;
        self
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Engine {
    Zenoh,
    Mock,
}

impl fmt::Display for Engine {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Engine::Zenoh => write!(f, "zenoh"),
            Engine::Mock => write!(f, "mock"),
        }
    }
}

impl FromStr for Engine {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self> {
        match s {
            "zenoh" => Ok(Engine::Zenoh),
            "mock" => Ok(Engine::Mock),
            _ => Err(Error::UnsupportedEngine),
        }
    }
}

pub enum Messenger {
    Zenoh(ZenohAdapter),
    Mock(MockAdapter),
}

macro_rules! delegate_to_variant {
    ($self:expr, $method:ident $(, $arg:expr)*) => {
        match $self {
            Messenger::Zenoh(adapter) => adapter.$method($($arg),*).await,
            Messenger::Mock(adapter) => adapter.$method($($arg),*).await,
        }
    };
}

#[async_trait]
impl MessengerBackend for Messenger {
    async fn start_router(&mut self) -> Result<()> {
        delegate_to_variant!(self, start_router)
    }

    async fn connect(&mut self) -> Result<()> {
        delegate_to_variant!(self, connect)
    }

    async fn publish(&self, message: Message) -> Result<()> {
        delegate_to_variant!(self, publish, message)
    }

    async fn subscribe(&self, topic: &str) -> Result<Subscription> {
        delegate_to_variant!(self, subscribe, topic)
    }

    async fn shutdown(&mut self) -> Result<()> {
        delegate_to_variant!(self, shutdown)
    }
}

impl Messenger {
    pub fn from_config(configuration: MessagingConfiguration) -> Self {
        match configuration.engine {
            Engine::Zenoh => {
                Messenger::Zenoh(ZenohAdapter::new(configuration.host, configuration.port))
            }
            Engine::Mock => Messenger::Mock(MockAdapter::default()),
        }
    }
}
