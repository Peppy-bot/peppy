#![forbid(unsafe_code)]

//! Parsing and validation of the daemon-side Peppy configuration documents.
//!
//! This crate owns the config formats only the peppy daemon and CLI read or
//! write: launcher documents (`peppy_schema: "launcher/v1"`) and their
//! deployment sources, interface documents (`interface/v1`), the global
//! daemon config `peppy_config.json5` with its comment-preserving completion,
//! the [`atomic_write::publish_atomic`] staging helper, and the
//! [`consts::PeppyDirs`] filesystem-layout helper with the process-global
//! [`consts::set_app_env`] dev/prod root switch (a set-once `OnceLock`).
//!
//! It builds on the shared `config` crate (`peppy-config-model`), which keeps
//! the wire-facing tier consumed by nodes and `peppylib`: the `peppy.json5`
//! node config model, runtime configs, fingerprints, org namespaces, and
//! schema tags. Types crossing that boundary (`config::runtime::Name`,
//! `config::node` manifest types, `config::peppy_config::PeerConfig`) are
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
    pub mod interface;
    pub mod launcher;
    pub mod pairing;
    pub mod peppy_config;
    pub mod source;
}

// -- error --
pub use error::{
    BindingTargetMismatch, BindingUnknownSlot, DuplicateInstanceIdAcrossStack,
    Error as DaemonConfigError, ParsingError, SlotKind, format_bulleted,
};

// -- atomic_write --
pub mod atomic_write {
    pub use crate::internal::atomic_write::publish_atomic;
}

// -- consts --
pub mod consts {
    pub use crate::internal::consts::{
        AppEnv, CREDENTIALS_FILE, DAEMON_STATE_FILE_ENV, DEFAULT_ALPINE_BASE_IMAGE,
        DEFAULT_PYTHON_BASE_IMAGE, DEFAULT_RUST_BASE_IMAGE, PEPPY_MESSAGING_PORT_VAR_NAME,
        PEPPY_OUTPUT_DIR, PEPPYLIB_OUTPUT_PATH, PeppyDirs, non_empty_env_path, peppy_root_dir,
        set_app_env,
    };
}

// -- peppy_config --
pub mod peppy_config {
    pub use crate::internal::peppy_config::{
        DAEMON_HEARTBEAT_INTERVAL_SECS, DEFAULT_API_URL, DEFAULT_FEDERATION_CONNECT_TIMEOUT_SECS,
        FederationConfig, LifecycleConfig, MAX_CORE_NODE_NAME_LEN, Mode, PeppyConfig,
        ResourceServers, load_or_create,
    };
}

// -- launcher --
pub mod launcher {
    pub use crate::internal::launcher::{
        AlreadyPairedSlots, BindingValidationItem, Deployment, DeploymentGitSource,
        DeploymentInstance, DeploymentLocalSource, DeploymentRepoSource, DeploymentSource,
        DeploymentUrlSource, FrameworkOverrides, PairingValidationItem, PeppyLauncher,
        PeppyLauncherParser, PlannedPairEndpoint, PlannedPairing, ValidatedBindings,
        ValidatedPairings, split_pair_target, validate_bindings, validate_pairings,
    };
}

// -- interface --
pub mod interface {
    pub use crate::internal::interface::{
        Interfaces, Manifest, PeppyInterface, PeppyInterfaceParser,
    };
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
