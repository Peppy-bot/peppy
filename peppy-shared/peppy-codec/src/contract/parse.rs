use super::comments::ContractComments;
use super::types::PeppyContract;
use std::path::Path;

/// A text is not a contract document.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ContractError {
    /// The file of the document does not read.
    #[error("Cannot read {path}: {kind}")]
    CannotRead {
        path: String,
        kind: std::io::ErrorKind,
    },
    /// The file of the document holds nothing but white space.
    #[error("Empty content found in: {0}")]
    EmptyContent(String),
    /// The text is not JSON5, or not a contract: the message names the key
    /// that the parser refused, by its path in the document.
    #[error("Cannot parse contract: {0}")]
    Invalid(String),
}

/// No contract raises a structured refusal: each refusal is a plain message
/// that names its key. This type stands for that in the shared JSON5
/// deserializer, which no value of it ever reaches.
#[derive(serde::Deserialize)]
enum NoStructuredRefusal {}

/// Parser responsible for extracting contract documents.
///
/// Contract files are stand-alone JSON5 documents declaring
/// `peppy_schema: "contract/v1"`. Like launchers, they are filename-agnostic:
/// schema and shape validation are handled by serde so callers walking a
/// repository can attempt to parse and treat failures as "not a contract."
pub struct PeppyContractParser;

impl PeppyContractParser {
    pub fn from_path(file: impl AsRef<Path>) -> Result<PeppyContract, ContractError> {
        let path = file.as_ref();
        let content = std::fs::read_to_string(path).map_err(|error| ContractError::CannotRead {
            path: path.display().to_string(),
            kind: error.kind(),
        })?;
        if content.trim().is_empty() {
            return Err(ContractError::EmptyContent(path.display().to_string()));
        }
        Self::from_content(&content)
    }

    /// Takes a JSON5 content string and parses it as a contract document,
    /// with the comments of the text.
    pub fn from_content(content: &str) -> Result<PeppyContract, ContractError> {
        let mut contract: PeppyContract = config::deserialize_json5_with_structured_errors(
            content,
            |error| ContractError::Invalid(error.to_string()),
            |refusal: NoStructuredRefusal| match refusal {},
            ContractError::Invalid,
        )?;
        contract.comments = ContractComments::of_document(content);
        Ok(contract)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use config::schema::PeppySchema;
    use tempfile::NamedTempFile;

    #[test]
    fn from_content_parses_contract() {
        let json5 = r#"{
            peppy_schema: "contract/v1",
            manifest: { name: "depth_camera", tag: "v1" },
            interfaces: {
                topics: [
                    { name: "video_stream", qos_profile: "sensor_data" }
                ]
            }
        }"#;
        let parsed = PeppyContractParser::from_content(json5).expect("should parse");
        assert_eq!(parsed.peppy_schema, PeppySchema::ContractV1);
        assert_eq!(parsed.manifest.name.as_str(), "depth_camera");
        assert_eq!(parsed.interfaces.topics.len(), 1);
    }

    #[test]
    fn from_path_loads_file() {
        let tmp = NamedTempFile::new().unwrap();
        let json5 = r#"{
            peppy_schema: "contract/v1",
            manifest: { name: "ping", tag: "v1" },
            interfaces: {}
        }"#;
        std::fs::write(tmp.path(), json5).unwrap();
        let parsed = PeppyContractParser::from_path(tmp.path()).expect("should parse");
        assert_eq!(parsed.manifest.name.as_str(), "ping");
    }

    #[test]
    fn empty_file_rejected() {
        let tmp = NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), b"").unwrap();
        let result = PeppyContractParser::from_path(tmp.path());
        assert!(matches!(
            result.unwrap_err(),
            ContractError::EmptyContent(_)
        ));
    }

    #[test]
    fn missing_file_rejected() {
        let result = PeppyContractParser::from_path("/path/does/not/exist.json5");
        assert!(matches!(
            result.unwrap_err(),
            ContractError::CannotRead {
                kind: std::io::ErrorKind::NotFound,
                ..
            }
        ));
    }

    #[test]
    fn malformed_json5_rejected() {
        let result = PeppyContractParser::from_content("{ manifest: [unclosed");
        assert!(matches!(result.unwrap_err(), ContractError::Invalid(_)));
    }

    /// A refused key is named by its path in the document.
    #[test]
    fn a_refusal_names_the_path_of_its_key() {
        let error = PeppyContractParser::from_content(
            r#"{
                peppy_schema: "contract/v1",
                manifest: { name: "arm", tag: "v1" },
                interfaces: { services: [{ name: "move", request_message_format: { x: "f65" } }] },
            }"#,
        )
        .expect_err("`f65` is no type");
        let ContractError::Invalid(message) = error else {
            panic!("a refusal of a key is `Invalid`, got {error:?}");
        };
        assert!(message.starts_with("interfaces.services"), "{message}");
    }

    /// A launcher document must not be misread as a contract, even though
    /// both lack a node's `execution` block. The schema field is the source
    /// of truth.
    #[test]
    fn launcher_document_rejected() {
        let json5 = r#"{
            peppy_schema: "launcher/v1",
            deployments: []
        }"#;
        let err = PeppyContractParser::from_content(json5)
            .expect_err("launcher must not parse as contract");
        assert!(
            err.to_string().contains("contract/v1"),
            "error should mention expected schema, got: {err}"
        );
    }
}
