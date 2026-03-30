use std::path::PathBuf;

use config::ConfigError;
use thiserror::Error;

pub type Result<T> = core::result::Result<T, Error>;

#[derive(Debug, Error)]
pub enum Error {
    // -- general
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("{0} not implemented yet")]
    NotImplemented(&'static str),
    #[error("{0} could not be found")]
    FileNotFound(PathBuf),

    // -- config-internal
    #[error(transparent)]
    Config(#[from] ConfigError),

    // -- nodes errors
    #[error("Cannot find the node in `{0}`")]
    NodeNotFound(String),
    #[error("The node name `{0}` or tag `{1}` could not be found")]
    NoMatchingNode(String, String),
    #[error(
        "`{dependant}`:{dependant_tag} expects {interface_kind} `{interface_name}` from `{dependency}`:{dependency_tag}, but it is not exposed"
    )]
    MissingInterface {
        dependant: String,
        dependant_tag: String,
        dependency: String,
        dependency_tag: String,
        interface_kind: String,
        interface_name: String,
    },
    #[error(
        "`{dependant}:{dependant_tag}` depends on `{dependency}:{dependency_tag}`, but it does not exist in the stack"
    )]
    MissingDependency {
        dependant: String,
        dependant_tag: String,
        dependency: String,
        dependency_tag: String,
    },
    #[error(
        "`{dependant}:{dependant_tag}` references undeclared local_node_id `{local_node_id}` in consumed interfaces"
    )]
    UndeclaredLocalNodeId {
        dependant: String,
        dependant_tag: String,
        local_node_id: String,
    },

    // -- node stack errors
    #[error("Cannot modify the root node (it always has exactly one instance)")]
    CannotModifyRootNode,
    #[error("NodeStack requires at least one node (the root)")]
    EmptyNodeStack,
    #[error("Config mismatch for `{name}`:{tag}: existing entity has different interfaces")]
    ConfigMismatch { name: String, tag: String },
    #[error("Cannot overwrite node `{node_name}`:{node_tag} because other nodes depend on it")]
    CannotOverwriteNodeWithDependents { node_name: String, node_tag: String },
    #[error("Instance ID `{instance_id}` already exists for node `{node_name}`:{node_tag}")]
    DuplicateInstanceId {
        instance_id: String,
        node_name: String,
        node_tag: String,
    },
    #[error("Cannot remove node `{node_name}`:{node_tag} because it still has instances")]
    CannotRemoveNodeWithInstances { node_name: String, node_tag: String },
}
