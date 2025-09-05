mod config;
mod error;

pub use config::ConfigTemplateType;
pub use config::{NodeConfig, NodeConfigCreator, NodeConfigParser};
pub use error::{Error as ConfigError, Result as ConfigResult};
