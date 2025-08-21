use crate::commands::serve::messaging::error::MessengerError;
use crate::commands::serve::messaging::{Message, MessengerBackend, Subscription};
use async_trait::async_trait;

pub struct ZenohAdapter {
    // hold zenoh session, pubs, subs, etc.
    host: String,
    port: u16,
}

impl ZenohAdapter {
    pub fn new(host: String, port: u16) -> Self {
        Self { host, port }
    }
}

impl Default for ZenohAdapter {
    fn default() -> Self {
        Self {
            host: "localhost".into(),
            port: 7447,
        }
    }
}

#[async_trait]
impl MessengerBackend for ZenohAdapter {
    async fn start_router(&mut self) -> Result<(), MessengerError> {
        // start a Zenoh router session
        Ok(())
    }

    async fn connect(&mut self) -> Result<(), MessengerError> {
        // open zenoh session
        Ok(())
    }

    async fn publish(&self, message: Message) -> Result<(), MessengerError> {
        // zenoh publish
        Ok(())
    }

    async fn subscribe(&self, topic: &str) -> Result<Subscription, MessengerError> {
        // create zenoh subscriber, forward events into rx
        let (tx, rx) = tokio::sync::mpsc::channel(128);
        // spawn task to pump zenoh samples into tx
        Ok(Subscription { rx })
    }

    async fn shutdown(&mut self) -> Result<(), MessengerError> {
        // close session
        Ok(())
    }
}
