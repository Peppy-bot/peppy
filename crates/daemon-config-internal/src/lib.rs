#![forbid(unsafe_code)]

//! Parsing and validation of the daemon-side Peppy configuration documents.
//!
//! This crate owns the config formats only the peppy daemon and CLI read or
//! write: launcher documents (`peppy_schema: "launcher/v1"`) and their
//! deployment sources, contract documents (`contract/v1`), MCP exposure
//! documents (`mcp_exposure/v1`), the global
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
    pub mod env;
    pub mod launcher;
    pub mod mcp_deployment;
    pub mod mcp_exposure;
    pub mod pairing;
    pub mod peppy_config;
    pub mod repository;
    pub mod source;
}

// -- error --
pub use error::{
    BindingTargetMismatch, DuplicateInstanceIdAcrossStack, Error as DaemonConfigError,
    LinkUnknownSlot, ParsingError, SlotKind, format_bulleted, format_quoted_list,
};

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
        DEFAULT_RUST_BASE_IMAGE, PEPPY_GIT_TAG, PEPPY_MESSAGING_PORT_VAR_NAME, PEPPY_OUTPUT_DIR,
        PEPPY_VERSION, PEPPYLIB_OUTPUT_PATH, PeppyDirs, REPOSITORY_INDEX_FILE, non_empty_env_path,
        peppy_root_dir, set_app_env,
    };
}

// -- peppy_config --
pub mod peppy_config {
    pub use crate::internal::peppy_config::{
        DAEMON_HEARTBEAT_INTERVAL_SECS, DEFAULT_API_URL, DEFAULT_FEDERATION_CONNECT_TIMEOUT_SECS,
        ExternalZenohConfig, FederationConfig, LifecycleConfig, LocalNodesTopology,
        ManagedZenohConfig, ParsedEndpointBuf, PeppyConfig, ResourceServers, ZenohConfig,
        load_or_create,
    };
}

// -- launcher --
pub mod launcher {
    pub use crate::internal::launcher::{
        Adjustment, AlreadyPairedSlots, AppliedAdjustment, AppliedChange, ArgumentOverrides,
        BindingValidationItem, ComponentAxis, ComponentCardinality, ComposedJoin, ComposedLaunch,
        CompositionError, CompositionReport, CopyEntry, CopyRecord, Deployment, DeploymentInstance,
        DeploymentSource, DuplicateLinkTarget, EmptyVacantReason, ExternallyCoveredSlots, Fragment,
        FragmentPart, FragmentSpec, FrameworkOverrides, JoinRequest, LauncherFragment,
        LauncherFragmentParser, LinkTargets, LinkValue, OptionDeployment, PairingValidationItem,
        PeppyLauncher, PeppyLauncherParser, Placements, PlannedObservation, PlannedPairEndpoint,
        PlannedPairing, PreparedLauncher, RunningStack, Selection, SelectionConstraint,
        SelectionEntry, SelectionSource, SkipReason, SkippedAdjustment, UnitSelection,
        VacantReason, ValidatedBindings, ValidatedLinkPlan, ValidatedObservations,
        ValidatedPairings, check_composition, participant_vacancies, split_link_target,
        validate_bindings, validate_link_plan, validate_link_slots, validate_observations,
        validate_pairings, validate_sim_time_source,
    };
}

// -- contract --
pub mod contract {
    pub use crate::internal::contract::{Interfaces, Manifest, PeppyContract, PeppyContractParser};
}

// -- mcp_exposure --
pub mod mcp_exposure {
    pub use crate::internal::mcp_exposure::{
        ActionExposure, ActionOperation, ExposureManifest, ExposureTarget, FreshnessPolicy,
        ImageCodec, ImageFieldMap, ImageRepresentation, JpegQuality, MaxHz, McpExposure,
        OversizePolicy, PeppyMcpExposureParser, PinnedContractRef, PublicName, RestrictBounds,
        ServerIdentity, ServiceExposure, ServiceOperation, TopicExposure, UpdatePolicy,
    };
}

// -- mcp_deployment --
//
// The built-in MCP server as a deployment: the identity and manifest the
// daemon synthesizes from a set of exposures, the slot-merging rules, and
// the spec file the daemon hands `peppy mcp serve`.
pub mod mcp_deployment {
    pub use crate::internal::mcp_deployment::{
        BUILT_IN_TAG, DEFAULT_PORT, ExposureViolations, McpDeploymentError, McpDeploymentPlan,
        McpServeSpec, PORT_PARAMETER, Pinned, PinnedContract, PinnedDocument, PinnedExposure,
        RUN_COMMAND, SPEC_ENV_VAR, SlotConflict, built_in_identity, plan_deployment,
    };
}

// -- pairing --
pub mod pairing {
    pub use crate::internal::pairing::{PairingTopic, PeppyPairing, PeppyPairingParser};
}

// -- repository --
pub mod repository {
    pub use crate::internal::repository::{
        DeclaredItem, DeclaredPaths, DeploymentPins, DeploymentRoot, EntryOrigin, GitCommit,
        GitCommitError, IndexedItem, ItemName, ItemTag, ManifestFingerprint,
        ManifestFingerprintError, PeppyRepositoryIndexParser, PinKind, PinnedItem, RepoItemKind,
        RepoPathError, RepoRelativePath, RepositoryIndex, TaggedSection, UniqueMap,
    };
}

// -- source --
pub mod source {
    pub use crate::internal::source::{DeploymentSource, ExposureRef, ItemRef};
}
