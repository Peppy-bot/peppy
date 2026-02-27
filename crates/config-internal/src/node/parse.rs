use super::types::NodeConfig;
use crate::error::{ParsingError, Result};
use std::collections::HashSet;
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
        let config: NodeConfig = serde_json5::from_str(content).map_err(ParsingError::from)?;
        validate_dataflow_references(&config)?;
        Ok(config)
    }
}

/// Validates that dataflow group references point to existing interfaces.
///
/// For each `DataflowGroup` declared on the node:
/// - Every `publishes` entry must match a `name` in `exposes.topics`
/// - Every `consumes` entry must match an `id` in `subscribes_to.topics`
/// - At least one of `publishes` / `consumes` must be non-empty
/// - No duplicate group names within the same node
fn validate_dataflow_references(config: &NodeConfig) -> std::result::Result<(), ParsingError> {
    let dataflow = &config.interfaces.dataflow;
    if dataflow.is_empty() {
        return Ok(());
    }

    // Check for duplicate group names
    let mut seen_groups = HashSet::new();
    for group in dataflow {
        let group_name = group.group.as_str();
        if !seen_groups.insert(group_name) {
            return Err(ParsingError::DataflowDuplicateGroup {
                group: group_name.to_string(),
            });
        }
    }

    // Collect exposed topic names
    let exposed_topic_names: HashSet<&str> = config
        .interfaces
        .exposes
        .as_ref()
        .and_then(|e| e.topics.as_ref())
        .map(|topics| topics.iter().map(|t| t.name.as_str()).collect())
        .unwrap_or_default();

    // Collect subscribed topic IDs
    let subscribed_topic_ids: HashSet<&str> = config
        .interfaces
        .subscribes_to
        .as_ref()
        .and_then(|s| s.topics.as_ref())
        .map(|topics| topics.iter().map(|t| t.id.as_str()).collect())
        .unwrap_or_default();

    for group in dataflow {
        let group_name = group.group.as_str();

        // At least one of publishes/consumes must be non-empty
        if group.publishes.is_empty() && group.consumes.is_empty() {
            return Err(ParsingError::DataflowEmptyGroup {
                group: group_name.to_string(),
            });
        }

        // Validate publishes references
        for topic_name in &group.publishes {
            if !exposed_topic_names.contains(topic_name.as_str()) {
                return Err(ParsingError::DataflowPublishesUnknownTopic {
                    group: group_name.to_string(),
                    topic: topic_name.clone(),
                });
            }
        }

        // Validate consumes references
        for topic_id in &group.consumes {
            if !subscribed_topic_ids.contains(topic_id.as_str()) {
                return Err(ParsingError::DataflowConsumesUnknownTopic {
                    group: group_name.to_string(),
                    id: topic_id.clone(),
                });
            }
        }
    }

    Ok(())
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
                language: "rust",
            },
            build: {
                start_cmd: ["./target/release/test_node"],
            },
        }"#;
        let config = NodeConfigParser::from_content(json5).unwrap();
        assert_eq!(config.manifest.name.as_str(), "test_node");
        assert_eq!(config.manifest.tag, "0.1.0");
        assert_eq!(config.build.start_cmd, vec!["./target/release/test_node"]);
        assert!(config.parameters.is_empty());
    }

    #[test]
    fn test_parse_complex_config() {
        let json5 = r#"{
            schema_version: 1,
            manifest: {
                name: "camera_driver",
                tag: "2.1.0",
                language: "rust",
            },
            build: {
                start_cmd: ["./target/release/camera_driver"],
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
        assert_eq!(
            config.manifest.language,
            crate::node::PeppygenLanguage::Rust
        );
        assert_eq!(
            config.build.start_cmd,
            vec!["./target/release/camera_driver"]
        );
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

    // -- Dataflow validation tests --

    fn node_config_with_dataflow(dataflow_json: &str) -> String {
        format!(
            r#"{{
                schema_version: 1,
                manifest: {{ name: "test_node", tag: "0.1.0", language: "rust" }},
                build: {{ start_cmd: ["./test"] }},
                interfaces: {{
                    exposes: {{
                        topics: [
                            {{ name: "object_position", qos_profile: "sensor_data", message_format: {{ x: "f64" }} }},
                            {{ name: "arm_position", qos_profile: "sensor_data", message_format: {{ y: "f64" }} }}
                        ]
                    }},
                    subscribes_to: {{
                        topics: [
                            {{ id: "arm_state", node: "arm_controller", name: "arm_position" }},
                            {{ id: "target_pos", node: "vision", name: "object_position" }}
                        ]
                    }},
                    dataflow: [{dataflow_json}]
                }}
            }}"#
        )
    }

    #[test]
    fn dataflow_valid_config_passes_validation() {
        let json5 = node_config_with_dataflow(
            r#"{ group: "servo_loop", role: "sensor", publishes: ["object_position"], consumes: ["arm_state"], rate_hz: 30.0 }"#,
        );
        let config =
            NodeConfigParser::from_content(&json5).expect("valid dataflow config should parse");
        assert_eq!(config.interfaces.dataflow.len(), 1);
        assert_eq!(config.interfaces.dataflow[0].group.as_str(), "servo_loop");
    }

    #[test]
    fn dataflow_publishes_unknown_topic_fails() {
        let json5 =
            node_config_with_dataflow(r#"{ group: "loop", publishes: ["nonexistent_topic"] }"#);
        let result = NodeConfigParser::from_content(&json5);
        let Error::Parsing(ParsingError::DataflowPublishesUnknownTopic { group, topic }) =
            result.unwrap_err()
        else {
            panic!("expected DataflowPublishesUnknownTopic error");
        };
        assert_eq!(group, "loop");
        assert_eq!(topic, "nonexistent_topic");
    }

    #[test]
    fn dataflow_consumes_unknown_topic_id_fails() {
        let json5 = node_config_with_dataflow(r#"{ group: "loop", consumes: ["nonexistent_id"] }"#);
        let result = NodeConfigParser::from_content(&json5);
        let Error::Parsing(ParsingError::DataflowConsumesUnknownTopic { group, id }) =
            result.unwrap_err()
        else {
            panic!("expected DataflowConsumesUnknownTopic error");
        };
        assert_eq!(group, "loop");
        assert_eq!(id, "nonexistent_id");
    }

    #[test]
    fn dataflow_empty_group_fails() {
        let json5 = node_config_with_dataflow(r#"{ group: "empty_loop" }"#);
        let result = NodeConfigParser::from_content(&json5);
        let Error::Parsing(ParsingError::DataflowEmptyGroup { group }) = result.unwrap_err() else {
            panic!("expected DataflowEmptyGroup error");
        };
        assert_eq!(group, "empty_loop");
    }

    #[test]
    fn dataflow_duplicate_group_name_fails() {
        let json5 = r#"{
            schema_version: 1,
            manifest: { name: "test_node", tag: "0.1.0", language: "rust" },
            build: { start_cmd: ["./test"] },
            interfaces: {
                exposes: {
                    topics: [
                        { name: "topic_a", message_format: { x: "f64" } },
                        { name: "topic_b", message_format: { y: "f64" } }
                    ]
                },
                dataflow: [
                    { group: "same_name", publishes: ["topic_a"] },
                    { group: "same_name", publishes: ["topic_b"] }
                ]
            }
        }"#;
        let result = NodeConfigParser::from_content(json5);
        let Error::Parsing(ParsingError::DataflowDuplicateGroup { group }) = result.unwrap_err()
        else {
            panic!("expected DataflowDuplicateGroup error");
        };
        assert_eq!(group, "same_name");
    }
}
