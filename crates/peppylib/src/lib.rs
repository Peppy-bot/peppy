mod error;
mod node;

pub mod checker;
pub mod messaging;

pub use error::{Error as ControlError, Result as ControlResult};
pub use messaging::PeppyMessenger;
pub use node::{setup_node, setup_node_from_config};
