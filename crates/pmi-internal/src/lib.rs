// Mock backend is always available as a fallback when zenoh is not enabled

mod adapters;

mod encoding;
mod error;
mod messaging_types;
#[cfg(feature = "zenoh")]
mod zenohd;

// Exports for users of the lib
pub use encoding::{Encoder, EncodingBackend, EncodingFormat};
pub use error::Error as PeppyMessagingInterfaceError;
pub use messaging_types::{
    Message, Messenger, MessengerAdapter, MessengerBackend, PublisherQoS, SubscriberQoS,
    Subscription,
};

pub use adapters::mock::MockAdapter;

// Zenoh specific exports (only when feature is enabled)
#[cfg(feature = "zenoh")]
pub use adapters::zenoh::{ZenohAdapter, ZenohClientConfigTemplate};
#[cfg(feature = "zenoh")]
pub use zenohd::facade::{ZenohNetProtocol, ZenohRouterConfigTemplate};
