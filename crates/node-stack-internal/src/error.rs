use std::path::PathBuf;

use config::{ConfigError, ParsingError};
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

    // -- node stack errors
    #[error("Cannot modify the root node (it always has exactly one instance)")]
    CannotModifyRootNode,
    #[error("Cannot overwrite node `{node_name}`:{node_tag} because other nodes depend on it")]
    CannotOverwriteNodeWithDependents { node_name: String, node_tag: String },
    #[error("Cannot overwrite node `{node_name}`:{node_tag} because it still has live instances")]
    CannotOverwriteNodeWithLiveInstances { node_name: String, node_tag: String },
    #[error("Instance ID `{instance_id}` already exists for node `{node_name}`:{node_tag}")]
    DuplicateInstanceId {
        instance_id: String,
        node_name: String,
        node_tag: String,
    },
    #[error(
        "link_id `{link_id}` is already claimed by instance `{held_by}` of node `{node_name}`:{node_tag}"
    )]
    DuplicateLinkId {
        link_id: String,
        held_by: String,
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
    #[error("Failed to build node `{node_name}:{node_tag}`: {reason}")]
    BuildFailed {
        node_name: String,
        node_tag: String,
        reason: String,
    },
    #[error("Failed to start node `{node_name}:{node_tag}`: {reason}")]
    StartFailed {
        node_name: String,
        node_tag: String,
        reason: String,
    },

    // -- virtual deptree errors
    #[error("Virtual dependency tree contains a cycle involving: {nodes:?}")]
    VirtualDeptreeCycle { nodes: Vec<String> },
    #[error("Duplicate local node `{name}:{tag}` discovered at `{first}` and `{second}`")]
    DuplicateLocalNode {
        name: String,
        tag: String,
        first: PathBuf,
        second: PathBuf,
    },
}

impl From<ParsingError> for Error {
    fn from(err: ParsingError) -> Self {
        Self::Config(ConfigError::Parsing(err))
    }
}
