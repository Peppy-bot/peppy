use super::types::{Execution, NodeConfig, ParsedNodeConfig, RawNodeConfig, VariantConfig};
use crate::{
    consts::NODE_CONFIG_FILE,
    error::{ParsingError, Result},
    parsing::read_non_empty_file,
};
use std::path::{Path, PathBuf};

/// Validates execution constraints shared by both full node configs and variant configs.
fn validate_execution(execution: &Execution) -> Result<()> {
    if let Some(cmds) = &execution.run_cmd
        && cmds.is_empty()
    {
        return Err(ParsingError::EmptyRunCmd.into());
    }

    // `run_cmd` and `container` are mutually exclusive; exactly one must be present.
    match (&execution.run_cmd, &execution.container) {
        (Some(_), Some(_)) => return Err(ParsingError::ProcessAndContainerConflict.into()),
        (None, None) => return Err(ParsingError::NoProcessOrContainer.into()),
        _ => {}
    }

    // Validate container mount paths (reject top-level system directories).
    if let Some(container) = &execution.container
        && let Err((path, blocked_list)) = container.validate()
    {
        return Err(ParsingError::InvalidMountPath(path, blocked_list).into());
    }

    // Validate ${parameters:...} references in mount paths.
    if let Some(container) = &execution.container
        && let Err((ref_path, reason)) = container.validate_parameter_refs(&execution.parameters)
    {
        return Err(ParsingError::InvalidMountPathParameterRef(ref_path, reason).into());
    }

    Ok(())
}

/// Parser responsible for extracting configuration from JSON5 documents
pub struct NodeConfigParser;

impl NodeConfigParser {
    pub fn from_path(file: impl AsRef<Path>) -> Result<ParsedNodeConfig> {
        let path = file.as_ref();
        let content = read_non_empty_file(path)?;
        Self::from_content(&content)
    }

    /// Takes a JSON5 content as parameter
    pub fn from_content(content: &str) -> Result<ParsedNodeConfig> {
        // Strict schema validation is handled by serde via #[serde(deny_unknown_fields)]
        let config: RawNodeConfig = crate::error::deserialize_json5_with_path(content)?;

        let has_default = config.has_default_variant();
        match (&config.execution, has_default) {
            (Some(_), true) => return Err(ParsingError::ExecutionWithDefaultVariant.into()),
            (None, true) => { /* execution comes from default variant */ }
            (Some(raw_exec), false) => {
                let execution = raw_exec.clone().into_execution()?;
                validate_execution(&execution)?;
            }
            (None, false) => return Err(ParsingError::MissingExecution.into()),
        }

        Ok(ParsedNodeConfig(config))
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
        let config: VariantConfig = crate::error::deserialize_json5_with_path(content)?;
        validate_execution(&config.execution)?;
        Ok(config)
    }
}

/// Walks up from `start_dir` looking for an ancestor directory containing a
/// valid root `peppy.json5` whose manifest declares at least one variant.
///
/// Returns `Ok(Some(path))` when found, `Ok(None)` when the filesystem root is
/// reached without finding a match, or `Err` when an ancestor config file
/// contains invalid syntax (other than the benign "missing field `manifest`"
/// that identifies another variant config).
///
/// Shared between:
/// - the CLI (`peppy node add .` invoked from inside a variant subdirectory)
/// - `peppylib` standalone mode (variant `peppy.json5` inherits `manifest`
///   and `interfaces` from the parent root — see [`load_standalone_node_config`]).
pub fn find_root_node_dir(start_dir: &Path) -> Result<Option<PathBuf>> {
    let mut dir = match start_dir.parent() {
        Some(d) => d,
        None => return Ok(None),
    };
    loop {
        let candidate = dir.join(NODE_CONFIG_FILE);
        if candidate.is_file() {
            match NodeConfigParser::from_path(&candidate) {
                Ok(cfg) if cfg.has_variants() => return Ok(Some(dir.to_path_buf())),
                Ok(_) => {} // Ancestor is a root but has no variants — keep walking.
                Err(crate::error::Error::Parsing(ref e)) if e.is_missing_manifest() => {}
                Err(other) => return Err(other),
            }
        }
        dir = match dir.parent() {
            Some(d) => d,
            None => return Ok(None),
        };
    }
}

