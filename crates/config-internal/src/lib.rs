mod config;
mod error;
mod watch;

pub mod consts;

// To create a new config
pub use config::{
    CallbackName, CallbackNameError, ConfigTemplateType, Deployment, DeploymentInstance,
    ExposedAction, ExposedService, ExposedTopic, GitRemoteSpec, Interfaces, MessageFormat,
    NodeConfig, NodeConfigCreator, NodeSource, SubscribedAction, SubscribedService,
    SubscribedTopic, SubscribesTo,
};

// To parse existing configs
pub use config::NodeConfigParser;

// To watch projects
pub use watch::{FSNodeConfigWatcher, NodeIndexState};

pub use error::{Error as ConfigError, Result as ConfigResult};
