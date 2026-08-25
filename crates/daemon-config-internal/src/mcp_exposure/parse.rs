use crate::{error::Result, parsing::read_non_empty_file};
use peppy_mcp_catalog::McpExposure;
use std::path::Path;

/// Parser responsible for extracting MCP exposure documents.
///
/// Exposure files are stand-alone JSON5 documents declaring
/// `peppy_schema: "mcp_exposure/v1"`. Like contracts, they are
/// filename-agnostic: schema and shape validation are handled by serde so
/// callers walking a repository can attempt to parse and treat failures as
/// "not an exposure."
pub struct PeppyMcpExposureParser;

impl PeppyMcpExposureParser {
    pub fn from_path(file: impl AsRef<Path>) -> Result<McpExposure> {
        let path = file.as_ref();
        let content = read_non_empty_file(path)?;
        Self::from_content(&content)
    }

    /// Takes a JSON5 content string and parses it as an exposure document.
    pub fn from_content(content: &str) -> Result<McpExposure> {
        crate::error::deserialize_json5_with_path(content)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::{Error, ParsingError};
    use config::schema::PeppySchema;
    use tempfile::NamedTempFile;

    const MINIMAL_EXPOSURE: &str = r#"{
        peppy_schema: "mcp_exposure/v1",
        manifest: { name: "camera_surface", tag: "v1" },
        server: { title: "Camera surface" },
        targets: {
            front_camera: {
                contract: {
                    name: "rgb_camera",
                    tag: "v1",
                    sha256: "9f2c9f2c9f2c9f2c9f2c9f2c9f2c9f2c9f2c9f2c9f2c9f2c9f2c9f2c9f2c9f2c",
                },
                services: [
                    {
                        member: "video_stream_info",
                        tool: "front_camera.info",
                        description: "Report the camera's stream parameters.",
                        operation: "read_only",
                        deadline_ms: 2000,
                    },
                ],
            },
        },
    }"#;

    #[test]
    fn from_content_parses_exposure() {
        let exposure = PeppyMcpExposureParser::from_content(MINIMAL_EXPOSURE).expect("parses");
        assert_eq!(exposure.peppy_schema, PeppySchema::McpExposureV1);
        assert_eq!(exposure.manifest.name.as_str(), "camera_surface");
        assert_eq!(exposure.targets.len(), 1);
    }

    #[test]
    fn from_path_loads_file() {
        let file = NamedTempFile::new().expect("temp file");
        std::fs::write(file.path(), MINIMAL_EXPOSURE).expect("write");
        let exposure = PeppyMcpExposureParser::from_path(file.path()).expect("parses");
        assert_eq!(exposure.manifest.tag, "v1");
    }

    #[test]
    fn empty_file_rejected() {
        let file = NamedTempFile::new().expect("temp file");
        let err = PeppyMcpExposureParser::from_path(file.path()).expect_err("empty rejected");
        assert!(matches!(
            err,
            Error::Parsing(ParsingError::EmptyContent { .. })
        ));
    }

    #[test]
    fn missing_file_rejected() {
        let err = PeppyMcpExposureParser::from_path("/nonexistent/exposure.json5")
            .expect_err("missing rejected");
        assert!(matches!(
            err,
            Error::Parsing(ParsingError::CannotRead { .. })
        ));
    }

    #[test]
    fn malformed_json5_rejected() {
        let err =
            PeppyMcpExposureParser::from_content("{ not valid").expect_err("malformed rejected");
        assert!(matches!(
            err,
            Error::Parsing(ParsingError::CannotParseConfig { .. })
        ));
    }

    #[test]
    fn contract_document_rejected() {
        let contract = r#"{
            peppy_schema: "contract/v1",
            manifest: { name: "rgb_camera", tag: "v1" },
            interfaces: {},
        }"#;
        let err = PeppyMcpExposureParser::from_content(contract).expect_err("wrong schema");
        assert!(err.to_string().contains("mcp_exposure/v1"));
    }
}
