use std::path::PathBuf;

use config::{ConfigError, ParsingError};
use thiserror::Error;

pub type Result<T> = core::result::Result<T, Error>;

#[derive(Debug, Error)]
pub enum Error {
    // -- config
    #[error(transparent)]
    Config(#[from] ConfigError),

    // -- nodes errors
    #[error("The node name `{0}` or tag `{1}` could not be found")]
    NoMatchingNode(String, String),

    // -- pairing registry errors
    #[error("instance `{instance_id}` declares no pairing slot `{link_id}` in depends_on.pairings")]
    PairingSlotNotFound {
        instance_id: String,
        link_id: String,
    },
    #[error("pairing endpoint instance `{instance_id}` is not running in this stack")]
    PairingInstanceNotRunning { instance_id: String },
    #[error(
        "pairing slot `{slot}` is already paired with `{peer}`; a pairing slot is exclusive until cleared"
    )]
    PairingSlotAlreadyPaired { slot: String, peer: String },
    #[error(
        "cannot pair `{a}` with `{b}`: both declare role `{role}` of pairing `{name}:{tag}` (roles must be complementary)"
    )]
    PairingRolesNotComplementary {
        a: String,
        b: String,
        role: String,
        name: String,
        tag: String,
    },
    #[error(
        "cannot pair `{a}` (pairing `{name_a}:{tag_a}`) with `{b}` (pairing `{name_b}:{tag_b}`): both slots must reference the same pairing"
    )]
    PairingMismatch {
        a: String,
        name_a: String,
        tag_a: String,
        b: String,
        name_b: String,
        tag_b: String,
    },
    #[error(
        "cannot pair `{a}` with `{b}`: their pinned sha256 for pairing `{name}:{tag}` differ (`{sha_a}` vs `{sha_b}`)"
    )]
    PairingShaMismatch {
        a: String,
        sha_a: String,
        b: String,
        sha_b: String,
        name: String,
        tag: String,
    },

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
    /// Stack-wide `instance_id` collision: the candidate `instance_id`
    /// is already tracked by a *different* `(node_name, node_tag)` than
    /// the one being spawned. Bindings address producers by raw
    /// `instance_id` so duplicates across the stack would make
    /// `--bind KEY@id` ambiguous. The validator catches this at plan
    /// time; this is the daemon's defensive backstop at the trust
    /// boundary.
    #[error(
        "Instance ID `{instance_id}` is already tracked by `{existing_node_name}`:{existing_node_tag}; instance_ids must be unique across the entire stack"
    )]
    DuplicateInstanceIdAcrossStack {
        instance_id: String,
        existing_node_name: String,
        existing_node_tag: String,
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

    // -- caller-driven (service/action) cycle errors
    /// A service or action dependency forms a cycle, whether routed directly
    /// or through a contract. Caller-driven request/response cycles deadlock
    /// at runtime, so only topics may be bidirectional. Detection is
    /// type-level, so a service/action contract with several conforming
    /// providers can be rejected even when a specific binding would avoid the
    /// cycle: pin the binding or split the contract.
    #[error(
        "{kind} dependency cycle through contracts involving {} (closing dependency `{closing_dependency}`). \
         {kind} request/response cycles deadlock and are not allowed; only topics may be bidirectional. \
         If these providers are not actually cross-bound, pin the binding or split the contract.",
        .nodes.join(" -> ")
    )]
    ServiceActionContractCycle {
        nodes: Vec<String>,
        closing_dependency: String,
        kind: String,
    },
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
