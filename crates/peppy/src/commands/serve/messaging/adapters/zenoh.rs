use super::super::{Message, MessengerBackend, Subscription};
use crate::{Error, Result, zenohd};
use std::path::PathBuf;

pub struct ZenohAdapter {
    zenohd: zenohd::ZenohdFacade,
    session: Option<zenoh::Session>,
}

impl ZenohAdapter {
    pub fn new(config: Option<PathBuf>) -> Result<Self> {
        let facade = zenohd::ZenohdFacade::new(config)?;

        Ok(Self {
            zenohd: facade,
            session: None,
        })
    }
}

impl MessengerBackend for ZenohAdapter {
    /// Starts a zenohd process, using std::process::Command is the recommended way as using the
    /// rust crate directly prevents the user from using plugins/adminspace
    async fn init(&mut self) -> Result<()> {
        self.zenohd.start_router()?;
        let session = zenoh::open(self.zenohd.config.clone())
            .await
            .map_err(|e| Error::BackendError(format!("Failed to create Zenoh session: {}", e)))?;

        self.session = Some(session);
        Ok(())
    }

    async fn publish(&self, _message: Message) -> Result<()> {
        // zenoh publish
        Ok(())
    }

    async fn subscribe(&self, _topic: &str) -> Result<Subscription> {
        // create zenoh subscriber, forward events into rx
        let (_tx, rx) = tokio::sync::mpsc::channel(128);
        // spawn task to pump zenoh samples into tx
        Ok(Subscription { rx })
    }

    async fn shutdown(&mut self) -> Result<()> {
        self.zenohd.stop_router()?;
        // Close the Zenoh session if it exists
        if let Some(session) = self.session.take() {
            drop(session);
        }
        Ok(())
    }
}
