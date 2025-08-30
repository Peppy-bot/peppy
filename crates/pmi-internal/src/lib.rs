// Mock backend is always available as a fallback when zenoh is not enabled

mod adapters;

mod error;
mod types;
#[cfg(feature = "zenoh")]
mod zenohd;

// Exports for users of the lib
pub use error::Error as PeppyMessagingInterfaceError;
pub use types::{
    Message, MessagingEngineContext, Messenger, MessengerBackend, Subscription, ThroughputMode,
};
