use crate::error::ParsingError;
use serde::{
    Deserialize, Serialize,
    de::{self, Deserializer},
    ser::{self, Serializer},
};
use std::{
    convert::TryFrom,
    fmt::{self, Display, Formatter},
    path::{Path, PathBuf},
};

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

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NodeSource {
    Local(PathBuf),
    Git(GitRemoteSpec),
    Http(String),
}

impl NodeSource {
    const FILE_SCHEME: &'static str = "file://";

    pub fn is_local(&self) -> bool {
        matches!(self, NodeSource::Local(_))
    }

    pub fn as_local_path(&self) -> Option<&Path> {
        match self {
            NodeSource::Local(path) => Some(path.as_path()),
            _ => None,
        }
    }

    pub fn git(&self) -> Option<&GitRemoteSpec> {
        match self {
            NodeSource::Git(spec) => Some(spec),
            _ => None,
        }
    }

    pub fn http(&self) -> Option<&str> {
        match self {
            NodeSource::Http(url) => Some(url.as_str()),
            _ => None,
        }
    }

    pub fn from_str(value: &str) -> Result<Self, ParsingError> {
        Self::from_string(value.to_owned())
    }

    fn from_string(value: String) -> Result<Self, ParsingError> {
        let trimmed = value.trim();
        if trimmed.is_empty() {
            return Err(ParsingError::InvalidDeploymentSource(
                "source cannot be empty".to_string(),
            ));
        }

        if let Some(rest) = trimmed.strip_prefix(Self::FILE_SCHEME) {
            if rest.is_empty() {
                return Err(ParsingError::InvalidDeploymentSource(
                    "file path cannot be empty".to_string(),
                ));
            }
            return Ok(NodeSource::Local(PathBuf::from(rest)));
        }

        if Self::is_http_url(trimmed) && !Self::looks_like_git(trimmed) {
            return Ok(NodeSource::Http(trimmed.to_owned()));
        }

        let spec = Self::parse_git_spec(trimmed)?;
        Ok(NodeSource::Git(spec))
    }

    fn parse_git_spec(value: &str) -> Result<GitRemoteSpec, ParsingError> {
        let (repo_raw, path_raw) = value
            .split_once("::")
            .map(|(repo, path)| (repo.trim(), Some(path.trim())))
            .unwrap_or_else(|| (value.trim(), None));

        if repo_raw.is_empty() {
            return Err(ParsingError::InvalidDeploymentSource(
                "git repo cannot be empty".to_string(),
            ));
        }

        let path = path_raw
            .filter(|segment| !segment.is_empty())
            .map(|segment| segment.trim_start_matches('/').to_owned());

        Ok(GitRemoteSpec {
            repo: repo_raw.to_owned(),
            path,
        })
    }

    fn is_http_url(value: &str) -> bool {
        value.starts_with("http://") || value.starts_with("https://")
    }

    fn looks_like_git(value: &str) -> bool {
        value.ends_with(".git")
            || value.contains(".git/")
            || value.contains(".git?")
            || value.starts_with("git@")
            || value.starts_with("ssh://")
            || value.starts_with("git://")
    }
}

impl Serialize for NodeSource {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match self {
            NodeSource::Local(path) => {
                let path_str = path
                    .to_str()
                    .ok_or_else(|| ser::Error::custom("local path is not valid UTF-8"))?;
                serializer.serialize_str(&format!("{}{}", Self::FILE_SCHEME, path_str))
            }
            NodeSource::Git(spec) => serializer.serialize_str(&spec.as_remote()),
            NodeSource::Http(url) => serializer.serialize_str(url),
        }
    }
}

impl<'de> Deserialize<'de> for NodeSource {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum RawNodeSource {
            String(String),
            Git { git: RawGitSpec },
        }

        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct RawGitSpec {
            repo: String,
            #[serde(default, skip_serializing_if = "Option::is_none")]
            path: Option<String>,
        }

        match RawNodeSource::deserialize(deserializer)? {
            RawNodeSource::String(value) => {
                NodeSource::from_string(value).map_err(de::Error::custom)
            }
            RawNodeSource::Git { git } => {
                if git.repo.trim().is_empty() {
                    return Err(de::Error::custom(ParsingError::InvalidDeploymentSource(
                        "git repo cannot be empty".to_string(),
                    )));
                }

                Ok(NodeSource::Git(GitRemoteSpec {
                    repo: git.repo,
                    path: git.path.and_then(|segment| {
                        let trimmed = segment.trim();
                        if trimmed.is_empty() {
                            None
                        } else {
                            Some(trimmed.trim_start_matches('/').to_owned())
                        }
                    }),
                }))
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GitRemoteSpec {
    pub repo: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
}

impl GitRemoteSpec {
    pub fn as_remote(&self) -> String {
        match &self.path {
            Some(path) if !path.is_empty() => format!("{}::{}", self.repo, path),
            _ => self.repo.clone(),
        }
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub optional: Option<bool>,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub optional: Option<bool>,
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
    pub callback: Option<CallbackName>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub feedback_callback: Option<CallbackName>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub results_callback: Option<CallbackName>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub optional: Option<bool>,
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

/// Supported template types
#[derive(Debug, Clone, Default)]
pub enum ConfigTemplateType {
    RootNode,
    #[default]
    SimpleNode,
    FullNode,
}

fn bool_is_false(value: &bool) -> bool {
    !*value
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub name: Name,
    pub tag: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub labels: Option<Vec<String>>,
    // Root nodes orchestrate deployments instead of running a command
    #[serde(default, skip_serializing_if = "bool_is_false")]
    pub is_root_node: bool,
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

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Deployment {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<NodeSource>,
    pub tag: String,
    #[serde(default, skip_serializing_if = "bool_is_false")]
    pub optional: bool,
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
    use std::path::Path;

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
        assert!(action.callback.is_none());
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
    fn node_source_validation() {
        let local: NodeSource = serde_json5::from_str("\"file:///tmp/node\"").unwrap();
        assert!(matches!(local, NodeSource::Local(ref path) if path == Path::new("/tmp/node")));

        let http: NodeSource =
            serde_json5::from_str("\"https://nodes.peppy.bot/nodes/camera\"").unwrap();
        assert!(
            matches!(http, NodeSource::Http(ref url) if url == "https://nodes.peppy.bot/nodes/camera")
        );

        let git: NodeSource = serde_json5::from_str(
            "{ git: { repo: \"https://github.com/Peppy/uvc_camera.git\", path: \"configs/camera\" } }",
        )
        .unwrap();
        assert!(matches!(
            git,
            NodeSource::Git(GitRemoteSpec { repo, path })
                if repo == "https://github.com/Peppy/uvc_camera.git" && path.as_deref() == Some("configs/camera")
        ));

        let defaulted: Deployment = serde_json5::from_str(
            r#"{
                name: "controller",
                tag: "0.1.0",
                instances: [{ namespace: "/" }]
            }"#,
        )
        .unwrap();
        assert!(defaulted.source.is_none());

        let empty: Result<NodeSource, _> = serde_json5::from_str("\"\"");
        let err = empty.expect_err("deserializing an empty node source should fail");
        let ParsingError::InvalidDeploymentSource(msg) = err.into() else {
            panic!("expected invalid deployment source error");
        };
        assert_eq!(msg, "source cannot be empty");
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
