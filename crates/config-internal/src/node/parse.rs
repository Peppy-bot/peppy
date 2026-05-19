use super::types::{Execution, Manifest, NodeConfig};
use crate::{
    error::{ParsingError, Result},
    parsing::read_non_empty_file,
};
use std::collections::HashSet;
use std::path::Path;

/// Validates `manifest.depends_on`:
///
/// - Rejects duplicate `link_id`s across the combined nodes/interfaces set.
///   `link_id` is the shared identity for both: consumed topics/services/actions
///   resolve their producer by `link_id` alone, so a collision either silently
///   overwrites a resolution or binds to the wrong producer.
/// - Rejects more than one entry with `from_any: true` for the same
///   `(name, tag)` pair. `from_any` marks a dependency as a wildcard producer;
///   two wildcards for the same `(name, tag)` would be ambiguous at resolution.
fn validate_depends_on(manifest: &Manifest) -> Result<()> {
    let Some(depends_on) = &manifest.depends_on else {
        return Ok(());
    };
    let mut seen_link_ids: HashSet<&str> = HashSet::new();
    let mut seen_from_any: HashSet<(&str, &str)> = HashSet::new();
    let nodes = depends_on.nodes.iter().map(|n| {
        (
            n.link_id.as_str(),
            n.name.as_str(),
            n.tag.as_str(),
            n.from_any,
        )
    });
    let interfaces = depends_on.interfaces.iter().map(|i| {
        (
            i.link_id.as_str(),
            i.name.as_str(),
            i.tag.as_str(),
            i.from_any,
        )
    });
    for (link_id, name, tag, from_any) in nodes.chain(interfaces) {
        if !seen_link_ids.insert(link_id) {
            return Err(ParsingError::DuplicateLinkId(link_id.to_owned()).into());
        }
        if from_any && !seen_from_any.insert((name, tag)) {
            return Err(ParsingError::ConflictingFromAny {
                name: name.to_owned(),
                tag: tag.to_owned(),
            }
            .into());
        }
    }
    Ok(())
}

/// Validates execution constraints.
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
    pub fn from_path(file: impl AsRef<Path>) -> Result<NodeConfig> {
        let path = file.as_ref();
        let content = read_non_empty_file(path)?;
        Self::from_content(&content)
    }

    /// Takes a JSON5 content as parameter
    pub fn from_content(content: &str) -> Result<NodeConfig> {
        // Strict schema validation is handled by serde via #[serde(deny_unknown_fields)]
        let config: NodeConfig = crate::error::deserialize_json5_with_path(content)?;
        validate_execution(&config.execution)?;
        validate_depends_on(&config.manifest)?;
        Ok(config)
    }
}

