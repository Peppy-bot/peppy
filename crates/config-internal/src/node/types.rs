use crate::{common::NodeParameters, error::ParsingError};
use serde::{
    Deserialize, Serialize,
    de::{self, Deserializer},
};
use std::{
    convert::TryFrom,
    fmt::{self, Display, Formatter},
};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NodeConfig {
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

/// Validated callback name shared across languages (Rust/Python/Java-friendly).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
#[serde(transparent)]
pub struct CallbackName(String);

impl CallbackName {
    pub fn new<S: Into<String>>(value: S) -> Result<Self, CallbackNameError> {
        let value = value.into();
        Self::validate(&value)?;
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    fn validate(value: &str) -> Result<(), CallbackNameError> {
        let mut chars = value.chars();
        let first = chars.next().ok_or(CallbackNameError::Empty)?;
        if !Self::is_valid_start(first) {
            return Err(CallbackNameError::InvalidStart(first));
        }
        for ch in chars {
            if !Self::is_valid_continue(ch) {
                return Err(CallbackNameError::InvalidChar(ch));
            }
        }
        Ok(())
    }

    fn is_valid_start(ch: char) -> bool {
        ch == '_' || ch.is_ascii_alphabetic()
    }

    fn is_valid_continue(ch: char) -> bool {
        ch == '_' || ch.is_ascii_alphanumeric()
    }
}

impl<'de> Deserialize<'de> for CallbackName {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw = String::deserialize(deserializer)?;
        CallbackName::new(raw).map_err(de::Error::custom)
    }
}

impl Display for CallbackName {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
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
    Object(std::collections::BTreeMap<String, SchemaType>),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ArraySchema {
    #[serde(rename = "type")]
    pub kind: ArrayKind,
    pub items: Box<SchemaType>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub length: Option<usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ArrayKind {
    Array,
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
    pub qos_profile: QoSProfile,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message_format: Option<MessageFormat>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
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
    #[serde(default)]
    pub node: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub tag: String,
    pub callback: CallbackName,
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
    pub callback: CallbackName,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub feedback_callback: Option<CallbackName>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub results_callback: Option<CallbackName>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActionServiceEndpoint {
    #[serde(skip_serializing_if = "Option::is_none", rename = "type")]
    pub service_type: Option<String>,
    #[serde(default = "default_action_service_qos_profile")]
    pub qos_profile: QoSProfile,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message_format: Option<MessageFormat>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

impl Default for ActionServiceEndpoint {
    fn default() -> Self {
        Self {
            service_type: None,
            qos_profile: default_action_service_qos_profile(),
            message_format: None,
            name: None,
        }
    }
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
    fn callback_name_validation() {
        assert!(CallbackName::new("on_brain_command_received").is_ok());
        assert!(CallbackName::new("handleEvent").is_ok());
        assert!(CallbackName::new("_internal_handler").is_ok());

        assert!(matches!(
            CallbackName::new(""),
            Err(CallbackNameError::Empty)
        ));
        assert!(matches!(
            CallbackName::new("1bad"),
            Err(CallbackNameError::InvalidStart('1'))
        ));
        assert!(matches!(
            CallbackName::new("bad-name"),
            Err(CallbackNameError::InvalidChar('-'))
        ));
        assert!(matches!(
            CallbackName::new("bad!name"),
            Err(CallbackNameError::InvalidChar('!'))
        ));
    }

    #[test]
    fn callback_name_deserialize_rejects_invalid() {
        let parsed: CallbackName = serde_json5::from_str("\"onBrainCommand\"").unwrap();
        assert_eq!(parsed.as_str(), "onBrainCommand");

        let err: Result<CallbackName, _> = serde_json5::from_str("\"1brainCommand\"");
        assert!(err.is_err());
    }

    #[test]
    fn subscribed_action_accepts_all_callbacks() {
        let json = r#"{
            node: "brain",
            name: "move_arm",
            tag: "0.1.0",
            feedback_callback: "onMoveArmFeedback",
            results_callback: "onMoveArmResult"
        }"#;

        let action: SubscribedAction = serde_json5::from_str(json).unwrap();
        assert_eq!(
            action.feedback_callback.as_ref().unwrap().as_str(),
            "onMoveArmFeedback"
        );
        assert_eq!(
            action.results_callback.as_ref().unwrap().as_str(),
            "onMoveArmResult"
        );
    }

    #[test]
    fn type_tokens_in_message_format() {
        // A snippet similar to the camera stream message_format
        let json5 = r#"{
            header: { stamp: "time", frame_id: "u32" },
            encoding: "string",
            width: "u32",
            height: "u32",
            image: { type: "array", items: "u8", length: 3 }
        }"#;

        let mf: MessageFormat = serde_json5::from_str(json5).unwrap();

        // header.stamp
        match mf.0.get("header").unwrap() {
            SchemaType::Object(map) => {
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
}
