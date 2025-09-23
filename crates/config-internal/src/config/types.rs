use crate::error::Error;
use crate::error::ParsingError;
use core::fmt;
use serde::{Deserialize, Serialize};
use std::convert::TryFrom;
use std::str::FromStr;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NodeConfig {
    pub manifest: Manifest,
    #[serde(default)]
    pub config: NodeRuntimeConfig,
    #[serde(default)]
    pub parameters: NodeParameters,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deployments: Option<Vec<Deployment>>, // Root node only feature
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

// A flexible value to hold arbitrary JSON5 content (runtime values only)
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(untagged)]
pub enum AnyType {
    #[default]
    Null,
    Bool(bool),
    /// A plain string value
    String(String),
    Array(Vec<AnyType>),
    Object(std::collections::BTreeMap<String, AnyType>),

    // Numeric values: prefer signed, then unsigned, then float
    Int(i64),
    UInt(u64),
    Float(f64),
}

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

// Node parameters with open-ended structure
pub type NodeParameters = std::collections::BTreeMap<String, AnyType>;

// Common wrapper for dynamic message formats in topics/services
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct MessageFormat(pub std::collections::BTreeMap<String, SchemaType>);

// Schema types used inside MessageFormat
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum SchemaType {
    Type(TypeToken),
    Array(Vec<SchemaType>),
    Object(std::collections::BTreeMap<String, SchemaType>),
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
    #[serde(default)]
    pub node: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub tag: String,
    #[serde(default)]
    pub callback: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub optional: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct SubscribedService {
    #[serde(default)]
    pub node: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub tag: String,
    #[serde(default)]
    pub callback: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub optional: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct SubscribedAction {
    #[serde(default)]
    pub name: String,
    // For the moment, actions are undecided/unfinished
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct ExposedAction {
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
    pub tag: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub labels: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
    // Command to launch the node, e.g., ["cargo", "run", "--release"]
    pub launch_cmd: Vec<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Language {
    Python,
    #[default]
    Rust,
}

impl fmt::Display for Language {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Language::Python => write!(f, "python"),
            Language::Rust => write!(f, "rust"),
        }
    }
}

impl FromStr for Language {
    type Err = Error;

    fn from_str(s: &str) -> crate::error::Result<Self> {
        match s {
            "python" => Ok(Language::Python),
            "rust" => Ok(Language::Rust),
            _ => Err(Error::UnsupportedLanguage),
        }
    }
}

impl TryFrom<&str> for Language {
    type Error = Error;

    fn try_from(s: &str) -> crate::error::Result<Self> {
        s.parse()
    }
}

impl From<Language> for &'static str {
    fn from(lang: Language) -> Self {
        match lang {
            Language::Python => "python",
            Language::Rust => "rust",
        }
    }
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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(try_from = "String", into = "String")]
pub struct DeploymentSource(String);

impl DeploymentSource {
    pub fn as_str(&self) -> &str {
        &self.0
    }
    pub fn is_local(&self) -> bool {
        self.0.starts_with("file://")
    }
}

impl TryFrom<String> for DeploymentSource {
    type Error = ParsingError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        if value.is_empty() {
            return Err(ParsingError::InvalidDeploymentSource(
                "source cannot be empty".to_string(),
            ));
        }
        Ok(DeploymentSource(value))
    }
}

impl From<DeploymentSource> for String {
    fn from(source: DeploymentSource) -> Self {
        source.0
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Deployment {
    pub name: String,
    pub source: DeploymentSource,
    pub tag: String,
    pub instances: Vec<DeploymentInstance>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeploymentInstance {
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

    #[test]
    fn deployment_source_validation() {
        let local: DeploymentSource = serde_json5::from_str("\"file:///tmp/node\"").unwrap();
        assert!(local.is_local());

        let remote: DeploymentSource =
            serde_json5::from_str("\"https://github.com/Peppy/uvc_camera.git\"").unwrap();
        assert_eq!(remote.as_str(), "https://github.com/Peppy/uvc_camera.git");

        let empty: Result<DeploymentSource, _> = serde_json5::from_str("\"\"");
        let err: ParsingError = empty
            .expect_err("deserializing an empty deployment source should fail")
            .into();
        assert!(matches!(
            err,
            ParsingError::InvalidDeploymentSource(ref msg) if msg == "source cannot be empty"
        ));
    }

    #[test]
    fn type_tokens_in_message_format() {
        // A snippet similar to the camera stream message_format
        let json5 = r#"{
            header: { stamp: "time", frame_id: "u32" },
            encoding: "string",
            width: "u32",
            height: "u32",
            image: ["u8", "u8", "u8"]
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
            SchemaType::Array(v) => {
                assert_eq!(v.len(), 3);
                assert!(
                    v.iter()
                        .all(|e| matches!(e, SchemaType::Type(TypeToken::U8)))
                );
            }
            _ => panic!("image should be an array"),
        }

        // Round-trip: ensure tokens serialize back to canonical strings
        let out = serde_json5::to_string(&mf).unwrap();
        assert!(out.contains("\"u8\""));
        assert!(out.contains("\"u32\""));
        assert!(out.contains("\"time\""));
        assert!(out.contains("\"string\""));
    }
}
