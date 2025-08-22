pub mod adapters;

use super::types::{Engine, MessagingConfiguration, Messenger};
use crate::Result;
use async_trait::async_trait;

#[async_trait]
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

pub struct MessagingFactory {}

impl MessagingFactory {
    pub fn build_messenger(configuration: MessagingConfiguration) -> Messenger {
        use adapters::{mock::MockAdapter, zenoh::ZenohAdapter};

        match configuration.engine {
            Engine::Zenoh => {
                Messenger::Zenoh(ZenohAdapter::new(configuration.host, configuration.port))
            }
            Engine::Mock => Messenger::Mock(MockAdapter::default()),
        }
    }
}
