// Mock backend is always available as a fallback when zenoh is not enabled

mod adapters;
mod error;
mod types;
mod wire;
#[cfg(feature = "zenoh")]
mod zenohd;

pub use error::Error as PeppyMessagingInterfaceError;
pub use types::{
    Message, Messenger, MessengerAdapter, MessengerBackend, MessengerPublisher, Payload,
    PayloadSlices, PublisherQoS, SubscriberQoS, Subscription, TopicMessage,
};
pub use wire::{
    ActionWireReceiver, ActionWireSender, InterfaceIdentifier, NodeIdentifier, SenderTarget,
    SenderTargetError, ServiceKind, ServiceWireReceiver, ServiceWireSender, TopicWireReceiver,
    TopicWireSender,
};

pub use adapters::mock::{MockAdapter, MockInstance};

#[cfg(feature = "zenoh")]
pub use adapters::zenoh::{ZenohAdapter, ZenohdInstance};
#[cfg(feature = "zenoh")]
pub use zenohd::ZenohNetProtocol;
