mod adapters;

use super::types::{Engine, MessagingConfiguration};
use crate::Result;
use adapters::mock::MockAdapter;
use adapters::zenoh::ZenohAdapter;
use async_trait::async_trait;

#[async_trait]
pub(in crate::commands::serve) trait MessengerBackend:
    Send + Sync
{
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

pub struct MessagingFactory {}

impl MessagingFactory {
    pub fn build_messenger(configuration: MessagingConfiguration) -> Box<dyn MessengerBackend> {
        match configuration.engine {
            Engine::Zenoh => Box::new(ZenohAdapter::new(configuration.host, configuration.port)),
            Engine::Mock => Box::new(MockAdapter::default()),
        }
    }
}
