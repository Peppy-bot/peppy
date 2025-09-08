mod config;
mod error;
mod watch;

pub mod consts;

// To create a new config
pub use config::{ConfigTemplateType, NodeConfig, NodeConfigCreator};

// To parse existing configs
pub use watch::{NodeConfigWatcher, NodeIndexState};

pub use error::{Error as ConfigError, Result as ConfigResult};