/// Loads a `peppy.json5` at `path` and returns a fully resolved [`NodeConfig`],
/// transparently handling the variant case.
///
/// - If the file contains a `manifest` section, it is parsed as a full root
///   node config via [`NodeConfigParser`] and returned as-is.
/// - If the file is missing `manifest` (i.e. it's a **variant** config), it is
///   parsed via [`VariantConfigParser`] and the parent root config is located
///   via [`find_root_node_dir`]. The root's manifest + interfaces are merged
///   with the variant's execution via [`ParsedNodeConfig::merge_variant`].
///
/// This mirrors what the CLI does when resolving a variant source for
/// `peppy node add`, but without git/http handling — standalone mode always
/// operates on local files.
///
/// Used by `peppylib`'s standalone mode so that Rust and Python nodes can
/// `cargo run` / `python -m` from inside a variant directory without the CLI
/// having to pre-resolve the config.
pub fn load_standalone_node_config(path: impl AsRef<Path>) -> Result<NodeConfig> {
    let path = path.as_ref();
    let content = read_non_empty_file(path)?;

    match NodeConfigParser::from_content(&content) {
        Ok(parsed) => parsed.into_resolved(),
        Err(crate::error::Error::Parsing(ref e)) if e.is_missing_manifest() => {
            let variant = VariantConfigParser::from_content(&content)?;
            merge_variant_with_parent(path, variant)
        }
        Err(other) => Err(other),
    }
}

