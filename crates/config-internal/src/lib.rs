mod config;
mod error;
mod watch;

pub mod consts;

// To create a new config
pub use config::{
    ConfigTemplateType, ExposedAction, ExposedService, ExposedTopic, Interfaces, Language,
    NodeConfig, NodeConfigCreator, SubscribedAction, SubscribedService, SubscribedTopic,
};

// To parse existing configs
pub use config::NodeConfigParser;

// To watch projects
pub use watch::{NodeConfigWatcher, NodeIndexState};

pub use error::{Error as ConfigError, Result as ConfigResult};
