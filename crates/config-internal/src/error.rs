use thiserror::Error;

pub(crate) const DUPLICATE_INSTANCE_ID_ERROR_PREFIX: &str = "Duplicate instance id: ";

pub type Result<T> = core::result::Result<T, Error>;

#[derive(Debug, Error, Clone)]
pub enum ParsingError {
    // -- General yaml syntax
    #[error("Cannot read: {0}")]
    CannotRead(String),
    #[error("Cannot parse configuration: {0}")]
    CannotParseConfig(String),
    #[error("Empty content found in: {0}")]
    EmptyContent(String),
    #[error("Invalid file name: expected {expected}, found {found}")]
    InvalidFileName { expected: String, found: String },

    // -- node_config
    #[error("Invalid name: {0}")]
    InvalidName(String),
    #[error("Invalid namespace: {0}")]
    InvalidNamespace(String),

    // -- types
    #[error("Invalid scalar type {0}: {1}")]
    InvalidScalar(String, String), // type, value
    #[error("Bad array found: {0}")]
    BadArray(String),
    #[error("Invalid QoS type {0}")]
    InValidQoS(String),

    // -- schema conformance
    #[error("Unknown key in {0}: {1}")]
    UnknownKey(String, String),

    #[error("Deleted file {0}")]
    DeletedFile(String),

    // -- deployments
    #[error("Invalid deployment source: {0}")]
    InvalidDeploymentSource(String),
    #[error("Duplicate instance id: {0}")]
    DuplicateInstanceId(String),
}

impl From<serde_json5::Error> for ParsingError {
    fn from(err: serde_json5::Error) -> Self {
        match err {
            serde_json5::Error::Message { msg, .. } => {
                if let Some(detail) = msg.strip_prefix("Invalid deployment source: ") {
                    ParsingError::InvalidDeploymentSource(detail.to_string())
                } else if let Some(duplicate) = msg.strip_prefix(DUPLICATE_INSTANCE_ID_ERROR_PREFIX)
                {
                    ParsingError::DuplicateInstanceId(duplicate.to_string())
                } else {
                    ParsingError::CannotParseConfig(msg)
                }
            }
        }
    }
}

#[derive(Debug, Error)]
pub enum Error {
    // -- general
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("Capnp error: {0}")]
    Capnp(#[from] capnp::Error),

    // -- Parsing error
    #[error(transparent)]
    Parsing(#[from] ParsingError),
    #[error("Serialize error: {0}")]
    Serialize(String),

    // -- Domain specific
    #[error("Unsupported language")]
    UnsupportedLanguage,

    // -- Node watcher
    #[error("Node watcher error: {0}")]
    NodeWatcher(String),

    // -- Askama
    #[error("Askama error: {0}")]
    AskamaError(String),
    #[error("Encoding error: {0}")]
    Encoding(String),
}
