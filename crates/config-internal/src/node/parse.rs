use super::types::{NodeConfig, Runtime, VariantConfig};
use crate::{
    error::{ParsingError, Result},
    parsing::read_non_empty_file,
};
use std::path::Path;

/// Validates runtime constraints shared by both full node configs and variant configs.
fn validate_runtime(runtime: &Runtime) -> Result<()> {
    // `start_cmd` and `container` are mutually exclusive; exactly one must be present.
    match (&runtime.start_cmd, &runtime.container) {
        (Some(_), Some(_)) => return Err(ParsingError::ProcessAndContainerConflict.into()),
        (None, None) => return Err(ParsingError::NoProcessOrContainer.into()),
        _ => {}
    }

    // Validate container mount paths (reject top-level system directories).
    if let Some(container) = &runtime.container
        && let Err((path, blocked_list)) = container.validate()
    {
        return Err(ParsingError::InvalidMountPath(path, blocked_list).into());
    }

    // Validate ${parameters:...} references in mount paths.
    if let Some(container) = &runtime.container
        && let Err((ref_path, reason)) = container.validate_parameter_refs(&runtime.parameters)
    {
        return Err(ParsingError::InvalidMountPathParameterRef(ref_path, reason).into());
    }

    Ok(())
}

/// Parser responsible for extracting configuration from JSON5 documents
pub struct NodeConfigParser;

impl NodeConfigParser {
    pub fn from_path(file: impl AsRef<Path>) -> Result<NodeConfig> {
        let path = file.as_ref();
        let content = read_non_empty_file(path)?;
        Self::from_content(&content)
    }

    /// Takes a JSON5 content as parameter
    pub fn from_content(content: &str) -> Result<NodeConfig> {
        // Strict schema validation is handled by serde via #[serde(deny_unknown_fields)]
        let config: NodeConfig = serde_json5::from_str(content).map_err(ParsingError::from)?;
        validate_runtime(&config.runtime)?;
        Ok(config)
    }
}

/// Parser for variant node configs where `manifest` and `interfaces` are optional.
pub struct VariantConfigParser;

impl VariantConfigParser {
    pub fn from_path(file: impl AsRef<Path>) -> Result<VariantConfig> {
        let path = file.as_ref();
        let content = read_non_empty_file(path)?;
        Self::from_content(&content)
    }

    pub fn from_content(content: &str) -> Result<VariantConfig> {
        let config: VariantConfig = serde_json5::from_str(content).map_err(ParsingError::from)?;
        validate_runtime(&config.runtime)?;
        Ok(config)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{config::PeppyLauncherParser, error::Error};
    use tempfile::NamedTempFile;

    #[test]
    fn test_parse_minimal_config() {
        let json5 = r#"{
            schema_version: 1,
            manifest: {
                name: "test_node",
                tag: "0.1.0",
            },
            runtime: {
                language: "rust",
                start_cmd: ["./target/release/test_node"],
            },
        }"#;
        let config = NodeConfigParser::from_content(json5).unwrap();
        assert_eq!(config.manifest.name.as_str(), "test_node");
        assert_eq!(config.manifest.tag, "0.1.0");
        assert_eq!(
            config.runtime.start_cmd.as_ref().unwrap(),
            &vec!["./target/release/test_node"]
        );
        assert!(config.runtime.parameters.is_empty());
    }

    #[test]
    fn test_parse_complex_config() {
        let json5 = r#"{
            schema_version: 1,
            manifest: {
                name: "camera_driver",
                tag: "2.1.0",
            },
            interfaces: {
                topics: {
                    emits: [
                        { name: "/camera/image_raw" }
                    ]
                }
            },
            runtime: {
                language: "rust",
                start_cmd: ["./target/release/camera_driver"],
            },
        }"#;
        let config = NodeConfigParser::from_content(json5).unwrap();
        assert_eq!(config.manifest.name.as_str(), "camera_driver");
        assert_eq!(config.manifest.tag, "2.1.0");
        assert_eq!(config.runtime.language, crate::node::PeppygenLanguage::Rust);
        assert_eq!(
            config.runtime.start_cmd.as_ref().unwrap(),
            &vec!["./target/release/camera_driver"]
        );
        assert!(config.interfaces.topics.is_some());
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

