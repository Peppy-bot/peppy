use super::super::{Message, MessengerBackend, Subscription};
use crate::Result;
use crate::zenohd;
use std::path::PathBuf;

pub struct ZenohAdapter {
    zenohd: zenohd::ZenohdFacade,
}

impl ZenohAdapter {
    pub fn new(config: Option<PathBuf>) -> Result<Self> {
        Ok(Self {
            zenohd: zenohd::ZenohdFacade::new(config)?,
        })
    }
}

impl MessengerBackend for ZenohAdapter {
    /// Starts a zenohd process, using std::process::Command is the recommended way as using the
    /// rust crate directly prevents the user from using plugins/adminspace
    fn init(&mut self) -> Result<()> {
        self.zenohd.start_router()?;
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

    fn shutdown(&mut self) -> Result<()> {
        self.zenohd.stop_router()?;
        Ok(())
    }
}
