use super::types::PeppyInterface;
use crate::{error::Result, parsing::read_non_empty_file};
use std::path::Path;

/// Parser responsible for extracting interface documents.
///
/// Interface files are stand-alone JSON5 documents declaring
/// `peppy_schema: "interface/v1"`. Like launchers, they are filename-agnostic
/// — schema and shape validation are handled by serde so callers walking a
/// repository can attempt to parse and treat failures as "not an interface."
pub struct PeppyInterfaceParser;

impl PeppyInterfaceParser {
    pub fn from_path(file: impl AsRef<Path>) -> Result<PeppyInterface> {
        let path = file.as_ref();
        let content = read_non_empty_file(path)?;
        Self::from_content(&content)
    }

    /// Takes a JSON5 content string and parses it as an interface document.
    pub fn from_content(content: &str) -> Result<PeppyInterface> {
        crate::error::deserialize_json5_with_path(content)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        error::{Error, ParsingError},
        schema::PeppySchema,
    };
    use tempfile::NamedTempFile;

    #[test]
    fn from_content_parses_interface() {
        let json5 = r#"{
            peppy_schema: "interface/v1",
            manifest: { name: "depth_camera", tag: "v1" },
            interfaces: {
                topics: [
                    { name: "video_stream", qos_profile: "sensor_data" }
                ]
            }
        }"#;
        let parsed = PeppyInterfaceParser::from_content(json5).expect("should parse");
        assert_eq!(parsed.peppy_schema, PeppySchema::InterfaceV1);
        assert_eq!(parsed.manifest.name.as_str(), "depth_camera");
        assert_eq!(parsed.interfaces.topics.len(), 1);
    }

    #[test]
    fn from_path_loads_file() {
        let tmp = NamedTempFile::new().unwrap();
        let json5 = r#"{
            peppy_schema: "interface/v1",
            manifest: { name: "ping", tag: "v1" },
            interfaces: {}
        }"#;
        std::fs::write(tmp.path(), json5).unwrap();
        let parsed = PeppyInterfaceParser::from_path(tmp.path()).expect("should parse");
        assert_eq!(parsed.manifest.name.as_str(), "ping");
    }

    #[test]
    fn empty_file_rejected() {
        let tmp = NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), b"").unwrap();
        let result = PeppyInterfaceParser::from_path(tmp.path());
        assert!(matches!(
            result.unwrap_err(),
            Error::Parsing(ParsingError::EmptyContent(_))
        ));
    }

    #[test]
    fn missing_file_rejected() {
        let result = PeppyInterfaceParser::from_path("/path/does/not/exist.json5");
        assert!(matches!(
            result.unwrap_err(),
            Error::Parsing(ParsingError::CannotRead(..))
        ));
    }

    #[test]
    fn malformed_json5_rejected() {
        let result = PeppyInterfaceParser::from_content("{ manifest: [unclosed");
        assert!(matches!(
            result.unwrap_err(),
            Error::Parsing(ParsingError::CannotParseConfig(_))
        ));
    }

    /// A launcher document must not be misread as an interface, even though
    /// both lack a node's `execution` block. The schema field is the source
    /// of truth.
    #[test]
    fn launcher_document_rejected() {
        let json5 = r#"{
            peppy_schema: "launcher/v1",
            deployments: []
        }"#;
        let err = PeppyInterfaceParser::from_content(json5)
            .expect_err("launcher must not parse as interface");
        assert!(
            err.to_string().contains("interface/v1"),
            "error should mention expected schema, got: {err}"
        );
    }
}
