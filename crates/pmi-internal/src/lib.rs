// Mock backend is always available as a fallback when zenoh is not enabled

mod adapters;
mod error;
mod types;
mod wire;
#[cfg(feature = "zenoh")]
mod zenoh_config;
#[cfg(feature = "zenoh")]
mod zenohd;

pub use error::Error as PeppyMessagingInterfaceError;
#[cfg(feature = "zenoh")]
pub use types::ZenohResponseToken;
pub use types::{
    IncomingRequest, Message, Messenger, MessengerAdapter, MessengerBackend, MessengerPublisher,
    MockResponseToken, Payload, PayloadSlices, PublisherQoS, ReplyStream, ResponseToken,
    ServiceQueryable, ServiceReply, SubscriberBufferSizes, SubscriberQoS, Subscription,
    TopicMessage,
};
pub use wire::{
    ActionWireReceiver, ActionWireSender, DEFAULT_LINK_ID, InterfaceIdentifier, NodeIdentifier,
    Segment, SegmentError, SenderTarget, SenderTargetError, ServiceKind, ServiceQueryKind,
    ServiceReplyKind, ServiceWireReceiver, ServiceWireSender, TopicWireReceiver, TopicWireSender,
};

pub use adapters::mock::{MockAdapter, MockInstance};

#[cfg(feature = "zenoh")]
pub use adapters::zenoh::ZenohAdapter;
#[cfg(feature = "router")]
pub use adapters::zenoh::ZenohdInstance;
#[cfg(feature = "router")]
pub use zenohd::RouterHealthChecker;
#[cfg(feature = "zenoh")]
pub use zenohd::ZenohNetProtocol;
