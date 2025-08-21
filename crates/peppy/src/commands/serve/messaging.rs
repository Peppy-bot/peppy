mod adapters;
mod error;

pub use adapters::mock::MockAdapter;
pub use adapters::zenoh::ZenohAdapter;
pub use error::MessengerError;

use async_trait::async_trait;

#[async_trait]
pub trait MessengerBackend: Send + Sync {
    async fn start_router(&mut self) -> Result<(), MessengerError>;
    async fn connect(&mut self) -> Result<(), MessengerError>;
    async fn publish(&self, message: Message) -> Result<(), MessengerError>;
    async fn subscribe(&self, topic: &str) -> Result<Subscription, MessengerError>;
    async fn shutdown(&mut self) -> Result<(), MessengerError>;
}

pub struct Subscription {
    // stream of messages; could be tokio::sync::mpsc::Receiver or a Stream
    pub rx: tokio::sync::mpsc::Receiver<Message>,
}

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
