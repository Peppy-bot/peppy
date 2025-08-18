use super::error::MessengerError;
use async_trait::async_trait;

// TODO: async_trait might not be needed anymore
#[async_trait]
pub trait MessengerBackend: Send + Sync {
    async fn connect(&mut self) -> Result<(), MessengerError>;
    async fn publish(&self, topic: &str, payload: &[u8]) -> Result<(), MessengerError>;
    async fn subscribe(&self, topic: &str) -> Result<Subscription, MessengerError>;
    async fn shutdown(&mut self) -> Result<(), MessengerError>;
}

pub struct Subscription {
    // stream of messages; could be tokio::mpsc::Receiver or a Stream
    pub rx: tokio::sync::mpsc::Receiver<Message>,
}

pub struct Message {
    pub topic: String,
    pub payload: bytes::Bytes,
}

// Static dispatch
pub struct Messenger<B: MessengerBackend> {
    backend: B,
}

impl<B: MessengerBackend> Messenger<B> {
    pub async fn new(mut backend: B) -> Result<Self, MessengerError> {
        backend.connect().await?;
        Ok(Self { backend })
    }
    pub async fn publish(&self, topic: &str, payload: &[u8]) -> Result<(), MessengerError> {
        self.backend.publish(topic, payload).await
    }
    pub async fn subscribe(&self, topic: &str) -> Result<Subscription, MessengerError> {
        self.backend.subscribe(topic).await
    }
    pub async fn shutdown(mut self) -> Result<(), MessengerError> {
        self.backend.shutdown().await
    }
}

// Dynamic dispatch (for runtime)
pub struct DynMessenger {
    backend: Box<dyn MessengerBackend>,
}

impl DynMessenger {
    pub async fn new(mut backend: Box<dyn MessengerBackend>) -> Result<Self, MessengerError> {
        backend.connect().await?;
        Ok(Self { backend })
    }
    pub async fn publish(&self, topic: &str, payload: &[u8]) -> Result<(), MessengerError> {
        self.backend.publish(topic, payload).await
    }
    pub async fn subscribe(&self, topic: &str) -> Result<Subscription, MessengerError> {
        self.backend.subscribe(topic).await
    }
    pub async fn shutdown(mut self) -> Result<(), MessengerError> {
        self.backend.shutdown().await
    }
}
