mod config;
mod error;
mod watch;

pub mod consts;

// To create a new config
pub use config::{
    ConfigTemplateType, Deployment, DeploymentInstance, DeploymentSource, ExposedAction,
    ExposedService, ExposedTopic, Interfaces, Language, MessageFormat, NodeConfig,
    NodeConfigCreator, SubscribedAction, SubscribedService, SubscribedTopic, SubscribesTo,
};

// To parse existing configs
pub use config::NodeConfigParser;

// To watch projects
pub use watch::{FSNodeConfigWatcher, NodeIndexState};

pub use error::{Error as ConfigError, Result as ConfigResult};
