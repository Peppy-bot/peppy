use thiserror::Error;

pub type Result<T> = core::result::Result<T, Error>;

#[derive(Debug, Error, Clone)]
pub enum ParsingError {
    // -- General yaml syntax
    #[error("Cannot read: {0}")]
    CannotRead(String),
    #[error("Cannot parse YAML: {0}")]
    CannotParseYaml(String),
    #[error("Empty content found in: {0}")]
    EmptyContent(String),

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

    #[error("Deleted file {0}")]
    DeletedFile(String),
}

#[derive(Debug, Error)]
pub enum Error {
    // -- general
    #[error(transparent)]
    Io(#[from] std::io::Error),

    // -- Parsing error
    #[error(transparent)]
    Parsing(#[from] ParsingError),
    #[error("Serialize error: {0}")]
    Serialize(String),

    // -- Node watcher
    #[error("Node watcher error: {0}")]
    NodeWatcher(String),

    // -- Askama
    #[error("Askama error: {0}")]
    AskamaError(String),
}
