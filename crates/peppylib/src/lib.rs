mod error;
mod node;

pub mod checker;
pub mod encoding;
pub mod messaging;
pub mod runtime;

pub use error::{Error as PeppyError, Result as PeppyResult};
pub use messaging::{ActionMessenger, MessengerHandle, ServiceMessenger, TopicMessenger};
pub use node::{setup_node, setup_node_from_config};
#[cfg(all(feature = "zenoh", any(test, feature = "test-support")))]
pub use pmi::start_zenohd_process;

// Reexport useful modules for the user of the lib
pub mod config {
    pub use config::consts::{NODE_CONFIG_FILE, NODE_CONFIG_FINGERPRINT_FILE};
    pub use config::node::*;
}