    #[test]
    fn test_parse_container_config() {
        let json5 = r#"{
            schema_version: 1,
            manifest: {
                name: "container_node",
                tag: "0.1.0",
            },
            runtime: {
                language: "rust",
                container: {
                    def_file: "apptainer.def",
                },
            },
        }"#;
        let config = NodeConfigParser::from_content(json5).unwrap();
        assert!(config.runtime.start_cmd.is_none());
        let container = config.runtime.container.as_ref().unwrap();
        assert_eq!(container.def_file, "apptainer.def");
    }

    #[test]
    fn test_process_and_container_conflict() {
        let json5 = r#"{
            schema_version: 1,
            manifest: {
                name: "bad_node",
                tag: "0.1.0",
            },
            runtime: {
                language: "rust",
                start_cmd: ["./bin"],
                container: {
                    def_file: "apptainer.def",
                },
            },
        }"#;
        let result = NodeConfigParser::from_content(json5);
        assert!(matches!(
            result.unwrap_err(),
            Error::Parsing(ParsingError::ProcessAndContainerConflict)
        ));
    }

    #[test]
    fn test_no_process_or_container() {
        let json5 = r#"{
            schema_version: 1,
            manifest: {
                name: "bare_node",
                tag: "0.1.0",
            },
            runtime: {
                language: "rust",
            },
        }"#;
        let result = NodeConfigParser::from_content(json5);
        assert!(matches!(
            result.unwrap_err(),
            Error::Parsing(ParsingError::NoProcessOrContainer)
        ));
    }

    #[test]
    fn test_invalid_deployment_source() {
        let json5 = r#"{
            deployments: [
                {
                    source: { local: "" },
                    instances: []
                }
            ]
        }"#;

        let result = PeppyLauncherParser::from_content(json5);
        let Error::Parsing(ParsingError::InvalidDeploymentSource(msg)) = result.unwrap_err() else {
            panic!("expected InvalidDeploymentSource error");
        };
        assert_eq!(msg, "local path cannot be empty");
    }

    #[test]
    fn test_container_config_rejects_system_path_mount() {
        let json5 = r#"{
            schema_version: 1,
            manifest: {
                name: "bad_mount_node",
                tag: "0.1.0",
            },
            runtime: {
                language: "rust",
                container: {
                    def_file: "apptainer.def",
                    mount_paths: ["/tmp:/tmp:rw"],
                },
            },
        }"#;
        let result = NodeConfigParser::from_content(json5);
        assert!(
            matches!(
                result.as_ref().unwrap_err(),
                Error::Parsing(ParsingError::InvalidMountPath(_, _))
            ),
            "expected InvalidMountPath error, got: {:?}",
            result.unwrap_err()
        );
    }

    #[test]
    fn test_container_config_rejects_private_system_path_mount() {
        let json5 = r#"{
            schema_version: 1,
            manifest: {
                name: "bad_mount_node",
                tag: "0.1.0",
            },
            runtime: {
                language: "rust",
                container: {
                    def_file: "apptainer.def",
                    mount_paths: ["/private/tmp:/tmp:rw"],
                },
            },
        }"#;
        let result = NodeConfigParser::from_content(json5);
        assert!(
            matches!(
                result.as_ref().unwrap_err(),
                Error::Parsing(ParsingError::InvalidMountPath(_, _))
            ),
            "expected InvalidMountPath error for /private/tmp, got: {:?}",
            result.unwrap_err()
        );
    }

    #[test]
    fn test_container_config_accepts_subdirectory_mount() {
        let json5 = r#"{
            schema_version: 1,
            manifest: {
                name: "good_mount_node",
                tag: "0.1.0",
            },
            runtime: {
                language: "rust",
                container: {
                    def_file: "apptainer.def",
                    mount_paths: ["/tmp/my_app_data:/tmp/my_app_data:rw"],
                },
            },
        }"#;
        let config =
            NodeConfigParser::from_content(json5).expect("subdirectory mount should be accepted");
        let mount_paths = config.runtime.container.unwrap().mount_paths.unwrap();
        assert_eq!(mount_paths, vec!["/tmp/my_app_data:/tmp/my_app_data:rw"]);
    }

    #[test]
    fn test_container_config_accepts_no_mount_paths() {
        let json5 = r#"{
            schema_version: 1,
            manifest: {
                name: "no_mount_node",
                tag: "0.1.0",
            },
            runtime: {
                language: "rust",
                container: {
                    def_file: "apptainer.def",
                },
            },
        }"#;
        let config = NodeConfigParser::from_content(json5).expect("no mount_paths should be valid");
        assert!(config.runtime.container.unwrap().mount_paths.is_none());
    }

    #[test]
    fn test_container_mount_path_with_parameter_ref() {
        let json5 = r#"{
            schema_version: 1,
            manifest: {
                name: "camera_node",
                tag: "0.1.0",
            },
            runtime: {
                language: "rust",
                parameters: {
                    device_path: "string",
                },
                container: {
                    def_file: "apptainer.def",
                    mount_paths: ["${parameters:device_path}:/dev/video0:rw"],
                },
            },
        }"#;
        let config = NodeConfigParser::from_content(json5)
            .expect("parameter ref in mount path should parse");
        let mount_paths = config.runtime.container.unwrap().mount_paths.unwrap();
        assert_eq!(
            mount_paths,
            vec!["${parameters:device_path}:/dev/video0:rw"]
        );
    }

    #[test]
    fn test_container_mount_path_with_nested_parameter_ref() {
        let json5 = r#"{
            schema_version: 1,
            manifest: {
                name: "camera_node",
                tag: "0.1.0",
            },
            runtime: {
                language: "rust",
                parameters: {
                    video: {
                        device_path: "string",
                        frame_rate: "u16",
                    },
                },
                container: {
                    def_file: "apptainer.def",
                    mount_paths: ["${parameters:video.device_path}:/dev/video0:rw"],
                },
            },
        }"#;
        let config = NodeConfigParser::from_content(json5)
            .expect("nested parameter ref in mount path should parse");
        let mount_paths = config.runtime.container.unwrap().mount_paths.unwrap();
        assert_eq!(
            mount_paths,
            vec!["${parameters:video.device_path}:/dev/video0:rw"]
        );
    }

    #[test]
    fn test_container_mount_path_rejects_unknown_parameter_ref() {
        let json5 = r#"{
            schema_version: 1,
            manifest: {
                name: "bad_ref_node",
                tag: "0.1.0",
            },
            runtime: {
                language: "rust",
                parameters: {
                    device_path: "string",
                },
                container: {
                    def_file: "apptainer.def",
                    mount_paths: ["${parameters:nonexistent}:/data:rw"],
                },
            },
        }"#;
        let result = NodeConfigParser::from_content(json5);
        assert!(
            matches!(
                result.as_ref().unwrap_err(),
                Error::Parsing(ParsingError::InvalidMountPathParameterRef(ref_path, _))
                    if ref_path == "nonexistent"
            ),
            "expected InvalidMountPathParameterRef error, got: {:?}",
            result.unwrap_err()
        );
    }

    #[test]
    fn test_container_mount_path_rejects_non_string_parameter_ref() {
        let json5 = r#"{
            schema_version: 1,
            manifest: {
                name: "bad_type_node",
                tag: "0.1.0",
            },
            runtime: {
                language: "rust",
                parameters: {
                    frame_rate: "u16",
                },
                container: {
                    def_file: "apptainer.def",
                    mount_paths: ["${parameters:frame_rate}:/data:rw"],
                },
            },
        }"#;
        let result = NodeConfigParser::from_content(json5);
        assert!(
            matches!(
                result.as_ref().unwrap_err(),
                Error::Parsing(ParsingError::InvalidMountPathParameterRef(ref_path, reason))
                    if ref_path == "frame_rate" && reason.contains("string")
            ),
            "expected InvalidMountPathParameterRef error about string type, got: {:?}",
            result.unwrap_err()
        );
    }

    #[test]
    fn test_container_mount_path_skips_blocked_check_for_parameter_ref() {
        // A mount path whose source is a parameter reference should NOT be rejected
        // at parse time, even though the resolved value might be a blocked path.
        let json5 = r#"{
            schema_version: 1,
            manifest: {
                name: "dynamic_mount_node",
                tag: "0.1.0",
            },
            runtime: {
                language: "rust",
                parameters: {
                    path: "string",
                },
                container: {
                    def_file: "apptainer.def",
                    mount_paths: ["${parameters:path}:/container/data:rw"],
                },
            },
        }"#;
        let config = NodeConfigParser::from_content(json5)
            .expect("parameter ref source should skip blocked-path check at parse time");
        let mount_paths = config.runtime.container.unwrap().mount_paths.unwrap();
        assert_eq!(mount_paths, vec!["${parameters:path}:/container/data:rw"]);
    }

    #[test]
    fn test_container_config_extra_args_default_to_none() {
        let json5 = r#"{
            schema_version: 1,
            manifest: {
                name: "container_node",
                tag: "0.1.0",
            },
            runtime: {
                language: "rust",
                container: {
                    def_file: "apptainer.def",
                },
            },
        }"#;
        let config = NodeConfigParser::from_content(json5).unwrap();
        let container = config.runtime.container.as_ref().unwrap();
        assert!(container.apptainer_build_extra_args.is_none());
        assert!(container.apptainer_run_extra_args.is_none());
        assert!(container.lima_shell_extra_args.is_none());
    }

    #[test]
    fn test_container_config_parses_extra_args() {
        let json5 = r#"{
            schema_version: 1,
            manifest: {
                name: "container_node",
                tag: "0.1.0",
            },
            runtime: {
                language: "rust",
                container: {
                    def_file: "apptainer.def",
                    apptainer_build_extra_args: ["--no-setgroups", "--force"],
                    apptainer_run_extra_args: ["--no-setgroups"],
                    lima_shell_extra_args: ["--timeout", "30"],
                },
            },
        }"#;
        let config = NodeConfigParser::from_content(json5).unwrap();
        let container = config.runtime.container.as_ref().unwrap();
        assert_eq!(
            container.apptainer_build_extra_args.as_deref().unwrap(),
            &["--no-setgroups", "--force"]
        );
        assert_eq!(
            container.apptainer_run_extra_args.as_deref().unwrap(),
            &["--no-setgroups"]
        );
        assert_eq!(
            container.lima_shell_extra_args.as_deref().unwrap(),
            &["--timeout", "30"]
        );
    }

    #[test]
    fn test_container_config_parses_empty_extra_args() {
        let json5 = r#"{
            schema_version: 1,
            manifest: {
                name: "container_node",
                tag: "0.1.0",
            },
            runtime: {
                language: "rust",
                container: {
                    def_file: "apptainer.def",
                    apptainer_build_extra_args: [],
                    apptainer_run_extra_args: [],
                    lima_shell_extra_args: [],
                },
            },
        }"#;
        let config = NodeConfigParser::from_content(json5).unwrap();
        let container = config.runtime.container.as_ref().unwrap();
        assert_eq!(
            container.apptainer_build_extra_args.as_deref().unwrap(),
            &[] as &[String]
        );
        assert_eq!(
            container.apptainer_run_extra_args.as_deref().unwrap(),
            &[] as &[String]
        );
        assert_eq!(
            container.lima_shell_extra_args.as_deref().unwrap(),
            &[] as &[String]
        );
    }
}
