use crate::commands::serve::pubsub::error::MessengerError;
use crate::commands::serve::pubsub::messenger::{MessengerBackend, Subscription};

pub struct ZenohBackend {
    // hold zenoh session, pubs, subs, etc.
}

impl Default for ZenohBackend {
    fn default() -> Self {
        Self {}
    }
}

// TODO: async_trait might not be needed anymore
#[async_trait::async_trait]
impl MessengerBackend for ZenohBackend {
    async fn connect(&mut self) -> Result<(), MessengerError> {
        // open zenoh session
        Ok(())
    }

    async fn publish(&self, topic: &str, payload: &[u8]) -> Result<(), MessengerError> {
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
