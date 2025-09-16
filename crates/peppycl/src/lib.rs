mod error;
mod generator;
mod node;

pub mod interfaces;

pub use error::{Error as ControlError, Result as ControlResult};
pub use generator::generate_interfaces_code;
pub use node::{setup_node, setup_node_from_config};
