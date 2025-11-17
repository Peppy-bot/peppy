use std::sync::Arc;

use crate::Error;
use pmi::Messenger;
use tokio::sync::Mutex;
use tracing::info;

pub struct CommandsListener {
    messenger: Arc<Mutex<Messenger>>,
}

impl CommandsListener {
    pub fn new(messenger: Arc<Mutex<Messenger>>) -> Self {
        Self { messenger }
    }
}

impl super::ServeAsyncCommand for CommandsListener {
    fn run(self: Box<Self>) -> super::ServeAsyncHandle {
        let messenger = self.messenger;

        let future = Box::pin(async move {
            info!("Starting commands listener...");

            // Keep the messenger alive for future command handling logic.
            let _messenger = messenger;

            tokio::signal::ctrl_c().await.map_err(|e| {
                Error::ExecutionFailed(format!("Failed to listen for shutdown signal: {}", e))
            })?;

            info!("Shutting down commands listener...");
            Ok(())
        });

        super::ServeAsyncHandle::new(future, None)
    }
}
