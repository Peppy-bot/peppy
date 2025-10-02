use super::types::PeppyConfig;
use crate::error::{ParsingError, Result};
use std::fs;
use std::path::Path;

/// Parser responsible for extracting `peppy_config.json5` documents
pub struct PeppyConfigParser;

const PEPPY_CONFIG_FILE_NAME: &str = "peppy_config.json5";

impl PeppyConfigParser {
    pub fn from_path(file: impl AsRef<Path>) -> Result<PeppyConfig> {
        let path = file.as_ref();
        let file_name = path.file_name().and_then(|name| name.to_str());
        if file_name != Some(PEPPY_CONFIG_FILE_NAME) {
            let found = file_name
                .map(str::to_owned)
                .unwrap_or_else(|| path.display().to_string());
            return Err(ParsingError::InvalidFileName {
                expected: PEPPY_CONFIG_FILE_NAME.to_string(),
                found,
            }
            .into());
        }
        let content = fs::read_to_string(path)
            .map_err(|_| ParsingError::CannotRead(path.display().to_string()))?;

        if content.trim().is_empty() {
            Err(ParsingError::EmptyContent(path.display().to_string()).into())
        } else {
            Self::from_content(&content)
        }
    }

    /// Takes a JSON5 content as parameter
    pub fn from_content(content: &str) -> Result<PeppyConfig> {
        // Strict schema validation is handled by serde via #[serde(deny_unknown_fields)]
        serde_json5::from_str::<PeppyConfig>(content).map_err(|e| ParsingError::from(e).into())
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        config::DeploymentNodeSource,
        error::{Error, ParsingError},
        node::LogFormat,
    };
    use tempfile::tempdir;

    use super::*;

    #[test]
    fn test_parse_peppy_config() {
        let json5 = r#"{
            deployments: [
                {
                    name: "uvc_camera",
                    tag: "0.1.0",
                    source: "file://tmp/peppy.json5",
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
                    source: "https://github.com/Peppy/web_video_stream.git",
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
                    source: "https://github.com/Peppy/peppy_web.git",
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
            logging: { min_level: "info" }
        }"#;

        let cfg = PeppyConfigParser::from_content(json5).unwrap();
        assert!(cfg.deployments.is_some());
        let deployments = cfg.deployments.unwrap();
        assert_eq!(deployments.len(), 3);

        // Check first deployment
        assert_eq!(deployments[0].name, "uvc_camera");
        assert_eq!(deployments[0].tag, "0.1.0");
        assert!(matches!(
            deployments[0].source,
            Some(DeploymentNodeSource::Local(_))
        ));
        assert_eq!(deployments[0].instances.len(), 2);

        // Check second deployment
        assert_eq!(deployments[1].name, "web_video_stream");
        assert!(matches!(
            deployments[1].source,
            Some(DeploymentNodeSource::Git(_))
        ));
        assert_eq!(deployments[1].instances.len(), 1);

        // Check third deployment
        assert_eq!(deployments[2].name, "peppy_web");
        assert!(matches!(
            deployments[2].source,
            Some(DeploymentNodeSource::Git(_))
        ));
        assert_eq!(deployments[2].instances.len(), 1);

        let logging = cfg.logging.expect("expected logging section");
        assert_eq!(logging.min_level, "info");
        assert!(logging.file_name.is_none());
        assert!(logging.max_file_size_mb.is_none());
        assert_eq!(logging.format, LogFormat::Text);
    }

    #[test]
    fn test_from_path_rejects_wrong_file_name() {
        let dir = tempdir().unwrap();
        let wrong_path = dir.path().join("peppy.json5");
        std::fs::write(&wrong_path, "{}").unwrap();

        let err = PeppyConfigParser::from_path(&wrong_path).unwrap_err();
        assert!(matches!(
            err,
            Error::Parsing(ParsingError::InvalidFileName { ref expected, ref found })
                if expected == PEPPY_CONFIG_FILE_NAME && found == "peppy.json5"
        ));
    }

    #[test]
    fn test_from_path_accepts_correct_file_name() {
        let dir = tempdir().unwrap();
        let path = dir.path().join(PEPPY_CONFIG_FILE_NAME);
        let json5 = r#"{
            deployments: [
                {
                    name: "uvc_camera",
                    tag: "0.1.0",
                    instances: [ { namespace: "/" } ]
                }
            ]
        }"#;
        std::fs::write(&path, json5).unwrap();

        let cfg = PeppyConfigParser::from_path(&path).unwrap();
        assert!(cfg.deployments.is_some());
    }
}
