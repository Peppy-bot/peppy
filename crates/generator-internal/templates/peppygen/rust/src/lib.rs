pub mod clock;
pub mod parameters;

pub mod capnp;
pub mod emitted_topics;
pub mod consumed_topics;
pub mod exposed_actions;
pub mod exposed_services;
pub mod consumed_actions;
pub mod consumed_services;
pub mod paired_topics;

// Test-only surfaces, compiled only under the `testing` feature (enabled
// from the node's dev-dependencies; never in production builds).
#[cfg(feature = "testing")]
pub mod fixtures;
#[cfg(feature = "testing")]
pub mod mock;

pub use parameters::Parameters;
pub use peppylib::config::QoSProfile;
pub use peppylib::messaging::{ObservedSource, PeerInfo, ProducerRef};
pub use peppylib::runtime::{NodeBuilder, NodeRunner, StandaloneConfig};
pub use peppylib::{
    MessengerHandle, PeppyError as Error, PeppyResult as Result, ServiceMessenger,
};
