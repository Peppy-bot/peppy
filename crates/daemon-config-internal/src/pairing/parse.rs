use super::types::PeppyPairing;
use crate::{error::Result, parsing::read_non_empty_file};
use std::path::Path;

/// Parser responsible for extracting pairing documents.
///
/// Pairing files are stand-alone JSON5 documents declaring
/// `peppy_schema: "pairing/v1"`. Like contracts and launchers, they are
/// filename-agnostic: schema and shape validation are handled by serde so
/// callers walking a repository can attempt to parse and treat failures as
/// "not a pairing."
pub struct PeppyPairingParser;

impl PeppyPairingParser {
    pub fn from_path(file: impl AsRef<Path>) -> Result<PeppyPairing> {
        let path = file.as_ref();
        let content = read_non_empty_file(path)?;
        Self::from_content(&content)
    }

    /// Takes a JSON5 content string and parses it as a pairing document.
    pub fn from_content(content: &str) -> Result<PeppyPairing> {
        crate::error::deserialize_json5_with_path(content)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::{Error, ParsingError};
    use config::schema::PeppySchema;
    use tempfile::NamedTempFile;

    #[test]
    fn from_content_parses_pairing() {
        let json5 = r#"{
            peppy_schema: "pairing/v1",
            manifest: { name: "arm_link", tag: "v1" },
            roles: ["controller", "arm"],
            topics: [
                { emitted_by: "controller", name: "joint_commands" },
                { emitted_by: "arm", name: "joint_states" }
            ]
        }"#;
        let parsed = PeppyPairingParser::from_content(json5).expect("should parse");
        assert_eq!(parsed.peppy_schema, PeppySchema::PairingV1);
        assert_eq!(parsed.manifest.name.as_str(), "arm_link");
        assert_eq!(parsed.topics.len(), 2);
    }

    #[test]
    fn from_path_loads_file() {
        let tmp = NamedTempFile::new().unwrap();
        let json5 = r#"{
            peppy_schema: "pairing/v1",
            manifest: { name: "ping_link", tag: "v1" },
            roles: ["a", "b"],
            topics: [{ emitted_by: "a", name: "ping" }]
        }"#;
        std::fs::write(tmp.path(), json5).unwrap();
        let parsed = PeppyPairingParser::from_path(tmp.path()).expect("should parse");
        assert_eq!(parsed.manifest.name.as_str(), "ping_link");
    }

    #[test]
    fn empty_file_rejected() {
        let tmp = NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), b"").unwrap();
        let result = PeppyPairingParser::from_path(tmp.path());
        assert!(matches!(
            result.unwrap_err(),
            Error::Parsing(ParsingError::EmptyContent(_))
        ));
    }

    #[test]
    fn missing_file_rejected() {
        let result = PeppyPairingParser::from_path("/path/does/not/exist.json5");
        assert!(matches!(
            result.unwrap_err(),
            Error::Parsing(ParsingError::CannotRead(..))
        ));
    }

    #[test]
    fn malformed_json5_rejected() {
        let result = PeppyPairingParser::from_content("{ manifest: [unclosed");
        assert!(matches!(
            result.unwrap_err(),
            Error::Parsing(ParsingError::CannotParseConfig(_))
        ));
    }

    /// A contract document must not be misread as a pairing. The schema
    /// field is the source of truth.
    #[test]
    fn contract_document_rejected() {
        let json5 = r#"{
            peppy_schema: "contract/v1",
            manifest: { name: "x", tag: "v1" },
            interfaces: {}
        }"#;
        let err = PeppyPairingParser::from_content(json5)
            .expect_err("contract must not parse as pairing");
        assert!(
            err.to_string().contains("pairing/v1"),
            "error should mention expected schema, got: {err}"
        );
    }
}
