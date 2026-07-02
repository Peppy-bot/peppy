use crate::{
    node::{EmittedTopic, ExposedAction, ExposedService, Name},
    schema::PeppySchema,
};
use serde::{
    Deserialize, Serialize,
    de::{self, Deserializer},
};
use std::collections::HashSet;

/// Reject any `peppy_schema` value other than `interface/v1` so a node or
/// launcher document can't slip through `PeppyInterfaceParser`.
fn deserialize_interface_v1_schema<'de, D>(deserializer: D) -> Result<PeppySchema, D::Error>
where
    D: Deserializer<'de>,
{
    PeppySchema::deserialize_expecting(deserializer, PeppySchema::InterfaceV1)
}

/// A reusable contract describing the topics, services, and actions a node
/// claims to expose. Interface documents are stand-alone JSON5 files identified
/// by `peppy_schema: "interface/v1"`; nodes reference them by name/tag to
/// declare conformance.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct PeppyInterface {
    #[serde(deserialize_with = "deserialize_interface_v1_schema")]
    pub peppy_schema: PeppySchema,
    pub manifest: Manifest,
    pub interfaces: Interfaces,
}

/// Identity of an interface document. Interfaces do not have build/runtime
/// concerns, so the manifest is narrower than a node manifest: `depends_on`
/// is rejected because an interface is a passive contract. `labels` are
/// allowed as descriptive metadata to help discovery and filtering.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub name: Name,
    #[serde(deserialize_with = "deserialize_tag")]
    pub tag: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub labels: Option<Vec<String>>,
}

/// Enforce the repo-ID tag contract on the manifest tag at parse time, reusing
/// the shared rules from `repo_node_id` so a `name`/`tag` pair from an interface
/// document matches node dependencies the same way everywhere.
fn deserialize_tag<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: Deserializer<'de>,
{
    let tag = String::deserialize(deserializer)?;
    crate::internal::repo_node_id::validate_repo_node_tag(&tag, "tag")
        .map_err(de::Error::custom)?;
    Ok(tag)
}

/// The body of an interface document. Each section is a flat list of items
/// — there is no `emits`/`consumes` split because an interface describes the
/// provider side only. Conformance for the consumer side is checked separately
/// against the node's declared interfaces.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Interfaces {
    #[serde(
        default,
        skip_serializing_if = "Vec::is_empty",
        deserialize_with = "deserialize_topics"
    )]
    pub topics: Vec<EmittedTopic>,
    #[serde(
        default,
        skip_serializing_if = "Vec::is_empty",
        deserialize_with = "deserialize_services"
    )]
    pub services: Vec<ExposedService>,
    #[serde(
        default,
        skip_serializing_if = "Vec::is_empty",
        deserialize_with = "deserialize_actions"
    )]
    pub actions: Vec<ExposedAction>,
}

fn deserialize_topics<'de, D>(deserializer: D) -> Result<Vec<EmittedTopic>, D::Error>
where
    D: Deserializer<'de>,
{
    let items = Vec::<EmittedTopic>::deserialize(deserializer)?;
    validate_named_items(items.iter().map(|t| t.name.as_str()), "topic")
        .map_err(de::Error::custom)?;
    Ok(items)
}

fn deserialize_services<'de, D>(deserializer: D) -> Result<Vec<ExposedService>, D::Error>
where
    D: Deserializer<'de>,
{
    let items = Vec::<ExposedService>::deserialize(deserializer)?;
    validate_named_items(items.iter().map(|s| s.name.as_str()), "service")
        .map_err(de::Error::custom)?;
    Ok(items)
}

fn deserialize_actions<'de, D>(deserializer: D) -> Result<Vec<ExposedAction>, D::Error>
where
    D: Deserializer<'de>,
{
    let items = Vec::<ExposedAction>::deserialize(deserializer)?;
    validate_named_items(items.iter().map(|a| a.name.as_str()), "action")
        .map_err(de::Error::custom)?;
    Ok(items)
}

