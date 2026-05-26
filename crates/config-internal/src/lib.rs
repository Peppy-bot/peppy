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
    pub mod repo_node_id;
    pub mod runtime;
    pub mod schema;
    pub mod source;
}

// -- common --
pub use common::{
    AnyType, DefaultValue, NodeArguments, NodeArgumentsError, ParameterSchema, ParameterSpec,
    TypeMismatch, apply_parameter_defaults, parse_node_arguments, resolve_argument_path,
    resolve_parameter_path, type_token_name, validate_node_arguments,
};

// -- error --
pub use error::{
    BindingMissingForPinnedDep, BindingTargetMismatch, DuplicateInstanceIdAcrossStack,
    Error as ConfigError, MissingInterface, ParsingError, Result as ConfigResult, SlotKind,
};

// -- atomic_write --
pub mod atomic_write {
    pub use crate::internal::atomic_write::publish_atomic;
}

// -- consts --
pub mod consts {
    pub use crate::internal::consts::{
        ALLOWED_CONFIG_CHARS, AppEnv, CORE_NODE_TOPIC_NAME, DAEMON_STATE_FILE_ENV,
        DEFAULT_ALPINE_BASE_IMAGE, DEFAULT_LINK_ID_SENTINEL, DEFAULT_MESSAGING_HOST,
        DEFAULT_MESSAGING_PORT, DEFAULT_PYTHON_BASE_IMAGE, DEFAULT_RUST_BASE_IMAGE,
        NODE_CONFIG_FILE, PEPPY_MESSAGING_PORT_VAR_NAME, PEPPY_OUTPUT_DIR, PEPPYGEN_OUTPUT_PATH,
        PEPPYLIB_OUTPUT_PATH, PYTHON_MAX_VERSION, PYTHON_MIN_VERSION, PeppyDirs,
        RUNTIME_CONFIG_VAR_NAME, app_env, set_app_env,
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
        create_wrong_release_fingerprint,
    };
}

// -- node --
pub mod node {
    pub use crate::internal::node::{
        ActionInterfaces, ActionServiceEndpoint, ActionTopicEndpoint, ArrayKind, ArraySchema,
        CallbackNameError, ConformsToItem, ConsumedAction, ConsumedService, ConsumedTopic,
        ContainerConfig, DependencySpec, DependsOn, EmittedTopic, Execution, ExposedAction,
        ExposedService, ExternalConsumedTopic, InterfaceKind, Interfaces, LinkedConsumedTopic,
        Manifest, MessageFormat, Name, NodeConfig, NodeConfigCreator, NodeConfigParser,
        NodeDependency, ObjectKind, ObjectSchema, PeppygenLanguage, PrimitiveSchema, QoSProfile,
        SchemaType, ServiceInterfaces, Toolchain, TopicInterfaces, TypeToken,
        collect_dependency_specs, extract_parameter_refs, is_blocked_mount_source,
        load_standalone_node_config, validate_dependency_specs,
    };
}

// -- runtime --
pub mod runtime {
    pub use crate::internal::runtime::{
        LauncherRuntimeConfig, NodeInstanceConfig, ResolvedFramework, RuntimeConfig, SlotBinding,
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
