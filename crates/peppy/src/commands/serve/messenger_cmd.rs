use super::{ServeAsyncCommand, ServeFuture};
use crate::Error;
use pmi::{Messenger, MessengerAdapter, MessengerBackend, MockAdapter, ZenohAdapter};
use tracing::info;

impl ServeAsyncCommand for Messenger {
    fn run(&self) -> ServeFuture {
        // Create a new adapter instance to move into the async block
        // Since we only have &self, we need to recreate the adapter
        let adapter = match &self.adapter {
            MessengerAdapter::Mock(_) => {
                // For Mock, create a new default instance since it uses Arc for shared state
                MessengerAdapter::Mock(MockAdapter::default())
            }
            MessengerAdapter::Zenoh(_) => {
                // For Zenoh, create a new adapter with default (None) config
                // This will use the default zenohd configuration
                MessengerAdapter::Zenoh(
                    ZenohAdapter::from_zenohd_config(None::<&std::path::Path>)
                        .expect("Failed to create Zenoh adapter"),
                )
            }
        };

        Box::pin(async move {
            let mut messenger = Messenger::new(adapter);

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
