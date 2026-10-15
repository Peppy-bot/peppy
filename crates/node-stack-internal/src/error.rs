use std::path::PathBuf;

use config::{ConfigError, ParsingError};
use core_node_api::{InstanceState, duplicate_instance_id_message};
use thiserror::Error;

pub type Result<T> = core::result::Result<T, Error>;

#[derive(Debug, Error)]
pub enum Error {
    // -- config
    #[error(transparent)]
    Config(#[from] ConfigError),

    // -- pairing registry errors
    #[error("instance `{instance_id}` declares no pairing slot `{link_id}` in depends_on.pairings")]
    PairingSlotNotFound {
        instance_id: String,
        link_id: String,
    },
    #[error("pairing endpoint instance `{instance_id}` is not in this stack")]
    PairingInstanceNotInStack { instance_id: String },
    #[error("pairing endpoint instance `{instance_id}` is {state}, with no process to pair")]
    PairingInstanceWithoutProcess {
        instance_id: String,
        state: InstanceState,
    },
    #[error(
        "pairing slot `{slot}` is already paired with `{peer}`; a `one` or `zero_or_one` slot holds one pair until it is cleared"
    )]
    PairingSlotAlreadyPaired { slot: String, peer: String },
    #[error(
        "pairing slot `{slot}` already holds a pair with `{peer}`; a slot holds each pair once until it is cleared"
    )]
    PairAlreadyHeld { slot: String, peer: String },
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
    #[error("{}", duplicate_instance_id_message(instance_id, node_name, node_tag, core_node, *state))]
    DuplicateInstanceId {
        instance_id: String,
        node_name: String,
        node_tag: String,
        core_node: String,
        state: InstanceState,
    },
    #[error("instance `{instance_id}` is no longer waiting for a restart")]
    RespawnTargetMissing { instance_id: String },
    #[error("the start of instance `{instance_id}` was ended by a stop before it spawned")]
    StartClaimed { instance_id: String },
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
    /// type-level, so a service/action contract with several implementing
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
