use super::{ServeAsyncCommand, ServeFuture};
use crate::Error;
use pmi::{Messenger, MessengerBackend};
use tracing::info;

impl ServeAsyncCommand for Messenger {
    fn run(self: Box<Self>) -> ServeFuture {
        Box::pin(async move {
            let mut messenger = *self;

            // Starts the zenoh router
            info!("Starting the messaging router...");
            messenger
                .start_router()
                .await
                .map_err(Error::PeppyMessagingInterface)?;

            // Keep the messenger alive until shutdown signal (Ctrl+C)
            tokio::signal::ctrl_c().await.map_err(|e| {
                Error::ExecutionFailed(format!("Failed to listen for ctrl-c: {}", e))
            })?;

            info!("Shutting down the messaging router...");
            messenger
                .stop_router()
                .await
                .map_err(Error::PeppyMessagingInterface)?;

            Ok(())
        })
    }
}
