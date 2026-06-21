#![forbid(unsafe_code)]

//! Parsing, validation, and encoding of Peppy configuration documents.
//!
//! This crate owns the on-disk Peppy config formats (`peppy.json5` node
//! configs, launcher files, `peppy_config.json5`) and the typed API the rest
//! of the workspace builds on: parsing those documents, validating them against
//! Peppy's schema and structural constraints, encoding a [`node::MessageFormat`]
//! to Cap'n Proto, and the [`consts::PeppyDirs`] filesystem-layout helper.
//!
//! Boundaries: it performs no I/O beyond reading/writing config files and
//! fingerprints and extracting the bundled Cap'n Proto binary, and requires no
//! global init — every type is built through an explicit parser/constructor.
//! The one process-global is [`consts::set_app_env`], a set-once `OnceLock`
//! that only shifts the default [`consts::PeppyDirs`] root between dev/prod.

mod common;
mod error;
mod parsing;

/// Private module that contains all implementation modules.
/// The `#[path = "."]` attribute tells Rust to resolve child modules from `src/`,
/// the same directory as this file, so existing file paths are preserved.
#[path = "."]
mod internal {
    pub mod atomic_write;
    pub mod consts;
    pub mod encoding;
    pub mod fingerprint;
    pub mod interface;
    pub mod json5_pretty;
    pub mod launcher;
    pub mod node;
    pub mod peppy_config;
    pub mod repo_node_id;
    pub mod runtime;
    pub mod schema;
    pub mod source;
}

// -- common --
pub use common::{
    AnyType, DefaultValue, NodeArguments, NodeArgumentsError, ParameterSchema, ParameterSpec,
    TypeMismatch, apply_parameter_defaults, resolve_argument_path, type_token_name,
    validate_node_arguments,
};

// -- error --
pub use error::{
    BindingMissingForPinnedDep, BindingTargetMismatch, DuplicateInstanceIdAcrossStack,
    Error as ConfigError, MissingInterface, ParsingError, SlotKind, format_bulleted,
};

// -- atomic_write --
pub mod atomic_write {
    pub use crate::internal::atomic_write::publish_atomic;
}

// -- consts --
pub mod consts {
    pub use crate::internal::consts::{
        ALLOWED_CONFIG_CHARS, AppEnv, CREDENTIALS_FILE, DAEMON_STATE_FILE_ENV,
        DEFAULT_ALPINE_BASE_IMAGE, DEFAULT_LINK_ID_SENTINEL, DEFAULT_MESSAGING_HOST,
        DEFAULT_MESSAGING_PORT, DEFAULT_PYTHON_BASE_IMAGE, DEFAULT_RUST_BASE_IMAGE,
        NODE_CONFIG_FILE, PEPPY_HOME_ENV, PEPPY_MESSAGING_PORT_VAR_NAME, PEPPY_OUTPUT_DIR,
        PEPPYGEN_OUTPUT_PATH, PEPPYLIB_OUTPUT_PATH, PYTHON_MAX_VERSION, PYTHON_MIN_VERSION,
        PeppyDirs, RUNTIME_CONFIG_VAR_NAME, peppy_root_dir, set_app_env,
    };
}

// -- encoding --
pub mod encoding {
    pub use crate::internal::encoding::{
        CapnpSchemaArtifacts, FunctionParam, MessageFormatMapper, compile_capnp,
    };
}

// -- json5_pretty --
pub mod json5_pretty {
    pub use crate::internal::json5_pretty::to_string_pretty;
}

// -- fingerprint --
pub mod fingerprint {
    pub use crate::internal::fingerprint::{
        fingerprint_for_bytes, generate_node_config_fingerprint, read_codegen_fingerprint,
        verify_codegen_fingerprint,
    };

    #[cfg(feature = "test_helpers")]
    pub use crate::internal::fingerprint::{
        create_codegen_fingerprint, create_wrong_codegen_fingerprint,
    };
}

// -- node --
pub mod node {
    pub use crate::internal::node::{
        ActionInterfaces, ActionServiceEndpoint, ActionTopicEndpoint, ArrayKind, ArraySchema,
        ConformsToItem, ConsumedAction, ConsumedService, ConsumedTopic, ContainerConfig,
        DependencySpec, DependsOn, EmittedTopic, Execution, ExposedAction, ExposedService,
        InterfaceConformanceEdge, InterfaceKind, Interfaces, Manifest, MessageFormat,
        MessageSizeEstimate, Name, NodeConfig, NodeConfigParser, NodeDependency, ObjectKind,
        ObjectSchema, PeppygenLanguage, PrimitiveSchema, QoSProfile, SchemaType, ServiceInterfaces,
        Toolchain, TopicInterfaces, TypeToken, collect_dependency_specs,
        collect_interface_conformance_edges, estimate_serialized_size, is_blocked_mount_source,
        load_standalone_node_config, node_conforms_to, validate_dependency_specs,
    };
}

// -- runtime --
pub mod runtime {
    pub use crate::internal::runtime::{
        DiscoveryConfig, LifecycleRuntimeConfig, NodeInstanceConfig, ProducerRef,
        ResolvedFramework, RuntimeConfig, SlotBinding,
    };
}

// -- peppy_config --
pub mod peppy_config {
    pub use crate::internal::peppy_config::{
        DAEMON_HEARTBEAT_INTERVAL_SECS, DEFAULT_API_URL, DEFAULT_DAEMON_GRACE_SECS,
        DEFAULT_HIGH_THROUGHPUT_BUFFER_SIZE, DEFAULT_SHUTDOWN_GRACE_SECS,
        DEFAULT_STANDARD_BUFFER_SIZE, EVENT_LOOP_JOIN_BUDGET_SECS, LifecycleConfig, Mode,
        PeerConfig, PeppyConfig, RUNTIME_FINALIZE_MARGIN_SECS, ResourceServers, load_or_create,
    };
}

// -- launcher --
pub mod launcher {
    pub use crate::internal::launcher::{
        BindingValidationItem, Deployment, DeploymentGitSource, DeploymentInstance,
        DeploymentLocalSource, DeploymentRepoSource, DeploymentSource, DeploymentUrlSource,
        FrameworkOverrides, Name, PeppyLauncher, PeppyLauncherParser, ValidatedBindings,
        validate_bindings,
    };
}

// -- schema --
pub mod schema {
    pub use crate::internal::schema::PeppySchema;
}

// -- interface --
pub mod interface {
    pub use crate::internal::interface::{
        Interfaces, Manifest, PeppyInterface, PeppyInterfaceParser,
    };
}

// -- source --
pub mod source {
    pub use crate::internal::source::{
        DeploymentGitSource, DeploymentLocalSource, DeploymentRepoSource, DeploymentSource,
        DeploymentUrlSource,
    };
}

// -- repo node id --
pub mod repo_node_id {
    pub use crate::internal::repo_node_id::{validate_repo_node_name, validate_repo_node_tag};
}

#[cfg(feature = "test_helpers")]
pub mod test_helpers;