/// Locates the parent root config via [`find_root_node_dir`] and merges it
/// with the caller-provided variant config.
fn merge_variant_with_parent(
    variant_config_path: &Path,
    variant: VariantConfig,
) -> Result<NodeConfig> {
    // Canonicalize first so that `.parent()` reliably returns a non-empty
    // directory even when the caller passed a bare file name like
    // `peppy.json5` (standalone mode in peppylib does exactly this).
    let canonical = std::fs::canonicalize(variant_config_path).map_err(|e| {
        ParsingError::CannotRead(format!(
            "failed to resolve variant config path '{}': {}",
            variant_config_path.display(),
            e
        ))
    })?;
    let variant_dir = canonical.parent().ok_or_else(|| {
        ParsingError::CannotParseConfig(format!(
            "variant config path '{}' has no parent directory",
            canonical.display()
        ))
    })?;

    let root_dir = find_root_node_dir(variant_dir)?.ok_or_else(|| {
        ParsingError::CannotParseConfig(format!(
            "no root `{}` with a `manifest.variants` section was found in any parent directory of '{}'",
            NODE_CONFIG_FILE,
            variant_dir.display()
        ))
    })?;

    let root_config = NodeConfigParser::from_path(root_dir.join(NODE_CONFIG_FILE))?;
    root_config
        .merge_variant(variant, &variant_dir.display().to_string())
        .map(|merged| merged.config)
        .map_err(|msg| ParsingError::CannotParseConfig(msg).into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{error::Error, launcher::PeppyLauncherParser, node::ContainerConfig};
    use tempfile::NamedTempFile;

    /// Test helper: borrows the `ContainerConfig` from a parsed config.
    fn container(config: &ParsedNodeConfig) -> &ContainerConfig {
        config
            .0
            .execution
            .as_ref()
            .expect("expected execution")
            .container
            .as_ref()
            .expect("expected container")
    }

    #[test]
    fn test_parse_minimal_config() {
        let json5 = r#"{
            schema_version: 1,
            manifest: {
                name: "test_node",
                tag: "0.1.0",
            },
            execution: {
                language: "rust",
                run_cmd: ["./target/release/test_node"],
            },
        }"#;
        let config = NodeConfigParser::from_content(json5).unwrap();
        assert_eq!(config.0.manifest.name.as_str(), "test_node");
        assert_eq!(config.0.manifest.tag, "0.1.0");
        assert_eq!(
            config
                .0
                .execution
                .as_ref()
                .unwrap()
                .run_cmd
                .as_ref()
                .unwrap(),
            &vec!["./target/release/test_node"]
        );
        assert!(config.0.execution.as_ref().unwrap().parameters.is_empty());
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
            execution: {
                language: "rust",
                run_cmd: ["./target/release/camera_driver"],
            },
        }"#;
        let config = NodeConfigParser::from_content(json5).unwrap();
        assert_eq!(config.0.manifest.name.as_str(), "camera_driver");
        assert_eq!(config.0.manifest.tag, "2.1.0");
        assert_eq!(
            config.0.execution.as_ref().unwrap().language,
            Some(crate::node::PeppygenLanguage::Rust)
        );
        assert_eq!(
            config
                .0
                .execution
                .as_ref()
                .unwrap()
                .run_cmd
                .as_ref()
                .unwrap(),
            &vec!["./target/release/camera_driver"]
        );
        assert!(config.0.interfaces.topics.is_some());
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
            execution: {
                language: "rust",
                container: {
                    def_file: "apptainer.def",
                },
            },
        }"#;
        let config = NodeConfigParser::from_content(json5).unwrap();
        assert!(config.0.execution.as_ref().unwrap().run_cmd.is_none());
        assert_eq!(container(&config).def_file, "apptainer.def");
    }

    #[test]
    fn test_process_and_container_conflict() {
        let json5 = r#"{
            schema_version: 1,
            manifest: {
                name: "bad_node",
                tag: "0.1.0",
            },
            execution: {
                language: "rust",
                run_cmd: ["./bin"],
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
            execution: {
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
    fn test_empty_run_cmd() {
        let json5 = r#"{
            schema_version: 1,
            manifest: {
                name: "empty_cmd_node",
                tag: "0.1.0",
            },
            execution: {
                language: "rust",
                run_cmd: [],
            },
        }"#;
        let result = NodeConfigParser::from_content(json5);
        assert!(matches!(
            result.unwrap_err(),
            Error::Parsing(ParsingError::EmptyRunCmd)
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

    /// Top-level system directories (e.g. `/tmp`) are blocked as mount sources
    /// because Lima 2.0+ rejects them as guest mount points and binding an
    /// entire system directory into a container is almost always a mistake.
    /// Users should mount a subdirectory instead (e.g. `/tmp/my_app`).
    #[test]
    fn test_container_config_rejects_system_path_mount() {
        let json5 = r#"{
            schema_version: 1,
            manifest: {
                name: "bad_mount_node",
                tag: "0.1.0",
            },
            execution: {
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

    /// macOS exposes `/private/tmp`, `/private/var`, etc. as aliases for
    /// `/tmp`, `/var`, etc. The validation strips the `/private` prefix so
    /// these paths are caught by the same blocked-mount-source check.
    #[test]
    fn test_container_config_rejects_private_system_path_mount() {
        let json5 = r#"{
            schema_version: 1,
            manifest: {
                name: "bad_mount_node",
                tag: "0.1.0",
            },
            execution: {
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
            execution: {
                language: "rust",
                container: {
                    def_file: "apptainer.def",
                    mount_paths: ["/tmp/my_app_data:/tmp/my_app_data:rw"],
                },
            },
        }"#;
        let config =
            NodeConfigParser::from_content(json5).expect("subdirectory mount should be accepted");
        assert_eq!(
            container(&config).mount_paths.as_deref().unwrap(),
            &["/tmp/my_app_data:/tmp/my_app_data:rw"]
        );
    }

    #[test]
    fn test_container_config_accepts_no_mount_paths() {
        let json5 = r#"{
            schema_version: 1,
            manifest: {
                name: "no_mount_node",
                tag: "0.1.0",
            },
            execution: {
                language: "rust",
                container: {
                    def_file: "apptainer.def",
                },
            },
        }"#;
        let config = NodeConfigParser::from_content(json5).expect("no mount_paths should be valid");
        assert!(container(&config).mount_paths.is_none());
    }

    // NOTE: These parse-time tests assert the raw `${parameters:...}` template
    // strings. Actual substitution with concrete argument values happens at
    // runtime in `resolve_mount_path_parameters()` (core-node-internal), which
    // has its own test coverage.
    #[test]
    fn test_container_mount_path_with_parameter_ref() {
        let json5 = r#"{
            schema_version: 1,
            manifest: {
                name: "camera_node",
                tag: "0.1.0",
            },
            execution: {
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
        assert_eq!(
            container(&config).mount_paths.as_deref().unwrap(),
            &["${parameters:device_path}:/dev/video0:rw"]
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
            execution: {
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
        assert_eq!(
            container(&config).mount_paths.as_deref().unwrap(),
            &["${parameters:video.device_path}:/dev/video0:rw"]
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
            execution: {
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
            execution: {
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
            execution: {
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
        assert_eq!(
            container(&config).mount_paths.as_deref().unwrap(),
            &["${parameters:path}:/container/data:rw"]
        );
    }

    #[test]
    fn test_container_config_extra_args_default_to_none() {
        let json5 = r#"{
            schema_version: 1,
            manifest: {
                name: "container_node",
                tag: "0.1.0",
            },
            execution: {
                language: "rust",
                container: {
                    def_file: "apptainer.def",
                },
            },
        }"#;
        let config = NodeConfigParser::from_content(json5).unwrap();
        assert!(container(&config).apptainer_build_extra_args.is_none());
        assert!(container(&config).apptainer_run_extra_args.is_none());
        assert!(container(&config).lima_shell_extra_args.is_none());
    }

    #[test]
    fn test_container_config_parses_extra_args() {
        let json5 = r#"{
            schema_version: 1,
            manifest: {
                name: "container_node",
                tag: "0.1.0",
            },
            execution: {
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
        let c = container(&config);
        assert_eq!(
            c.apptainer_build_extra_args.as_deref().unwrap(),
            &["--no-setgroups", "--force"]
        );
        assert_eq!(
            c.apptainer_run_extra_args.as_deref().unwrap(),
            &["--no-setgroups"]
        );
        assert_eq!(
            c.lima_shell_extra_args.as_deref().unwrap(),
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
            execution: {
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
        let c = container(&config);
        assert_eq!(
            c.apptainer_build_extra_args.as_deref().unwrap(),
            &[] as &[String]
        );
        assert_eq!(
            c.apptainer_run_extra_args.as_deref().unwrap(),
            &[] as &[String]
        );
        assert_eq!(
            c.lima_shell_extra_args.as_deref().unwrap(),
            &[] as &[String]
        );
    }

    #[test]
    fn test_parse_config_with_default_variant_no_execution() {
        let json5 = r#"{
            schema_version: 1,
            manifest: {
                name: "uvc_camera",
                tag: "0.1.0",
                variants: [
                    { name: "default", source: { local: "./variants/default" } },
                    { name: "mujoco", source: { local: "./variants/mujoco" } },
                ],
            },
            interfaces: {
                topics: {
                    emits: [{ name: "image" }],
                },
            },
        }"#;
        let config = NodeConfigParser::from_content(json5).unwrap();
        assert_eq!(config.0.manifest.name.as_str(), "uvc_camera");
        assert!(config.0.execution.is_none());
        assert!(config.has_default_variant());
    }

    #[test]
    fn test_parse_config_with_default_variant_and_execution_rejected() {
        let json5 = r#"{
            schema_version: 1,
            manifest: {
                name: "uvc_camera",
                tag: "0.1.0",
                variants: [
                    { name: "default", source: { local: "./variants/default" } },
                ],
            },
            execution: {
                language: "rust",
                run_cmd: ["./target/release/uvc_camera"],
            },
        }"#;
        let result = NodeConfigParser::from_content(json5);
        assert!(matches!(
            result.unwrap_err(),
            Error::Parsing(ParsingError::ExecutionWithDefaultVariant)
        ));
    }

    #[test]
    fn test_parse_config_with_default_variant_and_execution_without_language_rejected() {
        let json5 = r#"{
            schema_version: 1,
            manifest: {
                name: "uvc_camera",
                tag: "0.1.0",
                variants: [
                    { name: "default", source: { local: "./variants/linux" } },
                ],
            },
            execution: {
                container: {
                    def_file: "apptainer.def",
                },
                parameters: {
                    device_path: "string",
                },
            },
        }"#;
        let result = NodeConfigParser::from_content(json5);
        assert!(matches!(
            result.unwrap_err(),
            Error::Parsing(ParsingError::ExecutionWithDefaultVariant)
        ));
    }

    #[test]
    fn test_parse_config_execution_without_language_rejected() {
        let json5 = r#"{
            schema_version: 1,
            manifest: {
                name: "test_node",
                tag: "0.1.0",
            },
            execution: {
                run_cmd: ["./bin"],
            },
        }"#;
        let result = NodeConfigParser::from_content(json5);
        assert!(matches!(
            result.unwrap_err(),
            Error::Parsing(ParsingError::MissingExecutionLanguage)
        ));
    }

    #[test]
    fn test_parse_config_no_execution_no_default_variant_rejected() {
        let json5 = r#"{
            schema_version: 1,
            manifest: {
                name: "uvc_camera",
                tag: "0.1.0",
                variants: [
                    { name: "mujoco", source: { local: "./variants/mujoco" } },
                ],
            },
        }"#;
        let result = NodeConfigParser::from_content(json5);
        assert!(matches!(
            result.unwrap_err(),
            Error::Parsing(ParsingError::MissingExecution)
        ));
    }

    #[test]
    fn test_parse_config_no_execution_with_default_variant_accepted() {
        let json5 = r#"{
            schema_version: 1,
            manifest: {
                name: "uvc_camera",
                tag: "0.1.0",
                variants: [
                    { name: "default", source: { local: "./variants/my_default_variant" } },
                ],
            },
        }"#;
        let result = NodeConfigParser::from_content(json5);
        assert!(
            result.is_ok(),
            "expected parsing to succeed when a 'default' variant is present, but got: {:?}",
            result.unwrap_err()
        );
    }

    #[test]
    fn test_node_error_message_includes_field_path() {
        // run_cmd should be an array, not a map
        let json5 = r#"{
            schema_version: 1,
            manifest: {
                name: "test_node",
                tag: "0.1.0",
            },
            execution: {
                language: "rust",
                run_cmd: { wrong: "type" },
            },
        }"#;
        let result = NodeConfigParser::from_content(json5);
        let Error::Parsing(ParsingError::CannotParseConfig(msg)) = result.unwrap_err() else {
            panic!("expected CannotParseConfig error");
        };
        assert!(
            msg.contains("execution.run_cmd"),
            "error should include field path, got: {msg}"
        );
    }

    #[test]
    fn test_variant_error_message_includes_field_path() {
        // run_cmd should be an array, not a string
        let json5 = r#"{
            schema_version: 1,
            execution: {
                language: "rust",
                run_cmd: "not_an_array",
            },
        }"#;
        let result = VariantConfigParser::from_content(json5);
        let Error::Parsing(ParsingError::CannotParseConfig(msg)) = result.unwrap_err() else {
            panic!("expected CannotParseConfig error");
        };
        assert!(
            msg.contains("execution.run_cmd"),
            "error should include field path, got: {msg}"
        );
    }

    // -- load_standalone_node_config tests -----------------------------------

    use tempfile::TempDir;

    const ROOT_CONFIG_WITH_LOCAL_VARIANT: &str = r#"{
        schema_version: 1,
        manifest: {
            name: "uvc_camera",
            tag: "0.1.0",
            variants: [
                { name: "mock", source: { local: "variants/mock" } },
            ],
        },
        execution: {
            language: "rust",
            run_cmd: ["./target/debug/uvc_camera"],
        },
    }"#;

    const VARIANT_CONFIG_NO_MANIFEST: &str = r#"{
        schema_version: 1,
        execution: {
            language: "rust",
            run_cmd: ["./target/debug/mock"],
        },
    }"#;

    /// Writes a `peppy.json5` root + a `variants/mock/peppy.json5` variant
    /// into `temp` and returns the path to the variant file.
    fn write_root_and_variant(temp: &TempDir, root: &str, variant: &str) -> std::path::PathBuf {
        let root_path = temp.path().join(NODE_CONFIG_FILE);
        std::fs::write(&root_path, root).expect("root config should be written");

        let variant_dir = temp.path().join("variants").join("mock");
        std::fs::create_dir_all(&variant_dir).expect("variant dir should be created");
        let variant_path = variant_dir.join(NODE_CONFIG_FILE);
        std::fs::write(&variant_path, variant).expect("variant config should be written");
        variant_path
    }

    #[test]
    fn load_standalone_root_config_passes_through() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join(NODE_CONFIG_FILE);
        let root = r#"{
            schema_version: 1,
            manifest: { name: "my_node", tag: "0.1.0" },
            execution: { language: "rust", run_cmd: ["./target/debug/my_node"] },
        }"#;
        std::fs::write(&path, root).unwrap();

        let node_config =
            load_standalone_node_config(&path).expect("root config should load cleanly");
        assert_eq!(node_config.manifest.name.as_str(), "my_node");
        assert_eq!(
            node_config.execution.run_cmd.as_deref(),
            Some(["./target/debug/my_node".to_string()].as_slice())
        );
    }

    #[test]
    fn load_standalone_variant_no_manifest_no_interfaces() {
        let temp = TempDir::new().unwrap();
        let variant_path = write_root_and_variant(
            &temp,
            ROOT_CONFIG_WITH_LOCAL_VARIANT,
            VARIANT_CONFIG_NO_MANIFEST,
        );

        let merged =
            load_standalone_node_config(&variant_path).expect("variant should merge with parent");

        // Manifest comes from the root.
        assert_eq!(merged.manifest.name.as_str(), "uvc_camera");
        assert_eq!(merged.manifest.tag, "0.1.0");
        // Execution comes from the variant.
        assert_eq!(
            merged.execution.run_cmd.as_deref(),
            Some(["./target/debug/mock".to_string()].as_slice())
        );
    }

    #[test]
    fn load_standalone_variant_missing_only_interfaces_is_fine() {
        // Variant omits interfaces (which is what the user's uvc_camera
        // variants do). Same as the primary case — just double-checks the
        // absence of `interfaces` alone isn't a problem.
        let temp = TempDir::new().unwrap();
        let variant = r#"{
            schema_version: 1,
            execution: { language: "rust", run_cmd: ["./mock"] },
        }"#;
        let variant_path = write_root_and_variant(&temp, ROOT_CONFIG_WITH_LOCAL_VARIANT, variant);

        let merged = load_standalone_node_config(&variant_path)
            .expect("variant without interfaces should merge");
        assert_eq!(merged.manifest.name.as_str(), "uvc_camera");
    }

    #[test]
    fn load_standalone_variant_parent_not_found() {
        let temp = TempDir::new().unwrap();
        // Put an orphan variant config without any ancestor peppy.json5.
        let orphan_dir = temp.path().join("orphan");
        std::fs::create_dir_all(&orphan_dir).unwrap();
        let variant_path = orphan_dir.join(NODE_CONFIG_FILE);
        std::fs::write(&variant_path, VARIANT_CONFIG_NO_MANIFEST).unwrap();

        let err = load_standalone_node_config(&variant_path)
            .expect_err("orphan variant should fail to resolve parent");
        let msg = err.to_string();
        assert!(
            msg.contains("no root") && msg.contains("manifest"),
            "error should explain parent was not found, got: {msg}"
        );
    }

    #[test]
    fn load_standalone_variant_parent_has_no_variants() {
        // Parent peppy.json5 exists and is a valid root, but its manifest has
        // no `variants` list — `find_root_node_dir` treats it as a non-root
        // ancestor and keeps walking, eventually failing.
        let temp = TempDir::new().unwrap();
        let root_path = temp.path().join(NODE_CONFIG_FILE);
        let root_without_variants = r#"{
            schema_version: 1,
            manifest: { name: "solo_node", tag: "0.1.0" },
            execution: { language: "rust", run_cmd: ["./solo"] },
        }"#;
        std::fs::write(&root_path, root_without_variants).unwrap();

        let variant_dir = temp.path().join("variants").join("mock");
        std::fs::create_dir_all(&variant_dir).unwrap();
        let variant_path = variant_dir.join(NODE_CONFIG_FILE);
        std::fs::write(&variant_path, VARIANT_CONFIG_NO_MANIFEST).unwrap();

        let err = load_standalone_node_config(&variant_path)
            .expect_err("parent without a variants list should fail");
        let msg = err.to_string();
        assert!(
            msg.contains("no root") && msg.contains("variants"),
            "error should mention that no root with variants was found, got: {msg}"
        );
    }

    /// Regression guard for the bare-filename bug that caused
    /// `cargo run` from inside a variant directory to fail with
    /// "no root with manifest.variants was found" even though the parent
    /// existed. The CLI passes [`NODE_CONFIG_FILE`] (a bare filename with no
    /// directory component) as the config path. `Path::parent()` on such a
    /// path returns `Some("")` — an empty relative path — and walking up
    /// from an empty path starts at `/` instead of the variant's real
    /// location. The fix is `fs::canonicalize()` inside
    /// [`merge_variant_with_parent`], which produces an absolute path whose
    /// `.parent()` is the real directory.
    ///
    /// We can't exercise the cwd-dependent path directly in a unit test
    /// without mutating `std::env::current_dir`, which would race against
    /// other tests in this crate that read cwd implicitly (e.g. `capnpc`
    /// invocations in `encoding` tests). The bug is instead covered
    /// end-to-end by running the real `mock_rust` / `mock_python` variants —
    /// this assertion is just a lightweight canary confirming that the
    /// underlying platform behavior hasn't changed.
    #[test]
    fn bare_filename_has_empty_parent_sentinel() {
        let bare = Path::new(NODE_CONFIG_FILE);
        assert_eq!(
            bare.parent(),
            Some(Path::new("")),
            "Path::parent() on a bare filename must still return an empty path — \
             if this ever changes, merge_variant_with_parent can drop the \
             canonicalize() call"
        );
    }

    #[test]
    fn load_standalone_variant_deep_nesting() {
        // Root is 3 levels above the variant dir. Walk-up must still find it.
        let temp = TempDir::new().unwrap();
        let root_path = temp.path().join(NODE_CONFIG_FILE);
        let root = r#"{
            schema_version: 1,
            manifest: {
                name: "deep_node",
                tag: "0.1.0",
                variants: [
                    { name: "mock", source: { local: "a/b/variants/mock" } },
                ],
            },
            execution: { language: "rust", run_cmd: ["./deep"] },
        }"#;
        std::fs::write(&root_path, root).unwrap();

        let variant_dir = temp
            .path()
            .join("a")
            .join("b")
            .join("variants")
            .join("mock");
        std::fs::create_dir_all(&variant_dir).unwrap();
        let variant_path = variant_dir.join(NODE_CONFIG_FILE);
        std::fs::write(&variant_path, VARIANT_CONFIG_NO_MANIFEST).unwrap();

        let merged = load_standalone_node_config(&variant_path)
            .expect("deeply nested variant should still resolve its root");
        assert_eq!(merged.manifest.name.as_str(), "deep_node");
    }

    /// Future-proofing test: when a new field is added to [`NodeConfig`], this
    /// exhaustive destructuring will fail to compile, forcing the author to
    /// think about how the merge should treat it.  Catches the class of bug
    /// where `manifest`/`interfaces` were required on `NodeConfig` but a
    /// variant legitimately omitted them (the bug this file fixes).
    #[test]
    fn load_standalone_variant_merged_covers_all_node_config_fields() {
        let temp = TempDir::new().unwrap();
        let variant_path = write_root_and_variant(
            &temp,
            ROOT_CONFIG_WITH_LOCAL_VARIANT,
            VARIANT_CONFIG_NO_MANIFEST,
        );

        let merged = load_standalone_node_config(&variant_path).unwrap();

        // Exhaustive destructuring — add-a-field will fail compilation here.
        let NodeConfig {
            schema_version,
            manifest,
            interfaces,
            execution,
        } = merged;

        // schema_version comes from the variant (which matches the root).
        assert_eq!(schema_version, 1);
        // manifest is inherited from the root.
        assert_eq!(manifest.name.as_str(), "uvc_camera");
        assert_eq!(manifest.tag, "0.1.0");
        // interfaces inherited from the root (empty in this fixture).
        assert_eq!(interfaces, crate::node::Interfaces::default());
        // execution comes from the variant.
        assert_eq!(
            execution.run_cmd.as_deref(),
            Some(["./target/debug/mock".to_string()].as_slice())
        );
    }
}
