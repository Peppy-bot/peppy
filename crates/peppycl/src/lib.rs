mod error;
pub mod node;

pub use error::{Error as ControlError, Result as ControlResult};
pub use node::{spin_from_config_content, spin_from_config_content_async};