/// Loads a `peppy.json5` at `path` and returns a fully resolved [`NodeConfig`].
///
/// Used by `peppylib`'s standalone mode so Rust and Python nodes can
/// `cargo run` / `python -m` directly from a node directory.
pub fn load_standalone_node_config(path: impl AsRef<Path>) -> Result<NodeConfig> {
    NodeConfigParser::from_path(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{error::Error, launcher::PeppyLauncherParser, node::ContainerConfig};
    use tempfile::NamedTempFile;

    /// Test helper: borrows the `ContainerConfig` from a parsed config.
    fn container(config: &NodeConfig) -> &ContainerConfig {
        config
            .execution
            .container
            .as_ref()
            .expect("expected container")
    }

    #[test]
    fn test_parse_minimal_config() {
        let json5 = r#"{
            peppy_schema: "node_v1",
            manifest: {
                name: "test_node",
                tag: "v1",
            },
            execution: {
                language: "rust",
                run_cmd: ["./target/release/test_node"],
            },
        }"#;
        let config = NodeConfigParser::from_content(json5).unwrap();
        assert_eq!(config.manifest.name.as_str(), "test_node");
        assert_eq!(config.manifest.tag, "v1");
        assert_eq!(
            config.execution.run_cmd.as_ref().unwrap(),
            &vec!["./target/release/test_node"]
        );
        assert!(config.execution.parameters.is_empty());
    }

    #[test]
    fn test_parse_complex_config() {
        let json5 = r#"{
            peppy_schema: "node_v1",
            manifest: {
                name: "camera_driver",
                tag: "v21",
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
        assert_eq!(config.manifest.name.as_str(), "camera_driver");
        assert_eq!(config.manifest.tag, "v21");
        assert_eq!(
            config.execution.language,
            crate::node::PeppygenLanguage::Rust
        );
        assert_eq!(
            config.execution.run_cmd.as_ref().unwrap(),
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
            Error::Parsing(ParsingError::CannotRead(_, _))
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
            peppy_schema: "node_v1",
            manifest: {
                name: "container_node",
                tag: "v1",
            },
            execution: {
                language: "rust",
                container: {
                    def_file: "apptainer.def",
                },
            },
        }"#;
        let config = NodeConfigParser::from_content(json5).unwrap();
        assert!(config.execution.run_cmd.is_none());
        assert_eq!(container(&config).def_file, "apptainer.def");
    }

    #[test]
    fn test_process_and_container_conflict() {
        let json5 = r#"{
            peppy_schema: "node_v1",
            manifest: {
                name: "bad_node",
                tag: "v1",
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
            peppy_schema: "node_v1",
            manifest: {
                name: "bare_node",
                tag: "v1",
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
    fn test_duplicate_link_id_within_nodes_rejected() {
        let json5 = r#"{
            peppy_schema: "node_v1",
            manifest: {
                name: "dup_node",
                tag: "v1",
                depends_on: {
                    nodes: [
                        { name: "alpha", tag: "v1", link_id: "shared" },
                        { name: "beta",  tag: "v1", link_id: "shared" },
                    ],
                },
            },
            execution: {
                language: "rust",
                run_cmd: ["./bin"],
            },
        }"#;
        let result = NodeConfigParser::from_content(json5);
        assert!(
            matches!(
                result.as_ref().unwrap_err(),
                Error::Parsing(ParsingError::DuplicateLinkId(id)) if id == "shared"
            ),
            "expected DuplicateLinkId(\"shared\"), got: {:?}",
            result.unwrap_err()
        );
    }

    #[test]
    fn test_duplicate_link_id_across_nodes_and_interfaces_rejected() {
        let json5 = r#"{
            peppy_schema: "node_v1",
            manifest: {
                name: "dup_node",
                tag: "v1",
                depends_on: {
                    nodes: [
                        { name: "alpha", tag: "v1", link_id: "shared" },
                    ],
                    interfaces: [
                        { name: "depth_camera", tag: "v1", link_id: "shared" },
                    ],
                },
            },
            execution: {
                language: "rust",
                run_cmd: ["./bin"],
            },
        }"#;
        let result = NodeConfigParser::from_content(json5);
        assert!(
            matches!(
                result.as_ref().unwrap_err(),
                Error::Parsing(ParsingError::DuplicateLinkId(id)) if id == "shared"
            ),
            "expected DuplicateLinkId(\"shared\") across nodes+interfaces, got: {:?}",
            result.unwrap_err()
        );
    }

    #[test]
    fn test_unique_link_ids_parse_ok() {
        // Same (name, tag) repeated under distinct link_ids must parse.
        let json5 = r#"{
            peppy_schema: "node_v1",
            manifest: {
                name: "openarm01_backbone",
                tag: "v1",
                depends_on: {
                    nodes: [
                        { name: "depth_camera", tag: "v1", link_id: "wrist_left_camera" },
                        { name: "depth_camera", tag: "v1", link_id: "wrist_right_camera" },
                        { name: "depth_camera", tag: "v1", link_id: "torso_camera" },
                    ],
                },
            },
            interfaces: {
                topics: {
                    consumes: [
                        { link_id: "wrist_left_camera",  name: "video_stream" },
                        { link_id: "wrist_right_camera", name: "video_stream" },
                        { link_id: "torso_camera",       name: "video_stream" },
                    ],
                },
            },
            execution: {
                language: "rust",
                build_cmd: ["cargo", "build", "--release"],
                run_cmd: ["./target/release/backbone"],
            },
        }"#;
        let config =
            NodeConfigParser::from_content(json5).expect("distinct link_ids should parse cleanly");
        let deps = config
            .manifest
            .depends_on
            .expect("depends_on should be set");
        assert_eq!(deps.nodes.len(), 3);
    }

    #[test]
    fn test_from_any_defaults_to_false() {
        let json5 = r#"{
            peppy_schema: "node_v1",
            manifest: {
                name: "default_node",
                tag: "v1",
                depends_on: {
                    nodes: [
                        { name: "alpha", tag: "v1", link_id: "a" },
                    ],
                    interfaces: [
                        { name: "beta", tag: "v1", link_id: "b" },
                    ],
                },
            },
            execution: {
                language: "rust",
                run_cmd: ["./bin"],
            },
        }"#;
        let config = NodeConfigParser::from_content(json5).unwrap();
        let deps = config.manifest.depends_on.unwrap();
        assert!(!deps.nodes[0].from_any);
        assert!(!deps.interfaces[0].from_any);
    }

    #[test]
    fn test_from_any_explicit_true_parses() {
        // A single `from_any: true` node entry alongside three plain
        // interface entries with the same (name, tag).
        let json5 = r#"{
            peppy_schema: "node_v1",
            manifest: {
                name: "openarm01_backbone",
                tag: "v1",
                depends_on: {
                    interfaces: [
                        { name: "depth_camera", tag: "v1", link_id: "wrist_left_camera" },
                        { name: "depth_camera", tag: "v1", link_id: "wrist_right_camera" },
                        { name: "depth_camera", tag: "v1", link_id: "torso_camera" },
                    ],
                    nodes: [
                        { name: "depth_camera", tag: "v1", link_id: "extra_camera", from_any: true },
                    ],
                },
            },
            execution: {
                language: "rust",
                run_cmd: ["./bin"],
            },
        }"#;
        let config = NodeConfigParser::from_content(json5).unwrap();
        let deps = config.manifest.depends_on.unwrap();
        assert_eq!(deps.nodes.len(), 1);
        assert!(deps.nodes[0].from_any);
        assert_eq!(deps.interfaces.len(), 3);
        assert!(deps.interfaces.iter().all(|i| !i.from_any));
    }

    #[test]
    fn test_from_any_explicit_true_on_interface_with_node_duplicate() {
        // The wildcard is on an interface entry; a plain node entry shares
        // (name, tag).
        let json5 = r#"{
            peppy_schema: "node_v1",
            manifest: {
                name: "openarm01_backbone",
                tag: "v1",
                depends_on: {
                    interfaces: [
                        { name: "depth_camera", tag: "v1", link_id: "wrist_left_camera" },
                        { name: "depth_camera", tag: "v1", link_id: "wrist_right_camera" },
                        { name: "depth_camera", tag: "v1", link_id: "torso_camera", from_any: true },
                    ],
                    nodes: [
                        { name: "depth_camera", tag: "v1", link_id: "extra_camera" },
                    ],
                },
            },
            execution: {
                language: "rust",
                run_cmd: ["./bin"],
            },
        }"#;
        let config = NodeConfigParser::from_content(json5).unwrap();
        let deps = config.manifest.depends_on.unwrap();
        assert!(!deps.nodes[0].from_any);
        let from_any_count = deps.interfaces.iter().filter(|i| i.from_any).count();
        assert_eq!(from_any_count, 1);
    }

    #[test]
    fn test_conflicting_from_any_two_nodes_rejected() {
        let json5 = r#"{
            peppy_schema: "node_v1",
            manifest: {
                name: "dup_from_any_node",
                tag: "v1",
                depends_on: {
                    nodes: [
                        { name: "depth_camera", tag: "v1", link_id: "a", from_any: true },
                        { name: "depth_camera", tag: "v1", link_id: "b", from_any: true },
                    ],
                },
            },
            execution: {
                language: "rust",
                run_cmd: ["./bin"],
            },
        }"#;
        let result = NodeConfigParser::from_content(json5);
        assert!(
            matches!(
                result.as_ref().unwrap_err(),
                Error::Parsing(ParsingError::ConflictingFromAny { name, tag })
                    if name == "depth_camera" && tag == "v1"
            ),
            "expected ConflictingFromAny for (depth_camera, v1), got: {:?}",
            result.unwrap_err()
        );
    }

    #[test]
    fn test_conflicting_from_any_two_interfaces_rejected() {
        let json5 = r#"{
            peppy_schema: "node_v1",
            manifest: {
                name: "dup_from_any_iface",
                tag: "v1",
                depends_on: {
                    nodes: [],
                    interfaces: [
                        { name: "depth_camera", tag: "v1", link_id: "a", from_any: true },
                        { name: "depth_camera", tag: "v1", link_id: "b", from_any: true },
                    ],
                },
            },
            execution: {
                language: "rust",
                run_cmd: ["./bin"],
            },
        }"#;
        let result = NodeConfigParser::from_content(json5);
        assert!(
            matches!(
                result.as_ref().unwrap_err(),
                Error::Parsing(ParsingError::ConflictingFromAny { name, tag })
                    if name == "depth_camera" && tag == "v1"
            ),
            "expected ConflictingFromAny for (depth_camera, v1), got: {:?}",
            result.unwrap_err()
        );
    }

    #[test]
    fn test_conflicting_from_any_across_node_and_interface_rejected() {
        let json5 = r#"{
            peppy_schema: "node_v1",
            manifest: {
                name: "dup_from_any_mixed",
                tag: "v1",
                depends_on: {
                    nodes: [
                        { name: "depth_camera", tag: "v1", link_id: "a", from_any: true },
                    ],
                    interfaces: [
                        { name: "depth_camera", tag: "v1", link_id: "b", from_any: true },
                    ],
                },
            },
            execution: {
                language: "rust",
                run_cmd: ["./bin"],
            },
        }"#;
        let result = NodeConfigParser::from_content(json5);
        assert!(
            matches!(
                result.as_ref().unwrap_err(),
                Error::Parsing(ParsingError::ConflictingFromAny { name, tag })
                    if name == "depth_camera" && tag == "v1"
            ),
            "expected ConflictingFromAny across nodes+interfaces, got: {:?}",
            result.unwrap_err()
        );
    }

    #[test]
    fn test_from_any_true_allowed_for_distinct_name_tag_pairs() {
        // Different (name, tag) pairs may each carry their own from_any=true.
        let json5 = r#"{
            peppy_schema: "node_v1",
            manifest: {
                name: "distinct_from_any",
                tag: "v1",
                depends_on: {
                    nodes: [
                        { name: "depth_camera", tag: "v1", link_id: "a", from_any: true },
                        { name: "imu_sensor",   tag: "v1", link_id: "b", from_any: true },
                    ],
                },
            },
            execution: {
                language: "rust",
                run_cmd: ["./bin"],
            },
        }"#;
        let config = NodeConfigParser::from_content(json5)
            .expect("distinct (name, tag) pairs each with from_any=true should parse");
        let deps = config.manifest.depends_on.unwrap();
        assert!(deps.nodes.iter().all(|n| n.from_any));
    }

    #[test]
    fn test_empty_run_cmd() {
        let json5 = r#"{
            peppy_schema: "node_v1",
            manifest: {
                name: "empty_cmd_node",
                tag: "v1",
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
            peppy_schema: "launcher_v1",
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
            peppy_schema: "node_v1",
            manifest: {
                name: "bad_mount_node",
                tag: "v1",
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
            peppy_schema: "node_v1",
            manifest: {
                name: "bad_mount_node",
                tag: "v1",
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
            peppy_schema: "node_v1",
            manifest: {
                name: "good_mount_node",
                tag: "v1",
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
            peppy_schema: "node_v1",
            manifest: {
                name: "no_mount_node",
                tag: "v1",
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
            peppy_schema: "node_v1",
            manifest: {
                name: "camera_node",
                tag: "v1",
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
            peppy_schema: "node_v1",
            manifest: {
                name: "camera_node",
                tag: "v1",
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
            peppy_schema: "node_v1",
            manifest: {
                name: "bad_ref_node",
                tag: "v1",
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
            peppy_schema: "node_v1",
            manifest: {
                name: "bad_type_node",
                tag: "v1",
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
            peppy_schema: "node_v1",
            manifest: {
                name: "dynamic_mount_node",
                tag: "v1",
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
            peppy_schema: "node_v1",
            manifest: {
                name: "container_node",
                tag: "v1",
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
            peppy_schema: "node_v1",
            manifest: {
                name: "container_node",
                tag: "v1",
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
            peppy_schema: "node_v1",
            manifest: {
                name: "container_node",
                tag: "v1",
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
    fn test_parse_config_execution_without_language_rejected() {
        let json5 = r#"{
            peppy_schema: "node_v1",
            manifest: {
                name: "test_node",
                tag: "v1",
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
    fn test_node_error_message_includes_field_path() {
        // run_cmd should be an array, not a map
        let json5 = r#"{
            peppy_schema: "node_v1",
            manifest: {
                name: "test_node",
                tag: "v1",
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
    fn load_standalone_returns_resolved_config() {
        let tmp = NamedTempFile::new().unwrap();
        let json5 = r#"{
            peppy_schema: "node_v1",
            manifest: { name: "my_node", tag: "v1" },
            execution: { language: "rust", run_cmd: ["./target/debug/my_node"] },
        }"#;
        std::fs::write(tmp.path(), json5).unwrap();

        let node_config =
            load_standalone_node_config(tmp.path()).expect("config should load cleanly");
        assert_eq!(node_config.manifest.name.as_str(), "my_node");
        assert_eq!(
            node_config.execution.run_cmd.as_deref(),
            Some(["./target/debug/my_node".to_string()].as_slice())
        );
    }

    /// Future-proofing test: when a new field is added to [`NodeConfig`], this
    /// exhaustive destructuring will fail to compile, forcing the author to
    /// think about how it should be handled.
    #[test]
    fn node_config_field_set_is_complete() {
        let tmp = NamedTempFile::new().unwrap();
        let json5 = r#"{
            peppy_schema: "node_v1",
            manifest: { name: "my_node", tag: "v1" },
            execution: { language: "rust", run_cmd: ["./target/debug/my_node"] },
        }"#;
        std::fs::write(tmp.path(), json5).unwrap();

        let merged = load_standalone_node_config(tmp.path()).unwrap();

        // Exhaustive destructuring — add-a-field will fail compilation here.
        let NodeConfig {
            peppy_schema,
            manifest,
            interfaces,
            execution,
        } = merged;

        assert_eq!(peppy_schema, crate::schema::PeppySchema::NodeV1);
        assert_eq!(manifest.name.as_str(), "my_node");
        assert_eq!(manifest.tag, "v1");
        assert_eq!(interfaces, crate::node::Interfaces::default());
        assert_eq!(
            execution.run_cmd.as_deref(),
            Some(["./target/debug/my_node".to_string()].as_slice())
        );
    }
}
