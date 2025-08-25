use super::super::{Message, MessengerBackend, Subscription};
use crate::Result;

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

impl MessengerBackend for ZenohAdapter {
    async fn start_router(&mut self) -> Result<()> {
        // start a Zenoh router session
        Ok(())
    }

    async fn connect(&mut self) -> Result<()> {
        // open zenoh session
        Ok(())
    }

    async fn publish(&self, message: Message) -> Result<()> {
        // zenoh publish
        Ok(())
    }

    async fn subscribe(&self, topic: &str) -> Result<Subscription> {
        // create zenoh subscriber, forward events into rx
        let (tx, rx) = tokio::sync::mpsc::channel(128);
        // spawn task to pump zenoh samples into tx
        Ok(Subscription { rx })
    }

    async fn shutdown(&mut self) -> Result<()> {
        // close session
        Ok(())
    }
}
