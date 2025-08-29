// Mock backend is always available as a fallback when zenoh is not enabled

mod messaging;

mod error;
mod types;
#[cfg(feature = "zenoh")]
mod zenohd;

// Exports for users of the lib
pub use error::Error as PeppyMessagingInterfaceError;
pub use messaging::{Message, Messenger, MessengerBackend, ThroughputMode};
pub use types::MessagingEngineContext;
