use crate::error::ParsingError;
use serde::{Deserialize, Serialize};
use std::convert::TryFrom;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NodeConfig {
    pub manifest: Manifest,
    #[serde(default)]
    pub config: NodeRuntimeConfig,
    #[serde(default)]
    pub instances: Vec<NodeInstance>,
    #[serde(default)]
    pub interfaces: Interfaces,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resources: Option<Resources>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub logging: Option<Logging>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diagnostics: Option<Diagnostics>,
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

/// Validated namespace. Same as Name but allows '/'.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Namespace(String);

impl Namespace {
    pub fn new<S: Into<String>>(s: S) -> Result<Self, ParsingError> {
        Self::try_from(s.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    fn is_valid_char(c: char) -> bool {
        c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-' || c == '/'
    }
}

impl TryFrom<String> for Namespace {
    type Error = ParsingError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        if value.is_empty() {
            return Err(ParsingError::InvalidNamespace(
                "Namespace cannot be empty".to_string(),
            ));
        }
        if value.chars().all(Namespace::is_valid_char) {
            return Ok(Namespace(value));
        }
        Err(ParsingError::InvalidNamespace(value))
    }
}

impl From<Namespace> for String {
    fn from(v: Namespace) -> Self {
        v.0
    }
}

// NodeInfo is not part of the new schema; manifest/config/instances carry this information.

// A flexible value to hold arbitrary JSON5 content
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(untagged)]
pub enum AnyValue {
    #[default]
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    String(String),
    Array(Vec<AnyValue>),
    Object(std::collections::BTreeMap<String, AnyValue>),
}

// Node parameters with open-ended structure
pub type NodeParameters = std::collections::BTreeMap<String, AnyValue>;

// Common wrapper for dynamic message formats in topics/services
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct MessageFormat(pub std::collections::BTreeMap<String, AnyValue>);

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct Exposes {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub topics: Option<Vec<ExposedTopic>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub services: Option<Vec<ExposedService>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub actions: Option<Vec<Action>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct SubscribesTo {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub topics: Option<Vec<SubscribedTopic>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub services: Option<Vec<SubscribedService>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub actions: Option<Vec<Action>>,
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
    #[serde(default, rename = "type")]
    pub topic_type: String,
    #[serde(default)]
    pub qos_profile: QoSProfile,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message_format: Option<MessageFormat>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct ExposedService {
    #[serde(default, rename = "type")]
    pub service_type: String,
    #[serde(default)]
    pub qos_profile: QoSProfile,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message_format: Option<MessageFormat>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct SubscribedTopic {
    #[serde(default, rename = "type")]
    pub topic_type: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub namespace: String,
    #[serde(default)]
    pub callback: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub optional: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct SubscribedService {
    #[serde(default, rename = "type")]
    pub service_type: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub namespace: String,
    #[serde(default)]
    pub callback: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub optional: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct Action {
    #[serde(default, rename = "type")]
    pub action_type: String,
    #[serde(default)]
    pub name: String,
    // For the moment, actions are undecided/unfinished
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
    pub file_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_file_size_mb: Option<u32>,
    #[serde(default)]
    pub format: LogFormat,
}

impl Default for Logging {
    fn default() -> Self {
        Self {
            min_level: default_log_level(),
            file_path: None,
            max_file_size_mb: None,
            format: LogFormat::default(),
        }
    }
}

// Default value functions
fn default_version() -> String {
    "0.1.0".to_string()
}

fn default_log_level() -> String {
    "info".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct Diagnostics {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub publish_rate_hz: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub health_checks: Option<Vec<String>>,
}

/// Supported template types
#[derive(Debug, Clone, Default)]
pub enum ConfigTemplateType {
    RootNode,
    #[default]
    SimpleNode,
    FullNode,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub name: Name,
    #[serde(default = "default_version")]
    pub version: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tags: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct NodeRuntimeConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_root: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auto_start: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub respawn: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub respawn_delay: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub startup_type: Option<StartupType>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NodeInstance {
    pub namespace: Namespace,
    #[serde(default)]
    pub parameters: NodeParameters,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct Interfaces {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exposes: Option<Exposes>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subscribes_to: Option<SubscribesTo>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum StartupType {
    #[default]
    Thread,
    Fork,
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
    fn namespace_validation() {
        assert!(Namespace::new("/").is_ok());
        assert!(Namespace::new("/robot").is_ok());
        assert!(Namespace::new("/robot/camera_v1").is_ok());

        assert!(Namespace::new("").is_err()); // empty not permitted
        assert!(Namespace::new("/Robot").is_err()); // capital
        assert!(Namespace::new("/robot$cam").is_err()); // special
    }
}
