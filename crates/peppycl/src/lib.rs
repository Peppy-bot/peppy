mod error;
mod generator;
pub mod node;

pub use error::{Error as ControlError, Result as ControlResult};
pub use node::{setup_node, setup_node_from_config};
