use config::ConfigError;
use thiserror::Error;

pub type Result<T> = core::result::Result<T, Error>;

#[derive(Debug, Error)]
pub enum Error {
    // -- config-internal
    #[error(transparent)]
    Config(#[from] ConfigError),

    // -- nodes errors
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

    // -- lifecycle errors
    #[error("Invalid stage transition for `{node_name}`:{node_tag}: cannot go from {from} to {to}")]
    InvalidStageTransition {
        node_name: String,
        node_tag: String,
        from: &'static str,
        to: &'static str,
    },
    #[error("Failed to build node `{node_name}`:{node_tag}: {reason}")]
    BuildFailed {
        node_name: String,
        node_tag: String,
        reason: String,
    },
    #[error("Failed to start node `{node_name}`:{node_tag}: {reason}")]
    StartFailed {
        node_name: String,
        node_tag: String,
        reason: String,
    },
}
