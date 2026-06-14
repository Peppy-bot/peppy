// The MockAdapter in-process backend is compiled into every build alongside the
// zenoh transport, but it is not a standalone zenoh-less build target: the mock
// uses tokio's `time` and multi-thread runtime, which this crate only obtains
// transitively through the `zenoh` dependency. A `--no-default-features` build
// (no zenoh) therefore does not compile. Every supported configuration enables
// `zenoh` (the default). See the `tokio` note in Cargo.toml.

#![forbid(unsafe_code)]

mod adapters;
mod error;
mod probe;
mod types;
mod wire;
#[cfg(feature = "zenoh")]
mod zenoh_config;
#[cfg(feature = "zenoh")]
mod zenohd;

/// The full `(core_node, instance_id)` producer wire address taken by the
/// sender constructors. Defined in `config` (the serialized layer); re-exported
/// here so pmi's public API is nameable through pmi alone.
pub use config::runtime::ProducerRef;
pub use error::Error as PeppyMessagingInterfaceError;
pub use probe::{MAX_PROBE_REPLY_SIZE, build_sized_probe_request};
// `ZenohResponseToken` / `MockResponseToken` are intentionally NOT re-exported:
// they are opaque, non-constructible payloads of the public `ResponseToken` enum
// (reached only through `ResponseToken`'s methods), so naming them directly is
// not part of the crate's public surface.
pub use types::{
    ActionLivelinessEvent, ActionLivelinessProbe, ActionLivelinessToken, ActionLivelinessWatch,
    IncomingRequest, Messenger, MessengerAdapter, MessengerBackend, MessengerPublisher, Payload,
    PublisherQoS, ReplyStream, ResponseToken, ServiceQueryable, ServiceReply,
    SubscriberBufferSizes, SubscriberQoS, Subscription, TopicMessage,
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
