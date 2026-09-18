#![forbid(unsafe_code)]

//! Parsing and validation of the shared Peppy configuration documents.
//!
//! This crate owns the wire-facing config tier every peppy surface builds
//! on: the `peppy.json5` node config model, the runtime configs shipped to
//! spawned nodes, codegen fingerprints, workspace namespaces, and the schema
//! tags identifying each document shape. (Cap'n Proto schema generation for a
//! [`node::MessageFormat`] lives in the separate `encoding` crate; the
//! daemon-side documents, launcher files, interface documents,
//! `peppy_config.json5`, and the `PeppyDirs` filesystem layout, live in the
//! peppy `daemon-config` crate, which builds on this one.)
//!
//! Boundaries: it performs no I/O beyond reading/writing config files and
//! fingerprints, holds no process-global state, and requires no global
//! init; every type is built through an explicit parser/constructor.

mod common;
mod error;
mod macros;
mod parsing;

/// Private module that contains all implementation modules.
/// The `#[path = "."]` attribute tells Rust to resolve child modules from `src/`,
/// the same directory as this file, so existing file paths are preserved.
#[path = "."]
mod internal {
    pub mod consts;
    pub mod fingerprint;
    pub mod namespace;
    pub mod node;
    pub mod peppy_config;
    pub mod repo_node_id;
    pub mod runtime;
    pub mod schema;
}

// -- common --
pub use common::{
    AnyType, DefaultValue, NodeArguments, NodeArgumentsError, ParameterSchema, ParameterSpec,
    TypeMismatch, apply_parameter_defaults, resolve_argument_path, type_token_name,
    validate_node_arguments,
};

// -- error --
pub use error::{
    ConsumedInterfaceOnlyContractBacked, ContractCoverageMismatch, Error as ConfigError,
    MissingInterface, PairingCoverageMismatch, ParsingError, RefinementMismatch,
    deserialize_json5_with_structured_errors, write_joined,
};

// -- consts --
pub mod consts {
    pub use crate::internal::consts::{
        ALLOWED_CONFIG_CHARS, DEFAULT_LINK_ID_SENTINEL, DEFAULT_MESSAGING_HOST,
        DEFAULT_MESSAGING_PORT, NODE_CONFIG_FILE, PEPPY_CONFIG_ENV, PEPPY_HOME_ENV,
        PEPPYGEN_OUTPUT_PATH, PYTHON_MAX_VERSION, PYTHON_MIN_VERSION, RUNTIME_CONFIG_VAR_NAME,
        normalize_tag,
    };
}

// -- fingerprint --
pub mod fingerprint {
    pub use crate::internal::fingerprint::{
        ManifestFingerprint, ManifestFingerprintError, fingerprint_for_bytes,
        generate_node_config_fingerprint, read_codegen_fingerprint, verify_codegen_fingerprint,
    };

    #[cfg(feature = "fingerprint_test_helpers")]
    pub use crate::internal::fingerprint::{
        create_codegen_fingerprint, create_wrong_codegen_fingerprint,
    };
}

// -- node --
pub mod node {
    pub use crate::internal::node::{
        ActionInterfaces, ActionRefinement, ActionTopicEndpoint, ArrayKind, ArraySchema,
        Cardinality, ConsumedAction, ConsumedService, ConsumedTopic, ContainerConfig,
        ContractImplementationEdge, DependencySpec, DependsOn, EmittedTopic, EndpointDeclaration,
        EndpointDeclarations, EndpointKind, EndpointLabel, Execution, ExposedAction,
        ExposedService, FieldRefinement, FormatRefinement, FormatRuleViolation,
        GoalServiceEndpoint, ImplementsEntry, InterfaceKind, Interfaces, LinkedAction, LinkedEntry,
        LinkedMember, LinkedService, LinkedTopic, Manifest, MessageFormat, MessageSizeEstimate,
        MissingDependencyPolicy, NativeEmittedTopic, NativeExposedAction, NativeExposedService,
        NodeConfig, NodeConfigParser, NodeDependency, ObjectKind, ObjectSchema,
        PairingObserverDependency, PairingParticipantDependency, PeppygenLanguage, PrimitiveSchema,
        QoSProfile, RefinementProblem, RefinementProblemKind, Refines, ResultServiceEndpoint,
        ResultServiceRefinement, SchemaType, ServiceInterfaces, ServiceRefinement, Toolchain,
        TopicInterfaces, TopicRefinement, TypeToken, collect_contract_implementation_edges,
        collect_dependency_specs, estimate_serialized_size, is_blocked_mount_source,
        load_standalone_node_config, node_implements, refined, refined_ref,
        validate_dependency_specs,
    };
}

// -- runtime --
pub mod runtime {
    pub use crate::internal::runtime::{
        BoundProducers, ClockBinding, ClockDomainId, ClockIncarnation, ClockRole, CoreNodeName,
        CoreNodeNameError, DiscoveryConfig, InvalidIncarnation, LifecycleRuntimeConfig,
        MAX_CORE_NODE_NAME_LEN, Name, NodeInstanceConfig, NodeInstancePlan, ObservationSeedMember,
        ObservationSeeds, ObservedPeer, PairedPeer, PairingSlots, ProducerRef, ResolvedFramework,
        RuntimeConfig, SELF_CORE_NODE, SlotBindings, first_duplicate,
    };
}

// -- namespace --
pub mod namespace {
    pub use crate::internal::namespace::{InvalidNamespace, LOCAL_NAMESPACE, Namespace};
}

// -- peppy_config --
pub mod peppy_config {
    pub use crate::internal::peppy_config::{
        DEFAULT_DAEMON_GRACE_SECS, DEFAULT_HIGH_THROUGHPUT_BUFFER_SIZE,
        DEFAULT_SHUTDOWN_GRACE_SECS, DEFAULT_STANDARD_BUFFER_SIZE, EVENT_LOOP_JOIN_BUDGET_SECS,
        RUNTIME_FINALIZE_MARGIN_SECS, SubscriberBufferConfig,
    };
}

// -- schema --
pub mod schema {
    pub use crate::internal::schema::PeppySchema;
}

// -- repo node id --
pub mod repo_node_id {
    pub use crate::internal::repo_node_id::{validate_repo_node_name, validate_repo_node_tag};
}
