use capnp::Error as CapnpError;
use peppylib::PeppyError;
use std::env::VarError;
use thiserror::Error;

pub type Result<T> = core::result::Result<T, Error>;

#[derive(Debug, Error)]
pub enum Error {
    #[error("invalid node name `{node_name}`. Unauthorized characters found")]
    InvalidNodeName { node_name: String },
    #[error("failed to create messenger for topic `{topic_name}` on {host}:{port}")]
    TopicMessengerConnect {
        topic_name: String,
        host: String,
        port: u16,
        #[source]
        source: PeppyError,
    },
    #[error("failed to create messenger for node `{node_name}` on {host}:{port}")]
    NodeMessengerConnect {
        node_name: String,
        host: String,
        port: u16,
        #[source]
        source: PeppyError,
    },
    #[error("failed to subscribe to topic `{topic_name}` in namespace `{namespace}`")]
    TopicSubscribe {
        topic_name: String,
        namespace: String,
        #[source]
        source: PeppyError,
    },
    #[error("subscription to `{topic_name}` closed without yielding a message")]
    SubscriptionClosed { topic_name: String },
    #[error("failed to build blocking runtime for `{context}`")]
    RuntimeInitialization {
        context: String,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to serialize Cap'n Proto message for `{context}`")]
    CapnpSerialize {
        context: String,
        #[source]
        source: CapnpError,
    },
    #[error("failed to deserialize Cap'n Proto message for `{context}`")]
    CapnpDeserialize {
        context: String,
        #[source]
        source: CapnpError,
    },
    #[error("failed to read Cap'n Proto field `{field}` for `{context}`")]
    CapnpField {
        field: String,
        context: String,
        #[source]
        source: CapnpError,
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
    #[error("message format for `{context}` is not available in the generator")]
    MessageFormatUnavailable { context: String },
    #[error("failed to read `{var}` from the environment")]
    MissingInstanceIdEnvVar {
        var: &'static str,
        #[source]
        source: VarError,
    },
    #[error(transparent)]
    Messaging(#[from] PeppyError),
}
