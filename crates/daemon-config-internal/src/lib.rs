#![forbid(unsafe_code)]

//! Parsing and validation of the daemon-side Peppy configuration documents.
//!
//! This crate owns the config formats only the peppy daemon and CLI read or
//! write: launcher documents (`peppy_schema: "launcher/v1"`) and their
//! deployment sources, contract documents (`contract/v1`), the global
//! daemon config `peppy_config.json5` with its comment-preserving completion,
//! the [`atomic_write::publish_atomic`] staging helper, and the
//! [`consts::PeppyDirs`] filesystem-layout helper with the process-global
//! [`consts::set_app_env`] dev/prod root switch (a set-once `OnceLock`).
//!
//! It builds on the shared `config` crate (`peppy-config-model`), which keeps
//! the wire-facing tier consumed by nodes and `peppylib`: the `peppy.json5`
//! node config model, runtime configs, fingerprints, workspace namespaces, and
//! schema tags. Types crossing that boundary (`config::runtime::Name`,
//! `config::node` manifest types, `config::peppy_config::SubscriberBufferConfig`) are
//! used directly from `config` so each has exactly one definition.

mod error;
mod parsing;

/// Private module that contains all implementation modules.
/// The `#[path = "."]` attribute tells Rust to resolve child modules from
/// `src/`, the same directory as this file, so existing file paths are
/// preserved.
#[path = "."]
mod internal {
    pub mod atomic_write;
    pub mod consts;
    pub mod contract;
    pub mod core_node_name;
    pub mod env;
    pub mod launcher;
    pub mod pairing;
    pub mod peppy_config;
    pub mod source;
}

// -- error --
pub use error::{
    BindingTargetMismatch, DuplicateInstanceIdAcrossStack, Error as DaemonConfigError,
    LinkUnknownSlot, ParsingError, SlotKind, format_bulleted, format_quoted_list,
};

// -- core_node_name --
//
// The ONE core-node-name validator. `peppy_config`, the daemon's serve flag,
// the CLI's `--core-node` override, `--place` targets, and launcher core node
// link ids all go through it, so the rules (charset, length cap, and the
// `self` reservation) are stated once instead of re-derived per call site.
pub mod core_node_name {
    pub use crate::internal::core_node_name::{CoreNodeName, CoreNodeNameError, SELF_CORE_NODE};
}

// -- atomic_write --
pub mod atomic_write {
    pub use crate::internal::atomic_write::publish_atomic;
}

// -- env --
pub mod env {
    pub use crate::internal::env::{
        InvalidEnvVar, check_env_var, is_forbidden_env_name, is_safe_env_value, is_valid_env_name,
    };
}

// -- consts --
pub mod consts {
    pub use crate::internal::consts::{
        AppEnv, CREDENTIALS_FILE, DEFAULT_ALPINE_BASE_IMAGE, DEFAULT_PYTHON_BASE_IMAGE,
        DEFAULT_RUST_BASE_IMAGE, PEPPY_MESSAGING_PORT_VAR_NAME, PEPPY_OUTPUT_DIR,
        PEPPYLIB_OUTPUT_PATH, PeppyDirs, non_empty_env_path, peppy_root_dir, set_app_env,
    };
}

// -- peppy_config --
pub mod peppy_config {
    pub use crate::internal::peppy_config::{
        DAEMON_HEARTBEAT_INTERVAL_SECS, DEFAULT_API_URL, DEFAULT_FEDERATION_CONNECT_TIMEOUT_SECS,
        ExternalZenohConfig, FederationConfig, LifecycleConfig, LocalNodesTopology,
        MAX_CORE_NODE_NAME_LEN, ManagedZenohConfig, ParsedEndpointBuf, PeppyConfig,
        ResourceServers, ZenohConfig, load_or_create,
    };
}

// -- launcher --
pub mod launcher {
    pub use crate::internal::launcher::{
        AlreadyPairedSlots, BindingValidationItem, Deployment, DeploymentGitSource,
        DeploymentInstance, DeploymentLocalSource, DeploymentRepoSource, DeploymentSource,
        DeploymentUrlSource, DuplicateLinkTarget, FrameworkOverrides, LinkTargets, LinkValue,
        PairingValidationItem, PeppyLauncher, PeppyLauncherParser, Placements, PlannedObservation,
        PlannedPairEndpoint, PlannedPairing, ValidatedBindings, ValidatedLinkPlan,
        ValidatedObservations, ValidatedPairings, split_link_target, validate_bindings,
        validate_link_plan, validate_link_slots, validate_observations, validate_pairings,
    };
}

// -- contract --
pub mod contract {
    pub use crate::internal::contract::{Interfaces, Manifest, PeppyContract, PeppyContractParser};
}

// -- pairing --
pub mod pairing {
    pub use crate::internal::pairing::{PairingTopic, PeppyPairing, PeppyPairingParser};
}

// -- source --
pub mod source {
    pub use crate::internal::source::{
        DeploymentGitSource, DeploymentLocalSource, DeploymentRepoSource, DeploymentSource,
        DeploymentUrlSource,
    };
}
