use thiserror::Error;

pub type Result<T> = core::result::Result<T, Error>;

/// Deserializes JSON5 content with field-path tracking.
///
/// On error, prepends the JSON path (e.g. `execution.start_cmd`) to standard
/// serde error messages. `StructuredError`s (custom validation) are propagated
/// unchanged since they already contain descriptive messages.
pub fn deserialize_json5_with_path<'de, T>(content: &'de str) -> Result<T>
where
    T: serde::de::Deserialize<'de>,
{
    // Phase 1: parse JSON5 syntax. If this fails, there's no field path.
    let mut deserializer = serde_json5::Deserializer::from_str(content)
        .map_err(|e| Error::Parsing(ParsingError::from(e)))?;

    // Phase 2: deserialize with path tracking.
    serde_path_to_error::deserialize(&mut deserializer).map_err(|path_err| {
        let path = path_err.path().to_string();
        let inner: serde_json5::Error = path_err.into_inner();

        match inner {
            serde_json5::Error::Message { ref msg, .. } => {
                // Check if it's a StructuredError (custom validation).
                // These already have rich messages; don't prepend path.
                if let Ok(structured) = serde_json5::from_str::<StructuredError>(msg) {
                    return Error::Parsing(match structured {
                        StructuredError::InvalidDeploymentSource(detail) => {
                            ParsingError::InvalidDeploymentSource(detail)
                        }
                        StructuredError::DuplicateName(id) => ParsingError::DuplicateName(id),
                        StructuredError::InvalidName { name, allowed } => {
                            ParsingError::InvalidName(name, allowed)
                        }
                        StructuredError::EmptyName => ParsingError::EmptyName,
                    });
                }

                // Standard serde error: prepend path if non-empty.
                let message = if path.is_empty() || path == "." {
                    msg.clone()
                } else {
                    format!("{path}: {msg}")
                };
                Error::Parsing(ParsingError::CannotParseConfig(message))
            }
        }
    })
}

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
    #[error("Empty name")]
    EmptyName,
    #[error("Duplicate name: {0}")]
    DuplicateName(String),

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

    // -- build system
    #[error("Invalid toolchain {0}")]
    InvalidToolchain(String),

    // -- node config: process vs container
    #[error("Node config must have exactly one of `process` or `container`, not both")]
    ProcessAndContainerConflict,
    #[error("Node config must have either `process` or `container`")]
    NoProcessOrContainer,
    #[error("Node config `execution.start_cmd` must not be empty")]
    EmptyStartCmd,

    // -- node config: default variant
    #[error(
        "Node config with a 'default' variant must not define an `execution` section — the execution comes from the default variant"
    )]
    ExecutionWithDefaultVariant,
    #[error("Node config must define an `execution` section (or declare a 'default' variant)")]
    MissingExecution,
    #[error("Node config `execution.language` is required when an execution block is defined")]
    MissingExecutionLanguage,

    // -- container config: mount paths
    #[error(
        "Invalid mount path `{0}`: top-level system directories ({1}) cannot be used as mount sources — use a subdirectory instead (e.g., /tmp/my_app)"
    )]
    InvalidMountPath(String, String),
    #[error("Invalid parameter reference `${{parameters:{0}}}` in mount path: {1}")]
    InvalidMountPathParameterRef(String, String),

    #[error("{0}")]
    Structured(String),
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub enum StructuredError {
    InvalidDeploymentSource(String),
    DuplicateName(String),
    InvalidName { name: String, allowed: String },
    EmptyName,
}

impl ParsingError {
    /// Returns `true` when the error indicates that the `manifest` field is
    /// absent from the config.  This is the hallmark of a **variant** config
    /// (which deliberately omits `manifest`) and is used by the CLI to decide
    /// whether to walk up the directory tree to locate the root node config.
    pub fn is_missing_manifest(&self) -> bool {
        matches!(self, ParsingError::CannotParseConfig(msg) if msg.contains("missing field `manifest`"))
    }
}

impl StructuredError {
    pub(crate) fn json5_message(&self) -> String {
        serde_json5::to_string(self).unwrap_or_else(|_| "serialization error".to_string())
    }
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
                        StructuredError::DuplicateName(id) => ParsingError::DuplicateName(id),
                        StructuredError::InvalidName { name, allowed } => {
                            ParsingError::InvalidName(name, allowed)
                        }
                        StructuredError::EmptyName => ParsingError::EmptyName,
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

    // -- Askama
    #[error("Askama error: {0}")]
    AskamaError(String),
    #[error("Encoding error: {0}")]
    Encoding(String),

    // -- Fingerprint
    #[error(
        "Node config fingerprint mismatch: expected {expected}, got {actual}. The config may have been modified after code generation. Run `node sync` to update the peppygen lib on your node."
    )]
    FingerprintMismatch { expected: String, actual: String },
    #[error(
        "Release fingerprint mismatch: node was generated with peppy version {node_version}, but current peppy version is {current_version}. Run `node sync` to regenerate with the current version."
    )]
    ReleaseFingerprintMismatch {
        node_version: String,
        current_version: String,
    },
    #[error("Release fingerprint missing: {0}")]
    ReleaseFingerprintMissing(String),
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

        // DuplicateName
        let json =
            serde_json5::to_string(&StructuredError::DuplicateName("id1".to_string())).unwrap();
        let err = ParsingError::from(make_err(&json));
        if let ParsingError::DuplicateName(id) = err {
            assert_eq!(id, "id1");
        } else {
            panic!("Expected DuplicateName, got {:?}", err);
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

        // EmptyName
        let json = serde_json5::to_string(&StructuredError::EmptyName).unwrap();
        let err = ParsingError::from(make_err(&json));
        if !matches!(err, ParsingError::EmptyName) {
            panic!("Expected EmptyName, got {:?}", err);
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
