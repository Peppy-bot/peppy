use crate::MessengerHandle;
use crate::error::Result;

use super::processor::Processor;

/// The main runtime handle for a Peppy node.
///
/// Provides access to:
/// - Messaging system via `messenger()`
/// - Runtime configuration via `processor()`
pub struct NodeRunner {
    messenger: MessengerHandle,
    processor: Processor,
}

impl NodeRunner {
    /// Create a new NodeRunner, connecting to the messaging system.
    pub async fn new(processor: Processor) -> Result<Self> {
        let messenger =
            MessengerHandle::from_host_port(processor.messaging_host(), processor.messaging_port())
                .await?;

        Ok(Self {
            messenger,
            processor,
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
}
