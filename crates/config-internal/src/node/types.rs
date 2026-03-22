use crate::{
    common::{AnyType, NodeArguments, resolve_parameter_path},
    config::SchemaVersion,
    error::ParsingError,
};
use indexmap::IndexMap;
use serde::{
    Deserialize, Serialize,
    de::{self, Deserializer, MapAccess, Visitor},
};
use std::{
    convert::TryFrom,
    fmt::{self, Display, Formatter},
    str::FromStr,
};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PeppygenLanguage {
    #[default]
    Rust,
    Python,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, clap::ValueEnum)]
pub enum Toolchain {
    Cargo,
    #[default]
    Uv,
}

impl fmt::Display for Toolchain {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Toolchain::Uv => write!(f, "uv"),
            Toolchain::Cargo => write!(f, "cargo"),
        }
    }
}

impl FromStr for Toolchain {
    type Err = ParsingError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s.eq_ignore_ascii_case("cargo") {
            Ok(Toolchain::Cargo)
        } else if s.eq_ignore_ascii_case("uv") {
            Ok(Toolchain::Uv)
        } else {
            Err(ParsingError::InvalidToolchain(s.to_owned()))
        }
    }
}

impl Toolchain {
    pub fn map_to_language(&self) -> PeppygenLanguage {
        match self {
            Toolchain::Cargo => PeppygenLanguage::Rust,
            Toolchain::Uv => PeppygenLanguage::Python,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NodeConfig {
    pub schema_version: SchemaVersion,
    pub manifest: Manifest,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub process: Option<Process>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub container: Option<ContainerConfig>,
    // TODO: Rename `parameters` to `arguments` when it's given in a NodeConfig, the `parameters` name is only used in DeploymentInstance
    #[serde(default)]
    pub parameters: NodeArguments,
    #[serde(default)]
    pub interfaces: Interfaces,
}

/// Validated node name. Lowercase letters, digits, '_' and '-' only.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Name(String);

use crate::consts::ALLOWED_CONFIG_CHARS;

impl Name {
    pub fn new<S: Into<String>>(s: S) -> Result<Self, ParsingError> {
        Self::try_from(s.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    fn is_valid_char(c: char) -> bool {
        ALLOWED_CONFIG_CHARS.contains(c)
    }
}

impl TryFrom<String> for Name {
    type Error = ParsingError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        if value.is_empty() {
            return Err(ParsingError::Structured(
                crate::error::StructuredError::EmptyName.json5_message(),
            ));
        }
        if value.chars().all(Name::is_valid_char) {
            return Ok(Name(value));
        }
        let err = crate::error::StructuredError::InvalidName {
            name: value,
            allowed: ALLOWED_CONFIG_CHARS.to_string(),
        };
        Err(ParsingError::Structured(err.json5_message()))
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
    #[serde(alias = "float")]
    F32,
    #[serde(alias = "double")]
    F64,
}
// Derives above keep serde logic concise; `TypeToken` handles mapping of known strings.

// Common wrapper for dynamic message formats in topics/services
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct MessageFormat(pub IndexMap<String, SchemaType>);

fn is_false(value: &bool) -> bool {
    !*value
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum InterfaceKind {
    Topic,
    Service,
    Action,
}

impl std::fmt::Display for InterfaceKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            InterfaceKind::Topic => write!(f, "topic"),
            InterfaceKind::Service => write!(f, "service"),
            InterfaceKind::Action => write!(f, "action"),
        }
    }
}

impl std::str::FromStr for InterfaceKind {
    type Err = String;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s {
            "topic" => Ok(InterfaceKind::Topic),
            "service" => Ok(InterfaceKind::Service),
            "action" => Ok(InterfaceKind::Action),
            other => Err(format!("unknown interface kind: {other}")),
        }
    }
}

// Schema types used inside MessageFormat
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum SchemaType {
    Type(TypeToken),
    Primitive(PrimitiveSchema),
    Array(ArraySchema),
    Object(ObjectSchema),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct PrimitiveSchema {
    #[serde(rename = "$type")]
    pub kind: TypeToken,
    #[serde(rename = "$optional", default, skip_serializing_if = "is_false")]
    pub optional: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ArraySchema {
    #[serde(rename = "$type")]
    pub kind: ArrayKind,
    #[serde(rename = "$items", deserialize_with = "deserialize_array_items")]
    pub items: Box<SchemaType>,
    #[serde(rename = "$length", default, skip_serializing_if = "Option::is_none")]
    pub length: Option<usize>,
    #[serde(rename = "$optional", default, skip_serializing_if = "is_false")]
    pub optional: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ArrayKind {
    Array,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ObjectSchema {
    #[serde(rename = "$type")]
    pub kind: ObjectKind,
    #[serde(default, flatten)]
    pub fields: IndexMap<String, SchemaType>,
    #[serde(rename = "$optional", default, skip_serializing_if = "is_false")]
    pub optional: bool,
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
        SchemaType::Type(_) | SchemaType::Primitive(_) => Ok(Box::new(schema)),
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
        formatter.write_str("an object schema definition with a $type and primitive fields")
    }

    fn visit_map<M>(self, mut map: M) -> Result<Self::Value, M::Error>
    where
        M: MapAccess<'de>,
    {
        let mut kind: Option<ObjectKind> = None;
        let mut optional = false;
        let mut fields = IndexMap::<String, SchemaType>::new();

        while let Some(key) = map.next_key::<String>()? {
            match key.as_str() {
                "$type" => {
                    if kind.is_some() {
                        return Err(de::Error::duplicate_field("$type"));
                    }
                    let value: ObjectKind = map.next_value()?;
                    kind = Some(value);
                }
                "$optional" => {
                    optional = map.next_value()?;
                }
                _ => {
                    let value: SchemaType = map.next_value()?;
                    match value {
                        SchemaType::Type(_) | SchemaType::Primitive(_) => {
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
        }

        let kind = kind.ok_or_else(|| de::Error::missing_field("$type"))?;
        Ok(ObjectSchema {
            kind,
            fields,
            optional,
        })
    }
}

impl SchemaType {
    pub fn is_optional(&self) -> bool {
        match self {
            SchemaType::Type(_) => false,
            SchemaType::Primitive(schema) => schema.optional,
            SchemaType::Array(schema) => schema.optional,
            SchemaType::Object(schema) => schema.optional,
        }
    }

    pub fn as_type_token(&self) -> Option<&TypeToken> {
        match self {
            SchemaType::Type(token) => Some(token),
            SchemaType::Primitive(schema) => Some(&schema.kind),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct TopicInterfaces {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub emits: Option<Vec<EmittedTopic>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub consumes: Option<Vec<ConsumedTopic>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ServiceInterfaces {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exposes: Option<Vec<ExposedService>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub consumes: Option<Vec<ConsumedService>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ActionInterfaces {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exposes: Option<Vec<ExposedAction>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub consumes: Option<Vec<ConsumedAction>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum QoSProfile {
    SensorData,
    #[default]
    Standard,
    Reliable,
    Critical,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct EmittedTopic {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub qos_profile: QoSProfile,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message_format: Option<MessageFormat>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ExposedService {
    #[serde(default)]
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request_message_format: Option<MessageFormat>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response_message_format: Option<MessageFormat>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct LinkedConsumedTopic {
    #[serde(deserialize_with = "deserialize_consumed_topic_local_node_id")]
    pub local_node_id: String,
    #[serde(deserialize_with = "deserialize_consumed_topic_name")]
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ExternalConsumedTopic {
    #[serde(deserialize_with = "deserialize_consumed_topic_name")]
    pub name: String,
    pub message_format: MessageFormat,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum ConsumedTopic {
    Linked(LinkedConsumedTopic),
    External(ExternalConsumedTopic),
}

impl ConsumedTopic {
    pub fn name(&self) -> &str {
        match self {
            Self::Linked(t) => &t.name,
            Self::External(t) => &t.name,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ConsumedService {
    #[serde(deserialize_with = "deserialize_consumed_service_local_node_id")]
    pub local_node_id: String,
    #[serde(default)]
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ConsumedAction {
    #[serde(deserialize_with = "deserialize_consumed_action_local_node_id")]
    pub local_node_id: String,
    #[serde(default)]
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ActionServiceEndpoint {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request_message_format: Option<MessageFormat>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response_message_format: Option<MessageFormat>,
    #[serde(default = "default_action_service_qos_profile")]
    pub qos_profile: QoSProfile,
}

impl Default for ActionServiceEndpoint {
    fn default() -> Self {
        Self {
            qos_profile: default_action_service_qos_profile(),
            request_message_format: None,
            response_message_format: None,
        }
    }
}

fn deserialize_consumed_topic_local_node_id<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: Deserializer<'de>,
{
    deserialize_non_empty_identifier(deserializer, "ConsumedTopic.local_node_id")
}

fn deserialize_consumed_topic_name<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: Deserializer<'de>,
{
    deserialize_non_empty_identifier(deserializer, "ConsumedTopic.name")
}

fn deserialize_consumed_service_local_node_id<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: Deserializer<'de>,
{
    deserialize_non_empty_identifier(deserializer, "ConsumedService.local_node_id")
}

fn deserialize_consumed_action_local_node_id<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: Deserializer<'de>,
{
    deserialize_non_empty_identifier(deserializer, "ConsumedAction.local_node_id")
}

fn deserialize_non_empty_identifier<'de, D>(
    deserializer: D,
    label: &'static str,
) -> Result<String, D::Error>
where
    D: Deserializer<'de>,
{
    let raw = String::deserialize(deserializer)?;
    validate_non_empty_identifier(&raw, label).map_err(de::Error::custom)
}

fn validate_non_empty_identifier(raw: &str, label: &'static str) -> Result<String, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(format!("{label} cannot be empty"));
    }
    if !trimmed.chars().any(|ch| ch.is_ascii_alphanumeric()) {
        return Err(format!(
            "{label} must contain at least one alphanumeric character"
        ));
    }
    Ok(trimmed.to_string())
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
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

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NodeDependency {
    pub name: Name,
    pub tag: String,
    pub local_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DependsOn {
    pub nodes: Vec<NodeDependency>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub name: Name,
    pub tag: String,
    pub language: PeppygenLanguage,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub labels: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub depends_on: Option<DependsOn>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Process {
    // Command to run when right before the node is added to the node stack
    pub add_cmd: Option<Vec<String>>,
    // Command to launch the node, e.g., ["./target/release/my_node"]
    pub start_cmd: Vec<String>,
}

/// Top-level system directories that cannot be used as mount sources.
///
/// These paths are rejected by Lima 2.0+ as guest mountPoints, and using them
/// as bind-mount sources in Apptainer is almost always a mistake (mounting an
/// entire system directory into a container). Users should use subdirectories
/// instead (e.g., `/tmp/my_app` rather than `/tmp`).
///
/// NOTE: This list is duplicated in `containers-internal/src/apptainer/lima.rs`
/// (which cannot depend on this crate). Keep both in sync.
const BLOCKED_MOUNT_PATHS: &[&str] = &[
    "/", "/bin", "/dev", "/etc", "/home", "/opt", "/sbin", "/tmp", "/usr", "/var",
];

/// Format the blocked mount paths as a comma-separated display string.
fn blocked_mount_paths_display() -> String {
    BLOCKED_MOUNT_PATHS.join(", ")
}

/// Check whether a path is a blocked top-level system mount.
///
/// Only exact top-level matches are blocked — subdirectories like `/tmp/my_app`
/// are allowed. Also handles macOS `/private/X` equivalents (e.g., `/private/tmp`
/// maps to `/tmp`).
pub fn is_blocked_mount_source(path: &str) -> bool {
    if BLOCKED_MOUNT_PATHS.contains(&path) {
        return true;
    }
    // macOS: /private/tmp -> /tmp, /private/var -> /var
    if let Some(stripped) = path.strip_prefix("/private") {
        return BLOCKED_MOUNT_PATHS.contains(&stripped);
    }
    false
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContainerConfig {
    pub def_file: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mount_paths: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub apptainer_build_extra_args: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub apptainer_run_extra_args: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lima_shell_extra_args: Option<Vec<String>>,
}

/// Extract all `${parameters:...}` references from a mount path string.
///
/// Returns the dot-path portion of each reference (e.g., `"device_path"` or
/// `"video.device_path"`).
pub fn extract_parameter_refs(mount_path: &str) -> Vec<&str> {
    let mut refs = Vec::new();
    let mut remaining = mount_path;
    while let Some(start) = remaining.find("${parameters:") {
        let after_prefix = &remaining[start + "${parameters:".len()..];
        if let Some(end) = after_prefix.find('}') {
            refs.push(&after_prefix[..end]);
            remaining = &after_prefix[end + 1..];
        } else {
            break;
        }
    }
    refs
}

impl ContainerConfig {
    /// Validate mount_paths, rejecting top-level system directories as mount sources.
    ///
    /// Mount paths whose source contains `${parameters:...}` are skipped because
    /// the actual host path is not known until runtime.
    ///
    /// Returns `Err((invalid_path, blocked_list_display))` on the first invalid path found.
    pub fn validate(&self) -> Result<(), (String, String)> {
        let Some(mount_paths) = &self.mount_paths else {
            return Ok(());
        };
        for mount in mount_paths {
            // Parse "host_path:container_path[:options]" — only validate the source (host) path.
            let src = mount.split(':').next().unwrap_or(mount);
            // Skip blocked-path check when the source contains parameter references
            // (the actual path is resolved at runtime).
            if src.contains("${parameters:") {
                continue;
            }
            if is_blocked_mount_source(src) {
                return Err((mount.clone(), blocked_mount_paths_display()));
            }
        }
        Ok(())
    }

    /// Validate that `${parameters:...}` references in mount_paths point to existing
    /// string-typed parameters in the schema.
    ///
    /// Returns `Err((ref_path, reason))` on the first invalid reference found.
    pub fn validate_parameter_refs(
        &self,
        parameters: &NodeArguments,
    ) -> Result<(), (String, String)> {
        let Some(mount_paths) = &self.mount_paths else {
            return Ok(());
        };
        for mount in mount_paths {
            for ref_path in extract_parameter_refs(mount) {
                match resolve_parameter_path(parameters, ref_path) {
                    None => {
                        return Err((
                            ref_path.to_owned(),
                            "parameter not found in schema".to_owned(),
                        ));
                    }
                    Some(AnyType::String(type_name)) if type_name == "string" => {
                        // Valid — string-typed parameter.
                    }
                    Some(type_spec) => {
                        return Err((
                            ref_path.to_owned(),
                            format!(
                                "parameter must be of type \"string\", found \"{}\"",
                                type_spec_display(type_spec)
                            ),
                        ));
                    }
                }
            }
        }
        Ok(())
    }
}

/// Human-readable display for a parameter type spec.
fn type_spec_display(spec: &AnyType) -> &str {
    match spec {
        AnyType::String(s) => s.as_str(),
        AnyType::Object(_) => "object",
        AnyType::Array(_) => "array",
        _ => "unknown",
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Interfaces {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub topics: Option<TopicInterfaces>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub services: Option<ServiceInterfaces>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub actions: Option<ActionInterfaces>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_validation() {
        assert!(Name::new("node").is_ok());
        assert!(Name::new("my_node-1").is_ok());

        assert!(Name::new("").is_err()); // empty not permitted
        assert!(Name::new("Node").is_ok()); // capital letters allowed
        assert!(Name::new("node/").is_err()); // slash not allowed
        assert!(Name::new("node@!").is_err()); // specials not allowed
    }

    #[test]
    fn consumed_topic_linked_local_node_id_is_required() {
        let valid = r#"{ local_node_id: "uvc_camera", name: "video_stream" }"#;
        let topic: ConsumedTopic = serde_json5::from_str(valid).expect("valid topic should parse");
        let ConsumedTopic::Linked(LinkedConsumedTopic {
            local_node_id,
            name,
        }) = &topic
        else {
            panic!("expected Linked variant");
        };
        assert_eq!(local_node_id, "uvc_camera");
        assert_eq!(name, "video_stream");

        let empty_local_node_id = r#"{ local_node_id: "", name: "video_stream" }"#;
        assert!(serde_json5::from_str::<ConsumedTopic>(empty_local_node_id).is_err());

        let missing_name = r#"{ local_node_id: "uvc_camera", name: "" }"#;
        assert!(serde_json5::from_str::<ConsumedTopic>(missing_name).is_err());

        let whitespace_only = r#"{ local_node_id: "   ", name: "video_stream" }"#;
        assert!(serde_json5::from_str::<ConsumedTopic>(whitespace_only).is_err());

        let punctuation_only = r#"{ local_node_id: "--", name: "video_stream" }"#;
        assert!(serde_json5::from_str::<ConsumedTopic>(punctuation_only).is_err());

        let missing_name_field = r#"{ local_node_id: "uvc_camera" }"#;
        assert!(serde_json5::from_str::<ConsumedTopic>(missing_name_field).is_err());

        let trimmed = r#"{ local_node_id: " uvc_camera ", name: " video_stream " }"#;
        let topic: ConsumedTopic =
            serde_json5::from_str(trimmed).expect("whitespace should be trimmed");
        let ConsumedTopic::Linked(LinkedConsumedTopic {
            local_node_id,
            name,
        }) = &topic
        else {
            panic!("expected Linked variant");
        };
        assert_eq!(local_node_id, "uvc_camera");
        assert_eq!(name, "video_stream");
    }

    #[test]
    fn consumed_topic_external_requires_name_and_message_format() {
        let valid = r#"{ name: "cmd_vel", message_format: { linear_x: "f64", angular_z: "f64" } }"#;
        let topic: ConsumedTopic =
            serde_json5::from_str(valid).expect("valid external topic should parse");
        let ConsumedTopic::External(ExternalConsumedTopic {
            name,
            message_format,
        }) = &topic
        else {
            panic!("expected External variant, got: {:?}", topic);
        };
        assert_eq!(name, "cmd_vel");
        assert_eq!(message_format.0.len(), 2);
        assert_eq!(topic.name(), "cmd_vel");

        // name-only without message_format is an error (matches neither variant)
        let name_only = r#"{ name: "cmd_vel" }"#;
        assert!(
            serde_json5::from_str::<ConsumedTopic>(name_only).is_err(),
            "name-only (no local_node_id, no message_format) should fail"
        );

        // External with empty name should fail
        let empty_name = r#"{ name: "", message_format: { linear_x: "f64", angular_z: "f64" } }"#;
        assert!(serde_json5::from_str::<ConsumedTopic>(empty_name).is_err());
    }

    #[test]
    fn consumed_topic_mixed_linked_and_external() {
        let json = r#"[
            { local_node_id: "camera", name: "video_stream" },
            { name: "cmd_vel", message_format: { linear_x: "f64", angular_z: "f64" } }
        ]"#;
        let topics: Vec<ConsumedTopic> =
            serde_json5::from_str(json).expect("mixed array should parse");
        assert_eq!(topics.len(), 2);
        assert!(matches!(&topics[0], ConsumedTopic::Linked(_)));
        assert!(matches!(&topics[1], ConsumedTopic::External(_)));
        assert_eq!(topics[0].name(), "video_stream");
        assert_eq!(topics[1].name(), "cmd_vel");
    }

    #[test]
    fn consumed_topic_rejects_unknown_fields() {
        // Linked with extra message_format should fail (not silently drop it)
        let linked_with_extra = r#"{
            local_node_id: "camera",
            name: "video_stream",
            message_format: { x: "f64" }
        }"#;
        assert!(
            serde_json5::from_str::<ConsumedTopic>(linked_with_extra).is_err(),
            "linked topic with extra message_format should be rejected"
        );

        // External with extra local_node_id should fail (not silently drop it)
        let external_with_extra = r#"{
            local_node_id: "camera",
            name: "cmd_vel",
            message_format: { linear_x: "f64" }
        }"#;
        assert!(
            serde_json5::from_str::<ConsumedTopic>(external_with_extra).is_err(),
            "external topic with extra local_node_id should be rejected"
        );
    }

    #[test]
    fn consumed_service_local_node_id_is_required() {
        let with_local_node_id = r#"{ local_node_id: "uvc_camera", name: "enable_camera" }"#;
        let service: ConsumedService = serde_json5::from_str(with_local_node_id)
            .expect("service with local_node_id should parse");
        assert_eq!(service.local_node_id, "uvc_camera");

        let trimmed = r#"{ local_node_id: "  uvc_camera  ", name: "enable_camera" }"#;
        let service: ConsumedService =
            serde_json5::from_str(trimmed).expect("whitespace should be trimmed");
        assert_eq!(service.local_node_id, "uvc_camera");

        let without_local_node_id = r#"{ name: "enable_camera" }"#;
        assert!(serde_json5::from_str::<ConsumedService>(without_local_node_id).is_err());

        let blank_local_node_id = r#"{ local_node_id: "   ", name: "enable_camera" }"#;
        assert!(serde_json5::from_str::<ConsumedService>(blank_local_node_id).is_err());
    }

    #[test]
    fn action_service_endpoint_accepts_request_and_accept_keys() {
        let request_version = r#"
        {
            request_message_format: { value: "u32" }
        }
        "#;
        let endpoint: ActionServiceEndpoint =
            serde_json5::from_str(request_version).expect("request field should parse");
        assert!(endpoint.request_message_format.is_some());

        let accept_version = r#"
        {
            request_message_format: { value: "u32" }
        }
        "#;
        let endpoint: ActionServiceEndpoint =
            serde_json5::from_str(accept_version).expect("accept field should parse");
        assert!(endpoint.request_message_format.is_some());
    }

    #[test]
    fn exposed_service_accepts_request_and_accept_keys() {
        let request_version = r#"
        {
            request_message_format: { value: "u32" }
        }
        "#;
        let service: ExposedService =
            serde_json5::from_str(request_version).expect("request field should parse");
        assert!(service.request_message_format.is_some());

        let accept_version = r#"
        {
            request_message_format: { value: "u32" }
        }
        "#;
        let service: ExposedService =
            serde_json5::from_str(accept_version).expect("accept field should parse");
        assert!(service.request_message_format.is_some());
    }

    #[test]
    fn type_tokens_in_message_format() {
        // A snippet similar to the camera stream message_format
        let json5 = r#"{
            header: { $type: "object", stamp: "time", frame_id: "u32" },
            encoding: "string",
            width: "u32",
            height: "u32",
            image: { $type: "array", $items: "u8", $length: 3 }
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
        assert!(out.contains("\"$type\":\"array\""));
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
            image: { $items: "u8", $length: 3 }
        }"#;

        let parsed: Result<MessageFormat, _> = serde_json5::from_str(json5);
        assert!(parsed.is_err(), "array without type should fail parsing");
    }

    #[test]
    fn object_schema_rejects_nested_array() {
        let json5 = r#"{
            header: {
                $type: "object",
                nested: { $type: "array", $items: "u8" }
            }
        }"#;

        let parsed: Result<MessageFormat, _> = serde_json5::from_str(json5);
        assert!(parsed.is_err(), "object fields cannot contain arrays");
    }

    #[test]
    fn object_schema_rejects_nested_object() {
        let json5 = r#"{
            header: {
                $type: "object",
                nested: { $type: "object", field: "u8" }
            }
        }"#;

        let parsed: Result<MessageFormat, _> = serde_json5::from_str(json5);
        assert!(parsed.is_err(), "object fields cannot contain objects");
    }

    #[test]
    fn array_schema_rejects_nested_object() {
        let json5 = r#"{
            image: { $type: "array", $items: { $type: "object", field: "u8" } }
        }"#;

        let parsed: Result<MessageFormat, _> = serde_json5::from_str(json5);
        assert!(parsed.is_err(), "array items cannot contain objects");
    }

    #[test]
    fn array_schema_rejects_nested_array() {
        let json5 = r#"{
            image: { $type: "array", $items: { $type: "array", $items: "u8" } }
        }"#;

        let parsed: Result<MessageFormat, _> = serde_json5::from_str(json5);
        assert!(parsed.is_err(), "array items cannot contain arrays");
    }

    #[test]
    fn manifest_with_depends_on() {
        let json5 = r#"{
            name: "slam",
            tag: "0.1.0",
            language: "rust",
            depends_on: {
                nodes: [
                    { name: "lidar_driver", tag: "0.1.0", local_id: "lidar" },
                    { name: "nav_system", tag: "0.1.0", local_id: "navigation" }
                ]
            }
        }"#;
        let manifest: Manifest = serde_json5::from_str(json5).expect("should parse");
        let deps = manifest.depends_on.expect("depends_on should be Some");
        assert_eq!(deps.nodes.len(), 2);
        assert_eq!(deps.nodes[0].name.as_str(), "lidar_driver");
        assert_eq!(deps.nodes[0].tag, "0.1.0");
        assert_eq!(deps.nodes[0].local_id, "lidar");
        assert_eq!(deps.nodes[1].name.as_str(), "nav_system");
        assert_eq!(deps.nodes[1].local_id, "navigation");
    }

    #[test]
    fn manifest_without_depends_on() {
        let json5 = r#"{
            name: "simple_node",
            tag: "0.1.0",
            language: "rust"
        }"#;
        let manifest: Manifest = serde_json5::from_str(json5).expect("should parse");
        assert!(manifest.depends_on.is_none());
    }

    #[test]
    fn depends_on_rejects_unknown_fields() {
        let json5 = r#"{
            name: "node",
            tag: "0.1.0",
            language: "rust",
            depends_on: {
                nodes: [{ name: "dep", tag: "0.1.0", local_id: "d", extra: "bad" }]
            }
        }"#;
        assert!(serde_json5::from_str::<Manifest>(json5).is_err());
    }
}
