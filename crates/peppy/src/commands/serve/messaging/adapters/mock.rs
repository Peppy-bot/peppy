use crate::commands::serve::messaging::error::MessengerError;
use crate::commands::serve::messaging::{Message, MessengerBackend, Subscription};
use async_trait::async_trait;

pub struct MockAdapter {
    // hold mock session, pubs, subs, etc.
}

impl Default for MockAdapter {
    fn default() -> Self {
        Self {}
    }
}

#[async_trait]
impl MessengerBackend for MockAdapter {
    async fn start_router(&mut self) -> Result<(), MessengerError> {
        Ok(())
    }
    async fn connect(&mut self) -> Result<(), MessengerError> {
        Ok(())
    }
    async fn publish(&self, message: Message) -> Result<(), MessengerError> {
        Ok(())
    }
    async fn subscribe(&self, topic: &str) -> Result<Subscription, MessengerError> {
        let (_, rx) = tokio::sync::mpsc::channel(128);
        // spawn task to pump mock samples into tx
        Ok(Subscription { rx })
    }
    async fn shutdown(&mut self) -> Result<(), MessengerError> {
        Ok(())
    }
}
