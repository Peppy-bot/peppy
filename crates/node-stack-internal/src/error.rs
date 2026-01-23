use std::path::PathBuf;

use config::ConfigError;
use git2::Error as GitError;
use thiserror::Error;

pub type Result<T> = core::result::Result<T, Error>;

#[derive(Debug, Error)]
pub enum Error {
    // -- general
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Git(#[from] GitError),
    #[error("{0} not implemented yet")]
    NotImplemented(&'static str),
    #[error("{0} could not be found")]
    FileNotFound(PathBuf),
    #[error("Failed to download bundle `{url}`: {reason}")]
    HttpDownload { url: String, reason: String },
    #[error("Failed to extract bundle `{url}`: {reason}")]
    BundleExtraction { url: String, reason: String },
    #[error("Checksum mismatch for bundle `{0}`")]
    ChecksumMismatch(String),
    #[error("Unsupported checksum algorithm `{0}`")]
    UnsupportedChecksum(String),
    #[error("Invalid checksum `{0}`: {1}")]
    InvalidChecksum(String, String),

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

    // -- deployment errors
    // {0}: node_name + tag, {1}: Reason
    #[error("Failed to resolve deployment {0}: {1}")]
    DeploymentNotResolvable(String, String),
    #[error(
        "The deployment `{deployment}` contains wrong input parameters. Expected parameters: {expected:?}. Unexpected parameters: {unexpected:?}"
    )]
    WrongInputParameters {
        deployment: String,
        expected: Vec<String>,
        unexpected: Vec<String>,
    },
    #[error(
        "The deployment `{deployment}` has a parameter type mismatch at `{path}`: expected `{expected}`, got `{actual}`"
    )]
    WrongParameterType {
        deployment: String,
        path: String,
        expected: String,
        actual: String,
    },
}
