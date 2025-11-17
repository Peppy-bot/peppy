// Mock backend is always available as a fallback when zenoh is not enabled

mod adapters;

mod encoding;
mod error;
mod types;
#[cfg(feature = "zenoh")]
mod zenohd;
#[cfg(feature = "zenoh")]
pub mod zenohd_support;

// Exports for users of the lib
pub use encoding::{Encoder, EncodingBackend, EncodingFormat};
pub use error::Error as PeppyMessagingInterfaceError;
pub use types::{
    Message, Messenger, MessengerAdapter, MessengerBackend, PublisherQoS, SubscriberQoS,
    Subscription,
};

pub use adapters::mock::MockAdapter;

// Zenoh specific exports (only when feature is enabled)
#[cfg(feature = "zenoh")]
pub use adapters::zenoh::{ZenohAdapter, ZenohClientConfigTemplate};
#[cfg(feature = "zenoh")]
pub use zenohd::facade::{ZenohNetProtocol, ZenohRouterConfigTemplate};
#[cfg(feature = "zenoh")]
pub use zenohd_support::start_zenohd_process;
