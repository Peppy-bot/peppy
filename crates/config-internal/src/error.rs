use thiserror::Error;

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
    #[error("Invalid name: {0}, allowed characters: {1}")]
    InvalidName(String, String),

    #[error("Invalid instance id: {0}, allowed characters: {1}")]
    InvalidInstanceId(String, String),
    #[error("Empty instance id")]
    EmptyInstanceId,

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

    #[error("{0}")]
    Structured(String),
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub enum StructuredError {
    InvalidDeploymentSource(String),
    DuplicateInstanceId(String),
    InvalidName { name: String, allowed: String },
    InvalidInstanceId { id: String, allowed: String },
    EmptyInstanceId,
}

impl From<serde_json5::Error> for ParsingError {
    fn from(err: serde_json5::Error) -> Self {
        match err {
            serde_json5::Error::Message { msg, .. } => {
                // Try to deserialize the message as a StructuredError
                if let Ok(structured) = serde_json5::from_str::<StructuredError>(&msg) {
                    match structured {
                        StructuredError::InvalidDeploymentSource(detail) => {
                            ParsingError::InvalidDeploymentSource(detail)
                        }
                        StructuredError::DuplicateInstanceId(id) => {
                            ParsingError::DuplicateInstanceId(id)
                        }
                        StructuredError::InvalidName { name, allowed } => {
                            ParsingError::InvalidName(name, allowed)
                        }
                        StructuredError::InvalidInstanceId { id, allowed } => {
                            ParsingError::InvalidInstanceId(id, allowed)
                        }
                        StructuredError::EmptyInstanceId => ParsingError::EmptyInstanceId,
                    }
                } else {
                    // Fallback for standard serde errors or unparseable messages
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
    #[error("Duplicate instance id: {0}")]
    DuplicateInstanceIdSerde(String),

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_structured_error_deserialization() {
        // Helper to create a serde_json5 error from a string
        fn make_err(msg: &str) -> serde_json5::Error {
            serde::de::Error::custom(msg)
        }

        // InvalidDeploymentSource
        let json = serde_json5::to_string(&StructuredError::InvalidDeploymentSource(
            "bad source".to_string(),
        ))
        .unwrap();
        let err = ParsingError::from(make_err(&json));
        if let ParsingError::InvalidDeploymentSource(msg) = err {
            assert_eq!(msg, "bad source");
        } else {
            panic!("Expected InvalidDeploymentSource, got {:?}", err);
        }

        // DuplicateInstanceId
        let json = serde_json5::to_string(&StructuredError::DuplicateInstanceId("id1".to_string()))
            .unwrap();
        let err = ParsingError::from(make_err(&json));
        if let ParsingError::DuplicateInstanceId(id) = err {
            assert_eq!(id, "id1");
        } else {
            panic!("Expected DuplicateInstanceId, got {:?}", err);
        }

        // InvalidName
        let json = serde_json5::to_string(&StructuredError::InvalidName {
            name: "bad".to_string(),
            allowed: "a-z".to_string(),
        })
        .unwrap();
        let err = ParsingError::from(make_err(&json));
        if let ParsingError::InvalidName(name, allowed) = err {
            assert_eq!(name, "bad");
            assert_eq!(allowed, "a-z");
        } else {
            panic!("Expected InvalidName, got {:?}", err);
        }

        // InvalidInstanceId
        let json = serde_json5::to_string(&StructuredError::InvalidInstanceId {
            id: "bad_id".to_string(),
            allowed: "a-z".to_string(),
        })
        .unwrap();
        let err = ParsingError::from(make_err(&json));
        if let ParsingError::InvalidInstanceId(id, allowed) = err {
            assert_eq!(id, "bad_id");
            assert_eq!(allowed, "a-z");
        } else {
            panic!("Expected InvalidInstanceId, got {:?}", err);
        }

        // EmptyInstanceId
        let json = serde_json5::to_string(&StructuredError::EmptyInstanceId).unwrap();
        let err = ParsingError::from(make_err(&json));
        if !matches!(err, ParsingError::EmptyInstanceId) {
            panic!("Expected EmptyInstanceId, got {:?}", err);
        }
    }

    #[test]
    fn test_fallback_mechanism() {
        fn make_err(msg: &str) -> serde_json5::Error {
            serde::de::Error::custom(msg)
        }

        let raw_msg = "This is not JSON";
        let err = ParsingError::from(make_err(raw_msg));
        if let ParsingError::CannotParseConfig(msg) = err {
            assert_eq!(msg, raw_msg);
        } else {
            panic!("Expected CannotParseConfig, got {:?}", err);
        }

        let broken_json = "{ invalid json";
        let err = ParsingError::from(make_err(broken_json));
        if let ParsingError::CannotParseConfig(msg) = err {
            assert_eq!(msg, broken_json);
        } else {
            panic!("Expected CannotParseConfig, got {:?}", err);
        }
    }
}
