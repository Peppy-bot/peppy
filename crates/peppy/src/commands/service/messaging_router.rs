use super::serve::{ServeAsyncCommand, ServeAsyncHandle};
use crate::error::Error;
use pmi::{Messenger, MessengerBackend};
use std::sync::Arc;
use tokio::sync::{Mutex, oneshot, watch};
use tracing::info;

pub struct MessagingRouter {
    messenger: Arc<Mutex<Messenger>>,
    messaging_ready: watch::Sender<bool>,
}

impl MessagingRouter {
    pub fn new(messenger: Arc<Mutex<Messenger>>, messaging_ready: watch::Sender<bool>) -> Self {
        Self {
            messenger,
            messaging_ready,
        }
    }
}

impl ServeAsyncCommand for MessagingRouter {
    fn run(self: Box<Self>) -> ServeAsyncHandle {
        let (ready_tx, ready_rx) = oneshot::channel();
        let messenger = self.messenger;
        let messaging_ready = self.messaging_ready;

        let future = Box::pin(async move {
            {
                let mut messenger = messenger.lock().await;
                info!("Starting the messaging router...");
                messenger
                    .start_router()
                    .await
                    .map_err(Error::PeppyMessagingInterface)?;
                messenger
                    .start_session()
                    .await
                    .map_err(Error::PeppyMessagingInterface)?;
                info!("Messaging session initialized");
            }

            messaging_ready.send(true).ok();
            ready_tx.send(()).ok();

            tokio::signal::ctrl_c().await.map_err(|e| {
                Error::ExecutionFailed(format!("Failed to listen for ctrl-c: {}", e))
            })?;

            {
                let mut messenger = messenger.lock().await;
                info!("Shutting down the messaging router...");
                messenger
                    .stop_router()
                    .await
                    .map_err(Error::PeppyMessagingInterface)?;
            }

            Ok(())
        });

        ServeAsyncHandle::new(future, Some(ready_rx))
    }
}
