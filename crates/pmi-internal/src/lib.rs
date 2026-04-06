// Mock backend is always available as a fallback when zenoh is not enabled

mod adapters;
mod error;
mod types;
#[cfg(feature = "zenoh")]
mod zenohd;

pub use error::Error as PeppyMessagingInterfaceError;
pub use types::{
    Message, Messenger, MessengerAdapter, MessengerBackend, PublisherQoS, SubscriberQoS,
    Subscription, TopicMessage,
};

pub use adapters::mock::{MockAdapter, MockInstance};

#[cfg(feature = "zenoh")]
pub use adapters::zenoh::{ZenohAdapter, ZenohdInstance};
#[cfg(feature = "zenoh")]
pub use zenohd::ZenohNetProtocol;
