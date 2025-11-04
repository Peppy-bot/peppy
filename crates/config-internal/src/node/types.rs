use crate::{common::NodeParameters, config::SchemaVersion, error::ParsingError};
use serde::{
    Deserialize, Serialize,
    de::{self, Deserializer, MapAccess, Visitor},
};
use std::{
    convert::TryFrom,
    fmt::{self, Display, Formatter},
};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NodeConfig {
    pub schema_version: SchemaVersion,
    pub manifest: Manifest,
    #[serde(default)]
    pub config: NodeRuntimeConfig,
    #[serde(default)]
    pub parameters: NodeParameters,
    #[serde(default)]
    pub interfaces: Interfaces,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resources: Option<Resources>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub logging: Option<Logging>,
}

/// Validated node name. Lowercase letters, digits, '_' and '-' only.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Name(String);

impl Name {
    pub fn new<S: Into<String>>(s: S) -> Result<Self, ParsingError> {
        Self::try_from(s.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    fn is_valid_char(c: char) -> bool {
        c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-'
    }
}

impl TryFrom<String> for Name {
    type Error = ParsingError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        if value.is_empty() {
            return Err(ParsingError::InvalidName(
                "Name cannot be empty".to_string(),
            ));
        }
        if value.chars().all(Name::is_valid_char) {
            return Ok(Name(value));
        }
        Err(ParsingError::InvalidName(value))
    }
}

impl From<Name> for String {
    fn from(v: Name) -> Self {
        v.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CallbackNameError {
    Empty,
    InvalidStart(char),
    InvalidChar(char),
}

impl Display for CallbackNameError {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            CallbackNameError::Empty => write!(f, "callback name cannot be empty"),
            CallbackNameError::InvalidStart(ch) => write!(
                f,
                "callback name must start with an ASCII letter or '_' but found `{}`",
                ch.escape_default()
            ),
            CallbackNameError::InvalidChar(ch) => write!(
                f,
                "callback name may only contain ASCII letters, digits, or '_' but found `{}`",
                ch.escape_default()
            ),
        }
    }
}

impl std::error::Error for CallbackNameError {}

// NodeInfo is not part of the new schema; manifest/config/instances carry this information.

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum TypeToken {
    Bool,
    #[serde(alias = "str")]
    String,
    Bytes,
    Time,
    U8,
    U16,
    U32,
    U64,
    I8,
    I16,
    I32,
    I64,
    F32,
    F64,
}
// Derives above keep serde logic concise; `TypeToken` handles mapping of known strings.

// Common wrapper for dynamic message formats in topics/services
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct MessageFormat(pub std::collections::BTreeMap<String, SchemaType>);

// Schema types used inside MessageFormat
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum SchemaType {
    Type(TypeToken),
    Array(ArraySchema),
    Object(ObjectSchema),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ArraySchema {
    #[serde(rename = "type")]
    pub kind: ArrayKind,
    #[serde(deserialize_with = "deserialize_array_items")]
    pub items: Box<SchemaType>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub length: Option<usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ArrayKind {
    Array,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ObjectSchema {
    #[serde(rename = "type")]
    pub kind: ObjectKind,
    #[serde(default, flatten)]
    pub fields: std::collections::BTreeMap<String, SchemaType>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ObjectKind {
    Object,
}

fn deserialize_array_items<'de, D>(deserializer: D) -> Result<Box<SchemaType>, D::Error>
where
    D: Deserializer<'de>,
{
    let schema = SchemaType::deserialize(deserializer)?;
    match schema {
        SchemaType::Type(_) => Ok(Box::new(schema)),
        SchemaType::Array(_) | SchemaType::Object(_) => Err(de::Error::custom(
            "nested arrays or objects are not supported inside array schemas",
        )),
    }
}

impl<'de> Deserialize<'de> for ObjectSchema {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_map(ObjectSchemaVisitor)
    }
}

struct ObjectSchemaVisitor;

impl<'de> Visitor<'de> for ObjectSchemaVisitor {
    type Value = ObjectSchema;

    fn expecting(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter.write_str("an object schema definition with a type and primitive fields")
    }

    fn visit_map<M>(self, mut map: M) -> Result<Self::Value, M::Error>
    where
        M: MapAccess<'de>,
    {
        let mut kind: Option<ObjectKind> = None;
        let mut fields = std::collections::BTreeMap::<String, SchemaType>::new();

        while let Some(key) = map.next_key::<String>()? {
            if key == "type" {
                if kind.is_some() {
                    return Err(de::Error::duplicate_field("type"));
                }
                let value: ObjectKind = map.next_value()?;
                kind = Some(value);
            } else {
                let value: SchemaType = map.next_value()?;
                match value {
                    SchemaType::Type(_) => {
                        if fields.insert(key.clone(), value).is_some() {
                            return Err(de::Error::custom(format!(
                                "duplicate object field `{}`",
                                key
                            )));
                        }
                    }
                    SchemaType::Array(_) | SchemaType::Object(_) => {
                        return Err(de::Error::custom(format!(
                            "nested arrays or objects are not supported for field `{}`",
                            key
                        )));
                    }
                }
            }
        }

        let kind = kind.ok_or_else(|| de::Error::missing_field("type"))?;
        Ok(ObjectSchema { kind, fields })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct Exposes {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub topics: Option<Vec<ExposedTopic>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub services: Option<Vec<ExposedService>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub actions: Option<Vec<ExposedAction>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct SubscribesTo {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub topics: Option<Vec<SubscribedTopic>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub services: Option<Vec<SubscribedService>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub actions: Option<Vec<SubscribedAction>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum QoSProfile {
    #[default]
    Standard,
    Reliable,
    SensorData,
    Critical,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct ExposedTopic {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub qos_profile: QoSProfile,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message_format: Option<MessageFormat>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct ExposedService {
    #[serde(default)]
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request_message_format: Option<MessageFormat>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response_message_format: Option<MessageFormat>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct ExposedAction {
    #[serde(default)]
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub goal_service: Option<ActionServiceEndpoint>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub feedback_topic: Option<ActionTopicEndpoint>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result_service: Option<ActionServiceEndpoint>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SubscribedTopic {
    #[serde(deserialize_with = "deserialize_subscribed_topic_node")]
    pub node: String,
    #[serde(deserialize_with = "deserialize_subscribed_topic_name")]
    pub name: String,
    #[serde(default)]
    pub tag: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SubscribedService {
    #[serde(default)]
    pub node: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub tag: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SubscribedAction {
    #[serde(default)]
    pub node: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub tag: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActionServiceEndpoint {
    #[serde(skip_serializing_if = "Option::is_none", rename = "type")]
    pub service_type: Option<String>,
    #[serde(default = "default_action_service_qos_profile")]
    pub qos_profile: QoSProfile,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub accept_message_format: Option<MessageFormat>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response_message_format: Option<MessageFormat>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

impl Default for ActionServiceEndpoint {
    fn default() -> Self {
        Self {
            service_type: None,
            qos_profile: default_action_service_qos_profile(),
            accept_message_format: None,
            response_message_format: None,
            name: None,
        }
    }
}

fn deserialize_subscribed_topic_node<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: Deserializer<'de>,
{
    deserialize_non_empty_identifier(deserializer, "SubscribedTopic.node")
}

fn deserialize_subscribed_topic_name<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: Deserializer<'de>,
{
    deserialize_non_empty_identifier(deserializer, "SubscribedTopic.name")
}

fn deserialize_non_empty_identifier<'de, D>(
    deserializer: D,
    label: &'static str,
) -> Result<String, D::Error>
where
    D: Deserializer<'de>,
{
    let raw = String::deserialize(deserializer)?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(de::Error::custom(format!("{label} cannot be empty")));
    }
    if !trimmed.chars().any(|ch| ch.is_ascii_alphanumeric()) {
        return Err(de::Error::custom(format!(
            "{label} must contain at least one alphanumeric character"
        )));
    }
    Ok(trimmed.to_string())
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct ActionTopicEndpoint {
    #[serde(skip_serializing_if = "Option::is_none", rename = "type")]
    pub topic_type: Option<String>,
    #[serde(default)]
    pub qos_profile: QoSProfile,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message_format: Option<MessageFormat>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

fn default_action_service_qos_profile() -> QoSProfile {
    QoSProfile::Reliable
}

#[derive(Default, Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Resources {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_memory_mb: Option<u32>,
}

#[derive(Default, Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum LogFormat {
    #[default]
    Text,
    Json,
}

impl From<String> for LogFormat {
    fn from(s: String) -> Self {
        match s.as_str() {
            "json" => LogFormat::Json,
            _ => LogFormat::Text, // Default to Text for any other value
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Logging {
    #[serde(default = "default_log_level")]
    pub min_level: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_file_size_mb: Option<u32>,
    #[serde(default)]
    pub format: LogFormat,
}

impl Default for Logging {
    fn default() -> Self {
        Self {
            min_level: default_log_level(),
            file_name: None,
            max_file_size_mb: None,
            format: LogFormat::default(),
        }
    }
}

// Default value functions
fn default_log_level() -> String {
    "info".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub name: Name,
    pub tag: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub labels: Option<Vec<String>>,
    // Command to launch the node, e.g., ["cargo", "run", "--release"]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub launch_cmd: Option<Vec<String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct NodeRuntimeConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auto_start: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub respawn: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub respawn_delay: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct Interfaces {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exposes: Option<Exposes>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subscribes_to: Option<SubscribesTo>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_validation() {
        assert!(Name::new("node").is_ok());
        assert!(Name::new("my_node-1").is_ok());

        assert!(Name::new("").is_err()); // empty not permitted
        assert!(Name::new("Node").is_err()); // capital
        assert!(Name::new("node/").is_err()); // slash not allowed
        assert!(Name::new("node@!").is_err()); // specials not allowed
    }

    #[test]
    fn subscribed_topic_requires_non_empty_node_and_name() {
        let valid = r#"{ node: "uvc_camera", name: "stream" }"#;
        let topic: SubscribedTopic =
            serde_json5::from_str(valid).expect("valid topic should parse");
        assert_eq!(topic.node, "uvc_camera");
        assert_eq!(topic.name, "stream");

        let missing_node = r#"{ node: "", name: "stream" }"#;
        assert!(serde_json5::from_str::<SubscribedTopic>(missing_node).is_err());

        let missing_name = r#"{ node: "uvc_camera", name: "" }"#;
        assert!(serde_json5::from_str::<SubscribedTopic>(missing_name).is_err());

        let whitespace_only = r#"{ node: "   ", name: "stream" }"#;
        assert!(serde_json5::from_str::<SubscribedTopic>(whitespace_only).is_err());

        let punctuation_only = r#"{ node: "--", name: "stream" }"#;
        assert!(serde_json5::from_str::<SubscribedTopic>(punctuation_only).is_err());

        let missing_field = r#"{ node: "uvc_camera" }"#;
        assert!(serde_json5::from_str::<SubscribedTopic>(missing_field).is_err());

        let trimmed = r#"{ node: " uvc_camera ", name: " stream " }"#;
        let topic: SubscribedTopic =
            serde_json5::from_str(trimmed).expect("whitespace should be trimmed");
        assert_eq!(topic.node, "uvc_camera");
        assert_eq!(topic.name, "stream");
    }

    #[test]
    fn type_tokens_in_message_format() {
        // A snippet similar to the camera stream message_format
        let json5 = r#"{
            header: { type: "object", stamp: "time", frame_id: "u32" },
            encoding: "string",
            width: "u32",
            height: "u32",
            image: { type: "array", items: "u8", length: 3 }
        }"#;

        let mf: MessageFormat = serde_json5::from_str(json5).unwrap();

        // header.stamp
        match mf.0.get("header").unwrap() {
            SchemaType::Object(object) => {
                assert_eq!(object.kind, ObjectKind::Object);
                let map = &object.fields;
                assert!(matches!(
                    map.get("stamp"),
                    Some(SchemaType::Type(TypeToken::Time))
                ));
                assert!(matches!(
                    map.get("frame_id"),
                    Some(SchemaType::Type(TypeToken::U32))
                ));
            }
            _ => panic!("header should be an object"),
        }

        // encoding
        assert!(matches!(
            mf.0.get("encoding"),
            Some(SchemaType::Type(TypeToken::String))
        ));
        // dimensions
        assert!(matches!(
            mf.0.get("width"),
            Some(SchemaType::Type(TypeToken::U32))
        ));
        assert!(matches!(
            mf.0.get("height"),
            Some(SchemaType::Type(TypeToken::U32))
        ));

        // image array of tokens
        match mf.0.get("image").unwrap() {
            SchemaType::Array(array) => {
                assert_eq!(array.kind, ArrayKind::Array);
                assert!(matches!(&*array.items, SchemaType::Type(TypeToken::U8)));
                assert_eq!(array.length, Some(3));
            }
            _ => panic!("image should be an array"),
        }

        // Round-trip: ensure tokens serialize back to canonical strings
        let out = serde_json5::to_string(&mf).unwrap();
        assert!(out.contains("\"u8\""));
        assert!(out.contains("\"u32\""));
        assert!(out.contains("\"time\""));
        assert!(out.contains("\"string\""));
        assert!(out.contains("\"type\":\"array\""));
    }

    #[test]
    fn object_schema_requires_type_field() {
        let json5 = r#"{
            header: { stamp: "time", frame_id: "u32" }
        }"#;

        let parsed: Result<MessageFormat, _> = serde_json5::from_str(json5);
        assert!(parsed.is_err(), "object without type should fail parsing");
    }

    #[test]
    fn array_schema_requires_type_field() {
        let json5 = r#"{
            image: { items: "u8", length: 3 }
        }"#;

        let parsed: Result<MessageFormat, _> = serde_json5::from_str(json5);
        assert!(parsed.is_err(), "array without type should fail parsing");
    }

    #[test]
    fn object_schema_rejects_nested_array() {
        let json5 = r#"{
            header: {
                type: "object",
                nested: { type: "array", items: "u8" }
            }
        }"#;

        let parsed: Result<MessageFormat, _> = serde_json5::from_str(json5);
        assert!(parsed.is_err(), "object fields cannot contain arrays");
    }

    #[test]
    fn object_schema_rejects_nested_object() {
        let json5 = r#"{
            header: {
                type: "object",
                nested: { type: "object", field: "u8" }
            }
        }"#;

        let parsed: Result<MessageFormat, _> = serde_json5::from_str(json5);
        assert!(parsed.is_err(), "object fields cannot contain objects");
    }

    #[test]
    fn array_schema_rejects_nested_object() {
        let json5 = r#"{
            image: { type: "array", items: { type: "object", field: "u8" } }
        }"#;

        let parsed: Result<MessageFormat, _> = serde_json5::from_str(json5);
        assert!(parsed.is_err(), "array items cannot contain objects");
    }

    #[test]
    fn array_schema_rejects_nested_array() {
        let json5 = r#"{
            image: { type: "array", items: { type: "array", items: "u8" } }
        }"#;

        let parsed: Result<MessageFormat, _> = serde_json5::from_str(json5);
        assert!(parsed.is_err(), "array items cannot contain arrays");
    }
}
