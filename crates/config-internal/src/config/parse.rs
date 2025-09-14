use super::types::NodeConfig;
use crate::error::{ParsingError, Result};
use std::fs;
use std::path::Path;

/// Parser responsible for extracting configuration from JSON5 documents
pub struct NodeConfigParser;

impl NodeConfigParser {
    pub fn from_path(file: impl AsRef<Path>) -> Result<NodeConfig> {
        let path = file.as_ref();
        let content = fs::read_to_string(path)
            .map_err(|_| ParsingError::CannotRead(path.display().to_string()))?;

        if content.trim().is_empty() {
            Err(ParsingError::EmptyContent(path.display().to_string()).into())
        } else {
            Self::from_content(&content)
        }
    }

    /// Takes a JSON5 content as parameter
    pub fn from_content(content: &str) -> Result<NodeConfig> {
        // Strict schema validation is handled by serde via #[serde(deny_unknown_fields)]
        serde_json5::from_str::<NodeConfig>(content)
            .map_err(|e| ParsingError::CannotParseConfig(e.to_string()).into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::Error;
    use tempfile::NamedTempFile;

    #[test]
    fn test_parse_minimal_config() {
        let json5 = r#"{
            manifest: {
                name: "test_node",
                version: "0.1.0",
            },
            instances: [
                { namespace: "/test" }
            ]
        }"#;
        let config = NodeConfigParser::from_content(json5).unwrap();
        assert_eq!(config.manifest.name.as_str(), "test_node");
        assert_eq!(config.instances[0].namespace.as_str(), "/test");
        assert_eq!(config.manifest.version, "0.1.0"); // default
    }

    #[test]
    fn test_parse_complex_config() {
        let json5 = r#"{
            manifest: {
                name: "camera_driver",
                version: "2.1.0"
            },
            config: {
                auto_start: true,
                respawn: true,
                respawn_delay: 2.0
            },
            instances: [
                { namespace: "/sensors/camera" }
            ],
            interfaces: {
                exposes: {
                    topics: [
                        { name: "/camera/image_raw" }
                    ]
                }
            }
        }"#;
        let config = NodeConfigParser::from_content(json5).unwrap();
        assert_eq!(config.manifest.name.as_str(), "camera_driver");
        assert_eq!(config.instances[0].namespace.as_str(), "/sensors/camera");
        assert_eq!(config.manifest.version, "2.1.0");
        assert_eq!(config.config.auto_start, Some(true));
        assert_eq!(config.config.respawn, Some(true));
        assert_eq!(config.config.respawn_delay, Some(2.0));
        assert!(config.interfaces.exposes.is_some());
    }

    #[test]
    fn test_empty_file() {
        let tmp = NamedTempFile::new().unwrap();
        // Ensure file is empty
        std::fs::write(tmp.path(), b"").unwrap();
        let result = NodeConfigParser::from_path(tmp.path());
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            Error::Parsing(ParsingError::EmptyContent(_))
        ));
    }

    #[test]
    fn test_cannot_read_file() {
        let result = NodeConfigParser::from_path("/path/that/does/not/exist.json5");
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            Error::Parsing(ParsingError::CannotRead(_))
        ));
    }

    #[test]
    fn test_cannot_parse_json5() {
        let json5 = r#"{ manifest: [unclosed"#; // invalid JSON5
        let result = NodeConfigParser::from_content(json5);
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            Error::Parsing(ParsingError::CannotParseConfig(_))
        ));
    }
}
