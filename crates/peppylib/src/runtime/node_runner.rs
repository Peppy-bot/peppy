use tokio_util::sync::CancellationToken;

use crate::MessengerHandle;
use crate::error::Result;

use super::processor::Processor;

/// The main runtime handle for a Peppy node.
///
/// Provides access to:
/// - Messaging system via `messenger()`
/// - Runtime configuration via `processor()`
/// - Cancellation token for graceful shutdown via `cancellation_token()`
pub struct NodeRunner {
    messenger: MessengerHandle,
    processor: Processor,
    cancellation_token: CancellationToken,
}

impl NodeRunner {
    /// Create a new NodeRunner, connecting to the messaging system.
    pub async fn new(processor: Processor) -> Result<Self> {
        Self::with_cancellation_token(processor, CancellationToken::new()).await
    }

    /// Create a new NodeRunner with a provided cancellation token.
    pub async fn with_cancellation_token(
        processor: Processor,
        cancellation_token: CancellationToken,
    ) -> Result<Self> {
        let messenger =
            MessengerHandle::from_host_port(processor.messaging_host(), processor.messaging_port())
                .await?;

        Ok(Self {
            messenger,
            processor,
            cancellation_token,
        })
    }

    /// Get reference to the messenger handle
    pub fn messenger(&self) -> &MessengerHandle {
        &self.messenger
    }

    /// Get reference to the runtime processor
    pub fn processor(&self) -> &Processor {
        &self.processor
    }

    /// Get the cancellation token for coordinating graceful shutdown.
    ///
    /// Use this to observe shutdown signals in spawned tasks:
    /// ```ignore
    /// let token = node_runner.cancellation_token().clone();
    /// tokio::spawn(async move {
    ///     loop {
    ///         tokio::select! {
    ///             _ = token.cancelled() => break,
    ///             _ = do_work() => {}
    ///         }
    ///     }
    /// });
    /// ```
    pub fn cancellation_token(&self) -> &CancellationToken {
        &self.cancellation_token
    }

    /// Check if shutdown has been requested.
    ///
    /// Returns `true` if the cancellation token has been cancelled.
    pub fn is_shutting_down(&self) -> bool {
        self.cancellation_token.is_cancelled()
    }
}
