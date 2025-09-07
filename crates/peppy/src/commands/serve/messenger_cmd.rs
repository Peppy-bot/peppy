use super::ServeAsyncCommand;
use crate::{Error, Result};
use pmi::{Messenger, MessengerBackend};
use tokio::task::JoinHandle;
use tracing::info;

impl ServeAsyncCommand for Messenger {
    fn execute_async(&self) -> Result<JoinHandle<Result<()>>> {
        let context = self.context.clone();

        let handle = tokio::spawn(async move {
            let mut messenger = Messenger::new(context).map_err(Error::PeppyMessagingInterface)?;

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
        });

        Ok(handle)
    }
}