/// Rejects empty/whitespace names and duplicates within a single list. Both
/// states would otherwise survive parsing because the underlying item types
/// default `name` to `""` for ergonomics in node configs.
pub(crate) fn validate_named_items<'a>(
    names: impl Iterator<Item = &'a str>,
    kind: &'static str,
) -> Result<(), String> {
    let mut seen = HashSet::new();
    for (index, name) in names.enumerate() {
        if name.trim().is_empty() {
            return Err(format!(
                "interface {kind} at index {index} has an empty `name`"
            ));
        }
        if !seen.insert(name) {
            return Err(format!("duplicate interface {kind} name: `{name}`"));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::node::{ArrayKind, MessageFormat, QoSProfile, SchemaType, TypeToken};

    /// The depth-camera example from the schema doc parses end-to-end and
    /// exposes every field the way the user wrote it.
    #[test]
    fn parses_depth_camera_example() {
        let json5 = r#"{
            peppy_schema: "interface/v1",
            manifest: {
                name: "depth_camera",
                tag: "v1"
            },
            interfaces: {
                topics: [
                    {
                        name: "video_stream",
                        qos_profile: "sensor_data",
                        message_format: {
                            header: {
                                $type: "object",
                                stamp: "time",
                                frame_id: "u32",
                            },
                            encoding: "string",
                            width: "u32",
                            height: "u32",
                            frame: {
                                $type: "array",
                                $items: "u8",
                                $length: 4
                            },
                        },
                    }
                ],
                services: [
                    {
                        name: "video_stream_info",
                        response_message_format: {
                            width: "u32",
                            height: "u32",
                            frames_per_second: "u8",
                            encoding: "string",
                        }
                    },
                    {
                        name: "set_contrast",
                        request_message_format: {
                            value: "i32",
                        },
                        response_message_format: {
                            success: "bool",
                            message: "string",
                            current_value: "i32",
                        },
                    }
                ]
            }
        }"#;

        let parsed: PeppyInterface =
            serde_json5::from_str(json5).expect("depth_camera example should parse");

        assert_eq!(parsed.peppy_schema, PeppySchema::InterfaceV1);
        assert_eq!(parsed.manifest.name.as_str(), "depth_camera");
        assert_eq!(parsed.manifest.tag, "v1");

        assert_eq!(parsed.interfaces.topics.len(), 1);
        let topic = &parsed.interfaces.topics[0];
        assert_eq!(topic.name, "video_stream");
        assert_eq!(topic.qos_profile, QoSProfile::SensorData);
        let mf = topic.message_format.as_ref().expect("message_format set");
        assert!(matches!(
            mf.0.get("encoding"),
            Some(SchemaType::Type(TypeToken::String))
        ));
        let SchemaType::Array(frame) = mf.0.get("frame").unwrap() else {
            panic!("frame should be an array");
        };
        assert_eq!(frame.kind, ArrayKind::Array);
        assert_eq!(frame.length, Some(4));

        assert_eq!(parsed.interfaces.services.len(), 2);
        let svc = &parsed.interfaces.services[0];
        assert_eq!(svc.name, "video_stream_info");
        assert!(svc.request_message_format.is_none());
        assert!(svc.response_message_format.is_some());

        let set_contrast = &parsed.interfaces.services[1];
        assert_eq!(set_contrast.name, "set_contrast");
        assert!(set_contrast.request_message_format.is_some());
        assert!(set_contrast.response_message_format.is_some());

        assert!(parsed.interfaces.actions.is_empty());
    }

    #[test]
    fn minimal_interface_with_only_manifest_parses() {
        let json5 = r#"{
            peppy_schema: "interface/v1",
            manifest: { name: "empty_iface", tag: "v1" },
            interfaces: {}
        }"#;

        let parsed: PeppyInterface = serde_json5::from_str(json5).expect("should parse");
        assert!(parsed.interfaces.topics.is_empty());
        assert!(parsed.interfaces.services.is_empty());
        assert!(parsed.interfaces.actions.is_empty());
    }

    #[test]
    fn actions_can_be_declared() {
        let json5 = r#"{
            peppy_schema: "interface/v1",
            manifest: { name: "arm", tag: "v1" },
            interfaces: {
                actions: [
                    {
                        name: "move_arm",
                        goal_service: {
                            request_message_format: { x: "f64" },
                            response_message_format: { accepted: "bool" },
                        },
                        result_service: {
                            response_message_format: { success: "bool" },
                        }
                    }
                ]
            }
        }"#;

        let parsed: PeppyInterface = serde_json5::from_str(json5).expect("should parse");
        assert_eq!(parsed.interfaces.actions.len(), 1);
        let action = &parsed.interfaces.actions[0];
        assert_eq!(action.name, "move_arm");
        assert!(action.goal_service.is_some());
        assert!(action.result_service.is_some());
        assert!(action.feedback_topic.is_none());
    }

    /// The schema field is the source of truth — a node-shaped document
    /// must not be accepted by the interface parser, even if no
    /// interface-specific field is present.
    #[test]
    fn rejects_wrong_schema_tag() {
        let json5 = r#"{
            peppy_schema: "node/v1",
            manifest: { name: "x", tag: "v1" },
            interfaces: {}
        }"#;
        let err =
            serde_json5::from_str::<PeppyInterface>(json5).expect_err("node/v1 must be rejected");
        assert!(
            err.to_string().contains("interface/v1"),
            "error should mention expected schema, got: {err}"
        );
    }

    #[test]
    fn rejects_unknown_top_level_fields() {
        let json5 = r#"{
            peppy_schema: "interface/v1",
            manifest: { name: "x", tag: "v1" },
            interfaces: {},
            execution: { language: "rust" }
        }"#;
        assert!(serde_json5::from_str::<PeppyInterface>(json5).is_err());
    }

    #[test]
    fn rejects_unknown_interfaces_fields() {
        let json5 = r#"{
            peppy_schema: "interface/v1",
            manifest: { name: "x", tag: "v1" },
            interfaces: { mystery: [] }
        }"#;
        assert!(serde_json5::from_str::<PeppyInterface>(json5).is_err());
    }

    /// Interface manifests intentionally drop `depends_on`: an interface
    /// is a passive contract and cannot depend on other nodes.
    #[test]
    fn rejects_manifest_depends_on() {
        let json5 = r#"{
            peppy_schema: "interface/v1",
            manifest: {
                name: "x",
                tag: "v1",
                depends_on: { nodes: [] }
            },
            interfaces: {}
        }"#;
        assert!(serde_json5::from_str::<PeppyInterface>(json5).is_err());
    }

    /// `labels` are descriptive metadata on the manifest — accepted so
    /// catalog tooling can filter interfaces (e.g., `vendor`, `domain`)
    /// without changing the contract itself.
    #[test]
    fn accepts_manifest_labels() {
        let json5 = r#"{
            peppy_schema: "interface/v1",
            manifest: {
                name: "x",
                tag: "v1",
                labels: ["a", "b"]
            },
            interfaces: {}
        }"#;
        let parsed: PeppyInterface =
            serde_json5::from_str(json5).expect("labels should be accepted");
        assert_eq!(
            parsed.manifest.labels.as_deref(),
            Some(["a".to_string(), "b".to_string()].as_slice())
        );
    }

    #[test]
    fn rejects_invalid_manifest_name() {
        let json5 = r#"{
            peppy_schema: "interface/v1",
            manifest: { name: "bad/name", tag: "v1" },
            interfaces: {}
        }"#;
        assert!(serde_json5::from_str::<PeppyInterface>(json5).is_err());
    }

    /// The manifest tag must satisfy the same repo-ID rules as node
    /// dependencies (`repo_node_id::validate_repo_node_tag`); a tag that does
    /// not start with an ASCII letter is rejected at parse time.
    #[test]
    fn rejects_invalid_manifest_tag() {
        let json5 = r#"{
            peppy_schema: "interface/v1",
            manifest: { name: "x", tag: "1bad" },
            interfaces: {}
        }"#;
        let err = serde_json5::from_str::<PeppyInterface>(json5)
            .expect_err("invalid tag must be rejected");
        assert!(
            err.to_string().contains("must start with an ASCII letter"),
            "error: {err}"
        );
    }

    #[test]
    fn rejects_empty_topic_name() {
        let json5 = r#"{
            peppy_schema: "interface/v1",
            manifest: { name: "x", tag: "v1" },
            interfaces: {
                topics: [ { qos_profile: "standard" } ]
            }
        }"#;
        let err = serde_json5::from_str::<PeppyInterface>(json5)
            .expect_err("empty name must be rejected");
        assert!(err.to_string().contains("empty"), "error: {err}");
    }

    #[test]
    fn rejects_duplicate_topic_names() {
        let json5 = r#"{
            peppy_schema: "interface/v1",
            manifest: { name: "x", tag: "v1" },
            interfaces: {
                topics: [
                    { name: "stream" },
                    { name: "stream" }
                ]
            }
        }"#;
        let err = serde_json5::from_str::<PeppyInterface>(json5)
            .expect_err("duplicate topic name must be rejected");
        assert!(err.to_string().contains("duplicate"), "error: {err}");
    }

    #[test]
    fn rejects_duplicate_service_names() {
        let json5 = r#"{
            peppy_schema: "interface/v1",
            manifest: { name: "x", tag: "v1" },
            interfaces: {
                services: [
                    { name: "svc" },
                    { name: "svc" }
                ]
            }
        }"#;
        assert!(serde_json5::from_str::<PeppyInterface>(json5).is_err());
    }

    #[test]
    fn rejects_duplicate_action_names() {
        let json5 = r#"{
            peppy_schema: "interface/v1",
            manifest: { name: "x", tag: "v1" },
            interfaces: {
                actions: [
                    { name: "act" },
                    { name: "act" }
                ]
            }
        }"#;
        assert!(serde_json5::from_str::<PeppyInterface>(json5).is_err());
    }

    /// A topic with the same name as a service is allowed: the daemon
    /// addresses them through distinct namespaces, so an interface can
    /// reasonably emit `/status` while also exposing a `status` service.
    #[test]
    fn allows_same_name_across_kinds() {
        let json5 = r#"{
            peppy_schema: "interface/v1",
            manifest: { name: "x", tag: "v1" },
            interfaces: {
                topics: [ { name: "ping" } ],
                services: [ { name: "ping" } ],
                actions: [ { name: "ping" } ]
            }
        }"#;
        let parsed: PeppyInterface =
            serde_json5::from_str(json5).expect("cross-kind name collisions are allowed");
        assert_eq!(parsed.interfaces.topics.len(), 1);
        assert_eq!(parsed.interfaces.services.len(), 1);
        assert_eq!(parsed.interfaces.actions.len(), 1);
    }

    /// Round-tripping survives canonicalization — the parsed AST serializes
    /// back to a form that re-parses to the same AST.
    #[test]
    fn round_trips_through_serde() {
        let original = PeppyInterface {
            peppy_schema: PeppySchema::InterfaceV1,
            manifest: Manifest {
                name: Name::new("camera").unwrap(),
                tag: "v123".to_string(),
                labels: Some(vec!["vendor".to_string(), "sensor".to_string()]),
            },
            interfaces: Interfaces {
                topics: vec![EmittedTopic {
                    name: "stream".to_string(),
                    qos_profile: QoSProfile::SensorData,
                    message_format: Some(MessageFormat(
                        [("width".to_string(), SchemaType::Type(TypeToken::U32))]
                            .into_iter()
                            .collect(),
                    )),
                }],
                services: vec![],
                actions: vec![],
            },
        };

        let serialized = serde_json5::to_string(&original).expect("serialize");
        let reparsed: PeppyInterface = serde_json5::from_str(&serialized).expect("re-parse");
        assert_eq!(original, reparsed);
    }
}
