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
                tag: "0.1.0",
                language: "rust",
            },
        }"#;
        let config = NodeConfigParser::from_content(json5).unwrap();
        assert_eq!(config.manifest.name.as_str(), "test_node");
        assert_eq!(config.manifest.tag, "0.1.0");
        assert!(config.parameters.is_empty());
    }

    #[test]
    fn test_parse_complex_config() {
        let json5 = r#"{
            manifest: {
                name: "camera_driver",
                tag: "2.1.0",
                language: "rust"
            },
            config: {
                auto_start: true,
                respawn: true,
                respawn_delay: 2.0
            },
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
        assert_eq!(config.manifest.tag, "2.1.0");
        assert_eq!(config.config.auto_start, Some(true));
        assert_eq!(config.config.respawn, Some(true));
        assert_eq!(config.config.respawn_delay, Some(2.0));
        assert!(config.interfaces.exposes.is_some());
    }

    #[test]
    fn test_parse_root_config() {
        let json5 = r#"{
            manifest: {
                name: "my_robot_1",
                tag: "0.1.0",
                language: "rust"
            },
            config: {
                auto_start: true,
                respawn: true,
                respawn_delay: 1.0
            },
            deployments: [
                {
                    name: "uvc_camera",
                    tag: "0.1.0",
                    instances: [
                        {
                            namespace: "/camera/front",
                            parameters: {
                                device: { physical: "/dev/video_front", sim: "mujoco:camera_front", priority: "physical" },
                                video: {
                                    frame_rate: 30,
                                    resolution: { width: 1280, height: 720 },
                                    encoding: "mjpeg"
                                }
                            }
                        },
                        {
                            namespace: "/camera/rear",
                            parameters: {
                                device: { physical: "/dev/video_rear", sim: "mujoco:camera_rear", priority: "sim" },
                                video: {
                                    frame_rate: 30,
                                    resolution: { width: 1280, height: 720 },
                                    encoding: "mjpeg"
                                }
                            }
                        }
                    ]
                },
                {
                    name: "web_video_stream",
                    tag: "0.1.0",
                    instances: [
                        {
                            namespace: "/video",
                            parameters: {
                                http: { host: "localhost", port: 8081, max_connections: 1000, request_timeout_ms: 5000 },
                                video_stream: { format: "mjpeg", quality: 75, max_fps: 30 }
                            }
                        }
                    ]
                },
                {
                    name: "peppy_web",
                    tag: "0.1.0",
                    instances: [
                        {
                            namespace: "/",
                            parameters: {
                                http: { host: "0.0.0.0", port: 8080, max_connections: 500, request_timeout_ms: 5000 }
                            }
                        }
                    ]
                }
            ],
            interfaces: {
                subscribes_to: {
                    topics: [
                        {
                            node: "{any}",
                            tag: "{any}",
                            name: "peppy_node_status",
                            namespace: "/",
                            callback: "on_root_node_discovered"
                        }
                    ]
                }
            },
            resources: { max_memory_mb: 1024 },
            logging: { min_level: "info" }
        }"#;

        let cfg = NodeConfigParser::from_content(json5).unwrap();
        assert_eq!(cfg.manifest.name.as_str(), "my_robot_1");
        assert_eq!(cfg.manifest.tag, "0.1.0");
        assert_eq!(cfg.config.auto_start, Some(true));
        assert!(cfg.deployments.is_some());
        let deployments = cfg.deployments.unwrap();
        assert_eq!(deployments.len(), 3);

        // Check first deployment
        assert_eq!(deployments[0].name, "uvc_camera");
        assert_eq!(deployments[0].tag, "0.1.0");
        assert_eq!(deployments[0].instances.len(), 2);

        // Check second deployment
        assert_eq!(deployments[1].name, "web_video_stream");
        assert_eq!(deployments[1].instances.len(), 1);

        // Check third deployment
        assert_eq!(deployments[2].name, "peppy_web");
        assert_eq!(deployments[2].instances.len(), 1);
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
