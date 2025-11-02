mod error;
mod node;

pub mod checker;
pub mod encoding;
pub mod messaging;

pub use error::{Error as PeppyError, Result as PeppyResult};
pub use messaging::{ActionMessenger, ServiceMessenger, TopicMessenger};
pub use node::{setup_node, setup_node_from_config};

// Reexport useful modules for the user of the lib
pub mod config {
    pub use config::node::*;
}
