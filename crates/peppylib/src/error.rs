use std::env::VarError;
use std::fmt;
use thiserror::Error;

pub type Result<T> = core::result::Result<T, Error>;

/// Errors that can occur during parameter deserialization or validation.
#[derive(Debug)]
pub struct ParameterDeserializationError(pub Vec<String>);

impl ParameterDeserializationError {
    pub fn single(message: impl Into<String>) -> Self {
        Self(vec![message.into()])
    }

    pub fn multiple(messages: Vec<String>) -> Self {
        Self(messages)
    }
}

impl fmt::Display for ParameterDeserializationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0.as_slice() {
            [] => write!(f, "parameter deserialization error: unknown error"),
            [single] => write!(f, "parameter deserialization error: {}", single),
            multiple => {
                write!(f, "missing required parameters:")?;
                for msg in multiple {
                    write!(f, "\n  - {}", msg)?;
                }
                Ok(())
            }
        }
    }
}

impl std::error::Error for ParameterDeserializationError {}

#[derive(Debug, Error)]
pub enum Error {
    // -- general
    #[error(transparent)]
    Io(#[from] std::io::Error),

    // -- config
    #[error(transparent)]
    Config(#[from] config::ConfigError),

    // -- serde
    #[error(transparent)]
    SerdeJson5(#[from] serde_json5::Error),

    // -- pmi-internal
    #[error(transparent)]
    PeppyMessagingInterface(#[from] pmi::PeppyMessagingInterfaceError),

    #[error("invalid service request '{identifier}': {reason}")]
    InvalidServiceRequest { identifier: String, reason: String },

    #[error("service request stream closed unexpectedly")]
    ServiceRequestStreamClosed,

    #[error("action feedback channel closed unexpectedly")]
    ActionFeedbackChannelClosed,

    // -- topics/services/actions errors
    #[error(
        "service '{service_name}'{instance_suffix} is unreachable",
        instance_suffix = InstanceSuffix(.instance_id.as_deref())
    )]
    ServiceUnreachable {
        instance_id: Option<String>,
        service_name: String,
    },
    #[error(
        "service '{service_name}'{instance_suffix} has timed out",
        instance_suffix = InstanceSuffix(.instance_id.as_deref())
    )]
    ServiceTimeout {
        instance_id: Option<String>,
        service_name: String,
    },
    #[error(
        "service '{service_name}'{instance_suffix} returned error: {reason}",
        instance_suffix = InstanceSuffix(.instance_id.as_deref())
    )]
    ServiceError {
        instance_id: Option<String>,
        service_name: String,
        reason: String,
    },
    #[error(
        "action '{action_name}'{instance_suffix} has timed out waiting for result",
        instance_suffix = InstanceSuffix(.instance_id.as_deref())
    )]
    ActionResultTimeout {
        instance_id: Option<String>,
        action_name: String,
    },
    #[error(
        "action '{action_name}'{instance_suffix} is unreachable for result",
        instance_suffix = InstanceSuffix(.instance_id.as_deref())
    )]
    ActionResultUnreachable {
        instance_id: Option<String>,
        action_name: String,
    },

    // -- system
    #[error("failed to read `{var}` from the environment")]
    MissingInstanceIdEnvVar {
        var: &'static str,
        #[source]
        source: VarError,
    },

    #[error("failed to read launch config at `{path}`")]
    LaunchConfigRead {
        path: String,
        #[source]
        source: std::io::Error,
    },

    #[error("failed to parse launch config at `{path}`")]
    LaunchConfigParse {
        path: String,
        #[source]
        source: serde_json5::Error,
    },

    #[error(
        "peppy config fingerprint mismatch for `{path}` (expected `{expected}`, got `{actual}`)"
    )]
    PeppyConfigFingerprintMismatch {
        path: String,
        expected: String,
        actual: String,
    },

    #[error("failed to read codegen fingerprint at `{path}`")]
    CodegenFingerprintRead {
        path: String,
        #[source]
        source: std::io::Error,
    },

    #[error(transparent)]
    ParameterTypeMismatch(#[from] config::TypeMismatch),

    #[error("missing parameter `{path}` in compiled node parameters")]
    MissingCompiledParameter { path: String },

    #[error(transparent)]
    ParameterDeserialization(#[from] ParameterDeserializationError),

    // --- Capnp
    #[error("capnp encoding error: {0}")]
    Capnp(#[from] capnp::Error),

    #[error("capnp schema error: {0}")]
    CapnpNotInSchema(#[from] capnp::NotInSchema),

    #[error("failed to serialize Cap'n Proto message for `{context}`")]
    CapnpSerialize {
        context: String,
        #[source]
        source: capnp::Error,
    },

    #[error("failed to deserialize Cap'n Proto message for `{context}`")]
    CapnpDeserialize {
        context: String,
        #[source]
        source: capnp::Error,
    },

    #[error("failed to read Cap'n Proto field `{field}` for `{context}`")]
    CapnpField {
        field: String,
        context: String,
        #[source]
        source: capnp::Error,
    },

    #[error("expected {expected} bytes for `{field}` but received {actual}")]
    InvalidFixedBytes {
        field: String,
        expected: usize,
        actual: usize,
    },

    #[error("expected {expected} elements for `{field}` but received {actual}")]
    InvalidFixedListLength {
        field: String,
        expected: usize,
        actual: usize,
    },

    // --- Runner
    #[error("failed to build blocking runtime for `{context}`")]
    RuntimeInitialization {
        context: String,
        #[source]
        source: std::io::Error,
    },

    // --- Node/Topic
    #[error("invalid node name `{node_name}`: {reason}")]
    InvalidNodeName { node_name: String, reason: String },

    #[error("invalid master node name `{node_name}`: {reason}")]
    InvalidMasterNodeName { node_name: String, reason: String },

    #[error("failed to create messenger for topic `{topic_name}` on {host}:{port}, {source_msg}")]
    TopicMessengerConnect {
        topic_name: String,
        host: String,
        port: u16,
        source_msg: String,
    },

    #[error("failed to create messenger for node `{node_name}` on {host}:{port}, {source_msg}")]
    NodeMessengerConnect {
        node_name: String,
        host: String,
        port: u16,
        source_msg: String,
    },

    #[error("failed to subscribe to topic `{topic_name}` in node `{node_name}`, {source_msg}")]
    TopicSubscribe {
        topic_name: String,
        node_name: String,
        source_msg: String,
    },

    #[error("subscription to `{topic_name}` closed without yielding a message")]
    SubscriptionClosed { topic_name: String },

    #[error("message format for `{context}` is not available in the generator")]
    MessageFormatUnavailable { context: String },
}

struct InstanceSuffix<'a>(Option<&'a str>);

impl fmt::Display for InstanceSuffix<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(instance_id) = self.0 {
            write!(f, " for instance '{instance_id}'")
        } else {
            Ok(())
        }
    }
}
