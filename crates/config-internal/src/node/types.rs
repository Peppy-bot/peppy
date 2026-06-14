use crate::{
    common::{ParameterSchema, ParameterSpec, resolve_parameter_path, type_token_name},
    error::ParsingError,
    schema::PeppySchema,
};
use indexmap::IndexMap;
use serde::{
    Deserialize, Serialize,
    de::{self, Deserializer, MapAccess, Visitor},
};
use std::{
    convert::TryFrom,
    fmt::{self, Formatter},
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

/// Reject any `peppy_schema` value other than `node_v1` so a launcher
/// document that happens to share a node-compatible field set can't
/// slip through `NodeConfigParser`.
fn deserialize_node_v1_schema<'de, D>(deserializer: D) -> Result<PeppySchema, D::Error>
where
    D: Deserializer<'de>,
{
    PeppySchema::deserialize_expecting(deserializer, PeppySchema::NodeV1)
}

/// Fully resolved node configuration. A peppy node always has a `manifest`
/// and an `execution` block; `interfaces` is optional and defaults to empty
/// for nodes with no topics/services/actions.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NodeConfig {
    #[serde(deserialize_with = "deserialize_node_v1_schema")]
    pub peppy_schema: PeppySchema,
    pub manifest: Manifest,
    #[serde(default)]
    pub interfaces: Interfaces,
    pub execution: Execution,
}

/// Validated node name. Lowercase letters, digits, '_' and '-' only.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
#[serde(into = "String")]
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
            return Err(ParsingError::EmptyName);
        }
        if value.chars().all(Name::is_valid_char) {
            return Ok(Name(value));
        }
        Err(ParsingError::InvalidName(
            value,
            ALLOWED_CONFIG_CHARS.to_string(),
        ))
    }
}

impl<'de> Deserialize<'de> for Name {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        Name::try_from(s).map_err(|err| de::Error::custom(err.to_string()))
    }
}

impl From<Name> for String {
    fn from(v: Name) -> Self {
        v.0
    }
}

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
    if schema.is_optional() {
        return Err(de::Error::custom(
            "`$optional` is not supported on array items",
        ));
    }
    if matches!(schema, SchemaType::Array(_)) {
        return Err(de::Error::custom(
            "nested arrays (arrays of arrays) are not supported as array items",
        ));
    }
    Ok(Box::new(schema))
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
        formatter.write_str("an object schema definition with a $type and typed fields")
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
                    if value.is_optional() {
                        return Err(de::Error::custom(format!(
                            "`$optional` is not supported on nested field `{key}`"
                        )));
                    }
                    if fields.insert(key.clone(), value).is_some() {
                        return Err(de::Error::custom(format!("duplicate object field `{key}`")));
                    }
                }
            }
        }

        let kind = kind.unwrap_or(ObjectKind::Object);
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
pub struct ConsumedTopic {
    #[serde(deserialize_with = "deserialize_consumed_topic_link_id")]
    pub link_id: String,
    #[serde(deserialize_with = "deserialize_consumed_topic_name")]
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ConsumedService {
    #[serde(deserialize_with = "deserialize_consumed_service_link_id")]
    pub link_id: String,
    #[serde(default)]
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ConsumedAction {
    #[serde(deserialize_with = "deserialize_consumed_action_link_id")]
    pub link_id: String,
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

fn deserialize_consumed_topic_link_id<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: Deserializer<'de>,
{
    deserialize_non_empty_identifier(deserializer, "ConsumedTopic.link_id")
}

fn deserialize_consumed_topic_name<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: Deserializer<'de>,
{
    deserialize_non_empty_identifier(deserializer, "ConsumedTopic.name")
}

fn deserialize_consumed_service_link_id<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: Deserializer<'de>,
{
    deserialize_non_empty_identifier(deserializer, "ConsumedService.link_id")
}

fn deserialize_consumed_action_link_id<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: Deserializer<'de>,
{
    deserialize_non_empty_identifier(deserializer, "ConsumedAction.link_id")
}

fn deserialize_node_dependency_link_id<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: Deserializer<'de>,
{
    deserialize_non_empty_identifier(deserializer, "NodeDependency.link_id")
}

fn deserialize_interface_dependency_link_id<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: Deserializer<'de>,
{
    deserialize_non_empty_identifier(deserializer, "InterfaceDependency.link_id")
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
    #[serde(deserialize_with = "deserialize_node_dependency_link_id")]
    pub link_id: String,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub from_any: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InterfaceDependency {
    pub name: Name,
    pub tag: String,
    #[serde(deserialize_with = "deserialize_interface_dependency_link_id")]
    pub link_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub from_any: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DependsOn {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub nodes: Vec<NodeDependency>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub interfaces: Vec<InterfaceDependency>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Execution {
    pub language: PeppygenLanguage,
    #[serde(default)]
    pub parameters: ParameterSchema,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub build_cmd: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run_cmd: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub container: Option<ContainerConfig>,
}

/// Custom deserialization for [`Execution`] so a missing `language` field
/// produces a structured `MissingExecutionLanguage` error rather than the
/// generic serde "missing field" message.
impl<'de> Deserialize<'de> for Execution {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct RawExecution {
            language: Option<PeppygenLanguage>,
            #[serde(default)]
            parameters: ParameterSchema,
            build_cmd: Option<Vec<String>>,
            run_cmd: Option<Vec<String>>,
            container: Option<ContainerConfig>,
        }

        let raw = RawExecution::deserialize(deserializer)?;
        let language = raw.language.ok_or_else(|| {
            de::Error::custom(
                crate::error::StructuredError::MissingExecutionLanguage.json5_message(),
            )
        })?;
        Ok(Execution {
            language,
            parameters: raw.parameters,
            build_cmd: raw.build_cmd,
            run_cmd: raw.run_cmd,
            container: raw.container,
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub name: Name,
    pub tag: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub labels: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub depends_on: Option<DependsOn>,
}

// The blocked-mount-path list and predicate live in the sibling `mount_policy`
// module so this crate (config-parse-time validation) and `containers` (Lima YAML
// mutation) share one source of truth. Re-exported so
// `config::node::is_blocked_mount_source` stays part of this crate's public API.
pub use super::mount_policy::is_blocked_mount_source;

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
                return Err((
                    mount.clone(),
                    super::mount_policy::blocked_mount_paths_display(),
                ));
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
        parameters: &ParameterSchema,
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
                    Some(ParameterSpec::Primitive {
                        kind: TypeToken::String,
                        ..
                    }) => {
                        // Valid — string-typed parameter.
                    }
                    Some(spec) => {
                        return Err((
                            ref_path.to_owned(),
                            format!(
                                "parameter must be of type \"string\", found \"{}\"",
                                parameter_spec_display(spec)
                            ),
                        ));
                    }
                }
            }
        }
        Ok(())
    }
}

/// Human-readable display for a [`ParameterSpec`] used in mount-path error messages.
fn parameter_spec_display(spec: &ParameterSpec) -> &'static str {
    match spec {
        ParameterSpec::Primitive { kind, .. } => type_token_name(kind),
        ParameterSpec::Array { .. } => "array",
        ParameterSpec::Group(_) => "object",
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ConformsToItem {
    pub name: Name,
    pub tag: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub conforms_to: Option<Vec<ConformsToItem>>,
}

/// Puts a value into canonical form so that derived `PartialEq` becomes
/// order-independent: vecs are sorted by name, IndexMap keys are sorted
/// recursively, and `Some(default)` is collapsed to `None`.
trait Normalize {
    fn normalize(&mut self);

    fn normalized(mut self) -> Self
    where
        Self: Sized,
    {
        self.normalize();
        self
    }
}

fn normalize_schema_map(map: &mut IndexMap<String, SchemaType>) {
    for value in map.values_mut() {
        value.normalize();
    }
    map.sort_keys();
}

fn normalize_opt<T: Normalize>(opt: &mut Option<T>) {
    if let Some(inner) = opt.as_mut() {
        inner.normalize();
    }
}

fn normalize_opt_default<T: Normalize + Default + PartialEq>(opt: &mut Option<T>) {
    if let Some(inner) = opt.as_mut() {
        inner.normalize();
        let mut def = T::default();
        def.normalize();
        if *inner == def {
            *opt = None;
        }
    }
}

fn normalize_opt_vec<T: Normalize>(
    opt: &mut Option<Vec<T>>,
    cmp: impl Fn(&T, &T) -> std::cmp::Ordering,
) {
    if let Some(items) = opt.as_mut() {
        for item in items.iter_mut() {
            item.normalize();
        }
        items.sort_by(|a, b| cmp(a, b));
        if items.is_empty() {
            *opt = None;
        }
    }
}

impl Normalize for SchemaType {
    fn normalize(&mut self) {
        match self {
            SchemaType::Type(_) | SchemaType::Primitive(_) => {}
            SchemaType::Array(arr) => arr.items.normalize(),
            SchemaType::Object(obj) => normalize_schema_map(&mut obj.fields),
        }
    }
}

impl Normalize for MessageFormat {
    fn normalize(&mut self) {
        normalize_schema_map(&mut self.0);
    }
}

impl Normalize for EmittedTopic {
    fn normalize(&mut self) {
        normalize_opt(&mut self.message_format);
    }
}

impl Normalize for ConsumedTopic {
    fn normalize(&mut self) {}
}

impl Normalize for ConformsToItem {
    fn normalize(&mut self) {}
}

impl Normalize for ExposedService {
    fn normalize(&mut self) {
        normalize_opt(&mut self.request_message_format);
        normalize_opt(&mut self.response_message_format);
    }
}

impl Normalize for ConsumedService {
    fn normalize(&mut self) {}
}

impl Normalize for ConsumedAction {
    fn normalize(&mut self) {}
}

impl Normalize for ActionServiceEndpoint {
    fn normalize(&mut self) {
        normalize_opt(&mut self.request_message_format);
        normalize_opt(&mut self.response_message_format);
    }
}

impl Normalize for ActionTopicEndpoint {
    fn normalize(&mut self) {
        normalize_opt(&mut self.message_format);
    }
}

impl Normalize for ExposedAction {
    fn normalize(&mut self) {
        if let Some(gs) = &mut self.goal_service {
            gs.normalize();
        }
        if let Some(ft) = &mut self.feedback_topic {
            ft.normalize();
        }
        if let Some(rs) = &mut self.result_service {
            rs.normalize();
        }
    }
}

impl Normalize for TopicInterfaces {
    fn normalize(&mut self) {
        normalize_opt_vec(&mut self.emits, |a, b| {
            a.name
                .cmp(&b.name)
                .then_with(|| format!("{a:?}").cmp(&format!("{b:?}")))
        });
        normalize_opt_vec(&mut self.consumes, |a, b| {
            a.name.cmp(&b.name).then_with(|| a.link_id.cmp(&b.link_id))
        });
    }
}

impl Normalize for ServiceInterfaces {
    fn normalize(&mut self) {
        normalize_opt_vec(&mut self.exposes, |a, b| {
            a.name
                .cmp(&b.name)
                .then_with(|| format!("{a:?}").cmp(&format!("{b:?}")))
        });
        normalize_opt_vec(&mut self.consumes, |a, b| {
            a.name.cmp(&b.name).then_with(|| a.link_id.cmp(&b.link_id))
        });
    }
}

impl Normalize for ActionInterfaces {
    fn normalize(&mut self) {
        normalize_opt_vec(&mut self.exposes, |a, b| {
            a.name
                .cmp(&b.name)
                .then_with(|| format!("{a:?}").cmp(&format!("{b:?}")))
        });
        normalize_opt_vec(&mut self.consumes, |a, b| {
            a.name.cmp(&b.name).then_with(|| a.link_id.cmp(&b.link_id))
        });
    }
}

impl Normalize for Interfaces {
    fn normalize(&mut self) {
        normalize_opt_default(&mut self.topics);
        normalize_opt_default(&mut self.services);
        normalize_opt_default(&mut self.actions);
        normalize_opt_vec(&mut self.conforms_to, |a, b| {
            a.name
                .as_str()
                .cmp(b.name.as_str())
                .then_with(|| a.tag.cmp(&b.tag))
                .then_with(|| a.sha256.cmp(&b.sha256))
        });
    }
}

impl Interfaces {
    /// Compares two `Interfaces` for equivalence, ignoring the order of items
    /// within each list and the order of fields within message formats.
    pub fn matches_unordered(&self, other: &Interfaces) -> bool {
        self.clone().normalized() == other.clone().normalized()
    }
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
    fn blocked_mount_source_matches_top_level_and_private_aliases() {
        // Exact top-level system mounts are blocked.
        assert!(is_blocked_mount_source("/"));
        assert!(is_blocked_mount_source("/etc"));
        assert!(is_blocked_mount_source("/usr"));
        // macOS `/private/X` aliases map back onto the blocked root.
        assert!(is_blocked_mount_source("/private/tmp"));
        assert!(is_blocked_mount_source("/private/var"));
        // Subdirectories of a blocked root are allowed.
        assert!(!is_blocked_mount_source("/tmp/my_app"));
        assert!(!is_blocked_mount_source("/etc/myconf"));
        assert!(!is_blocked_mount_source("/home/user/project"));
        // Unrelated roots and non-root `/private` subpaths are fine.
        assert!(!is_blocked_mount_source("/data"));
        assert!(!is_blocked_mount_source("/private/tmp/sub"));
    }

    #[test]
    fn extract_parameter_refs_pulls_each_dot_path() {
        // No references -> empty.
        assert!(extract_parameter_refs("/data/output").is_empty());
        // A single bare reference.
        assert_eq!(
            extract_parameter_refs("${parameters:device_path}"),
            vec!["device_path"]
        );
        // Multiple references (one nested dot-path) embedded between literals.
        assert_eq!(
            extract_parameter_refs(
                "/mnt/${parameters:video.device_path}:/in/${parameters:out_dir}"
            ),
            vec!["video.device_path", "out_dir"]
        );
        // An unterminated reference (no closing brace) is ignored, not partial.
        assert!(extract_parameter_refs("${parameters:oops").is_empty());
    }

    #[test]
    fn consumed_topic_linked_link_id_is_required() {
        let valid = r#"{ link_id: "uvc_camera", name: "video_stream" }"#;
        let topic: ConsumedTopic = serde_json5::from_str(valid).expect("valid topic should parse");
        assert_eq!(topic.link_id, "uvc_camera");
        assert_eq!(topic.name, "video_stream");

        let empty_link_id = r#"{ link_id: "", name: "video_stream" }"#;
        assert!(serde_json5::from_str::<ConsumedTopic>(empty_link_id).is_err());

        let missing_name = r#"{ link_id: "uvc_camera", name: "" }"#;
        assert!(serde_json5::from_str::<ConsumedTopic>(missing_name).is_err());

        let whitespace_only = r#"{ link_id: "   ", name: "video_stream" }"#;
        assert!(serde_json5::from_str::<ConsumedTopic>(whitespace_only).is_err());

        let punctuation_only = r#"{ link_id: "--", name: "video_stream" }"#;
        assert!(serde_json5::from_str::<ConsumedTopic>(punctuation_only).is_err());

        let missing_name_field = r#"{ link_id: "uvc_camera" }"#;
        assert!(serde_json5::from_str::<ConsumedTopic>(missing_name_field).is_err());

        let trimmed = r#"{ link_id: " uvc_camera ", name: " video_stream " }"#;
        let topic: ConsumedTopic =
            serde_json5::from_str(trimmed).expect("whitespace should be trimmed");
        assert_eq!(topic.link_id, "uvc_camera");
        assert_eq!(topic.name, "video_stream");
    }

    #[test]
    fn consumed_topic_rejects_external_shape() {
        // External consumed topics were removed. An entry with an inline
        // message_format and no link_id must fail to parse as a consumed topic
        // (message_format is an unknown field, link_id is required).
        let external_shape =
            r#"{ name: "cmd_vel", message_format: { linear_x: "f64", angular_z: "f64" } }"#;
        assert!(
            serde_json5::from_str::<ConsumedTopic>(external_shape).is_err(),
            "external-shaped topic (no link_id, inline message_format) should be rejected"
        );

        // name-only (no link_id) must also fail.
        let name_only = r#"{ name: "cmd_vel" }"#;
        assert!(
            serde_json5::from_str::<ConsumedTopic>(name_only).is_err(),
            "name-only (no link_id) should fail"
        );
    }

    #[test]
    fn consumed_topic_rejects_unknown_fields() {
        // A linked consumed topic with an extra message_format must fail rather
        // than silently dropping the field.
        let linked_with_extra = r#"{
            link_id: "camera",
            name: "video_stream",
            message_format: { x: "f64" }
        }"#;
        assert!(
            serde_json5::from_str::<ConsumedTopic>(linked_with_extra).is_err(),
            "consumed topic with extra message_format should be rejected"
        );
    }

    #[test]
    fn consumed_service_link_id_is_required() {
        let with_link_id = r#"{ link_id: "uvc_camera", name: "enable_camera" }"#;
        let service: ConsumedService =
            serde_json5::from_str(with_link_id).expect("service with link_id should parse");
        assert_eq!(service.link_id, "uvc_camera");

        let trimmed = r#"{ link_id: "  uvc_camera  ", name: "enable_camera" }"#;
        let service: ConsumedService =
            serde_json5::from_str(trimmed).expect("whitespace should be trimmed");
        assert_eq!(service.link_id, "uvc_camera");

        let without_link_id = r#"{ name: "enable_camera" }"#;
        assert!(serde_json5::from_str::<ConsumedService>(without_link_id).is_err());

        let blank_link_id = r#"{ link_id: "   ", name: "enable_camera" }"#;
        assert!(serde_json5::from_str::<ConsumedService>(blank_link_id).is_err());
    }

    #[test]
    fn exposed_action_supports_optional_payloads() {
        let json5 = r#"{
            name: "move_arm",
            goal_service: {
                request_message_format: {
                    feedback_frequency: "u32",
                    desired_position: { $type: "array", $items: "f64", $length: 3 },
                    desired_orientation: { $type: "array", $items: "f64", $length: 4 },
                },
                response_message_format: {
                    accepted: "bool",
                },
            },
            result_service: {
                response_message_format: {
                    success: "bool",
                    message: "string",
                    final_joint_positions: { $type: "array", $items: "f64" },
                    final_ee_position: { $type: "array", $items: "f64", $length: 3 },
                    action_time: "f64",
                },
            },
        }"#;

        let action: ExposedAction =
            serde_json5::from_str(json5).expect("action with omitted optional payloads must parse");

        assert_eq!(action.name, "move_arm");
        assert!(
            action.feedback_topic.is_none(),
            "feedback_topic is optional"
        );

        let result = action
            .result_service
            .as_ref()
            .expect("result_service is present");
        assert!(
            result.request_message_format.is_none(),
            "result_service.request_message_format is optional"
        );
        assert!(
            result.response_message_format.is_some(),
            "result_service.response_message_format was provided"
        );

        let goal = action
            .goal_service
            .as_ref()
            .expect("goal_service is present");
        assert!(goal.request_message_format.is_some());
        assert!(goal.response_message_format.is_some());
    }

    #[test]
    fn exposed_action_accepts_empty_message_format_object() {
        let json5 = r#"{
            name: "calibrate",
            goal_service: {
                request_message_format: {},
                response_message_format: { accepted: "bool" },
            },
        }"#;

        let action: ExposedAction =
            serde_json5::from_str(json5).expect("empty `{}` message format must parse");
        let goal = action.goal_service.expect("goal_service is present");
        let request = goal
            .request_message_format
            .expect("request_message_format is Some when `{}` is given explicitly");
        assert!(
            request.0.is_empty(),
            "empty `{{}}` deserializes to an empty MessageFormat"
        );
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
    fn object_schema_implies_type_when_omitted() {
        let json5 = r#"{
            header: { stamp: "time", frame_id: "u32" }
        }"#;

        let parsed: MessageFormat = serde_json5::from_str(json5).expect("should parse");
        let SchemaType::Object(obj) = &parsed.0["header"] else {
            panic!("expected Object");
        };
        assert_eq!(obj.fields["stamp"], SchemaType::Type(TypeToken::Time));
        assert_eq!(obj.fields["frame_id"], SchemaType::Type(TypeToken::U32));
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
    fn object_fields_can_be_arrays() {
        let json5 = r#"{
            header: {
                $type: "object",
                data: { $type: "array", $items: "u8" }
            }
        }"#;

        let parsed: MessageFormat = serde_json5::from_str(json5).expect("should parse");
        let header = &parsed.0["header"];
        let SchemaType::Object(obj) = header else {
            panic!("expected Object");
        };
        let SchemaType::Array(arr) = &obj.fields["data"] else {
            panic!("expected Array");
        };
        assert_eq!(*arr.items, SchemaType::Type(TypeToken::U8));
    }

    #[test]
    fn object_fields_can_be_objects() {
        let json5 = r#"{
            outer: {
                $type: "object",
                inner: { $type: "object", value: "u32" }
            }
        }"#;

        let parsed: MessageFormat = serde_json5::from_str(json5).expect("should parse");
        let SchemaType::Object(outer) = &parsed.0["outer"] else {
            panic!("expected Object");
        };
        let SchemaType::Object(inner) = &outer.fields["inner"] else {
            panic!("expected nested Object");
        };
        assert_eq!(inner.fields["value"], SchemaType::Type(TypeToken::U32));
    }

    #[test]
    fn array_items_can_be_objects() {
        let json5 = r#"{
            frames: {
                $type: "array",
                $items: {
                    $type: "object",
                    name: "string",
                    parent: "string",
                    position: {
                        $type: "array",
                        $items: "i32",
                        $length: 3
                    },
                    orientation: {
                        $type: "array",
                        $items: "i32",
                        $length: 4
                    },
                },
            }
        }"#;

        let parsed: MessageFormat = serde_json5::from_str(json5).expect("should parse");
        let SchemaType::Array(frames_arr) = &parsed.0["frames"] else {
            panic!("expected Array");
        };
        assert!(frames_arr.length.is_none());

        let SchemaType::Object(frame_obj) = frames_arr.items.as_ref() else {
            panic!("expected Object items");
        };
        assert_eq!(
            frame_obj.fields["name"],
            SchemaType::Type(TypeToken::String)
        );
        assert_eq!(
            frame_obj.fields["parent"],
            SchemaType::Type(TypeToken::String)
        );

        let SchemaType::Array(pos) = &frame_obj.fields["position"] else {
            panic!("expected Array for position");
        };
        assert_eq!(*pos.items, SchemaType::Type(TypeToken::I32));
        assert_eq!(pos.length, Some(3));

        let SchemaType::Array(orient) = &frame_obj.fields["orientation"] else {
            panic!("expected Array for orientation");
        };
        assert_eq!(*orient.items, SchemaType::Type(TypeToken::I32));
        assert_eq!(orient.length, Some(4));
    }

    #[test]
    fn array_items_can_be_objects_without_object_type() {
        let json5 = r#"{
            frames: {
                $type: "array",
                $items: {
                    // $type: "object", This one is optional here since it's automatically implied
                    name: "string",
                    parent: "string",
                    position: {
                        $type: "array",
                        $items: "i32",
                        $length: 3
                    },
                    orientation: {
                        $type: "array",
                        $items: "i32",
                        $length: 4
                    },
                },
            }
        }"#;

        let parsed: MessageFormat = serde_json5::from_str(json5).expect("should parse");
        let SchemaType::Array(frames_arr) = &parsed.0["frames"] else {
            panic!("expected Array");
        };
        assert!(frames_arr.length.is_none());

        let SchemaType::Object(frame_obj) = frames_arr.items.as_ref() else {
            panic!("expected Object items");
        };
        assert_eq!(
            frame_obj.fields["name"],
            SchemaType::Type(TypeToken::String)
        );
        assert_eq!(
            frame_obj.fields["parent"],
            SchemaType::Type(TypeToken::String)
        );

        let SchemaType::Array(pos) = &frame_obj.fields["position"] else {
            panic!("expected Array for position");
        };
        assert_eq!(*pos.items, SchemaType::Type(TypeToken::I32));
        assert_eq!(pos.length, Some(3));

        let SchemaType::Array(orient) = &frame_obj.fields["orientation"] else {
            panic!("expected Array for orientation");
        };
        assert_eq!(*orient.items, SchemaType::Type(TypeToken::I32));
        assert_eq!(orient.length, Some(4));
    }

    #[test]
    /// Verifies that a nested schema (array of objects containing a fixed-length array)
    /// survives a serialize → deserialize roundtrip without data loss.
    fn nested_schema_roundtrip() {
        let json5 = r#"{
            frames: {
                $type: "array",
                $items: {
                    $type: "object",
                    name: "string",
                    position: { $type: "array", $items: "f32", $length: 3 }
                }
            }
        }"#;

        let parsed: MessageFormat = serde_json5::from_str(json5).expect("should parse");
        let serialized = serde_json5::to_string(&parsed).expect("should serialize");
        let reparsed: MessageFormat = serde_json5::from_str(&serialized).expect("should re-parse");
        assert_eq!(parsed, reparsed);
    }

    #[test]
    fn root_level_optional_is_accepted() {
        let json5 = r#"{
            error_msg: { 
              $type: "string", 
              $optional: true 
            },
            code: "u32"
        }"#;

        let parsed: MessageFormat = serde_json5::from_str(json5).expect("should parse");
        assert!(parsed.0["error_msg"].is_optional());
        assert!(!parsed.0["code"].is_optional());
    }

    #[test]
    fn object_field_rejects_optional() {
        let json5 = r#"{
            header: {
                $type: "object",
                debug: { 
                  $type: "string", 
                  $optional: true 
                }
            }
        }"#;

        let parsed: Result<MessageFormat, _> = serde_json5::from_str(json5);
        assert!(
            parsed.is_err(),
            "optional on nested object field should fail"
        );
    }

    #[test]
    fn array_items_rejects_optional() {
        let json5 = r#"{
            values: { 
              $type: "array", 
              $items: { 
                $type: "u8", 
                $optional: true 
              }
            }
        }"#;

        let parsed: Result<MessageFormat, _> = serde_json5::from_str(json5);
        assert!(parsed.is_err(), "optional on array items should fail");
    }

    #[test]
    fn array_items_rejects_nested_arrays() {
        let json5 = r#"{
            data: {
              $type: "array", 
              $items: {
                $type: "array", 
                $items: "u8" 
              }
            }
        }"#;

        let result: Result<MessageFormat, _> = serde_json5::from_str(json5);
        assert!(
            result.is_err(),
            "nested arrays (arrays of arrays) should fail"
        );
    }

    #[test]
    fn deeply_nested_rejects_optional() {
        let json5 = r#"{
            frames: {
                $type: "array",
                $items: {
                    $type: "object",
                    name: "string",
                    debug: {
                      $type: "string",
                      $optional: true 
                    }
                }
            }
        }"#;

        let parsed: Result<MessageFormat, _> = serde_json5::from_str(json5);
        assert!(
            parsed.is_err(),
            "optional on field inside array-of-objects should fail"
        );
    }

    #[test]
    fn manifest_with_depends_on() {
        let json5 = r#"{
            name: "slam",
            tag: "v1",
            depends_on: {
                nodes: [
                    { name: "lidar_driver", tag: "v1", link_id: "lidar" },
                    { name: "nav_system", tag: "v1", link_id: "navigation" }
                ]
            }
        }"#;
        let manifest: Manifest = serde_json5::from_str(json5).expect("should parse");
        let deps = manifest.depends_on.expect("depends_on should be Some");
        assert_eq!(deps.nodes.len(), 2);
        assert_eq!(deps.nodes[0].name.as_str(), "lidar_driver");
        assert_eq!(deps.nodes[0].tag, "v1");
        assert_eq!(deps.nodes[0].link_id, "lidar");
        assert_eq!(deps.nodes[1].name.as_str(), "nav_system");
        assert_eq!(deps.nodes[1].link_id, "navigation");
        assert!(deps.interfaces.is_empty());
    }

    #[test]
    fn depends_on_with_interfaces_full() {
        let json5 = r#"{
            nodes: [],
            interfaces: [
                { name: "depth_camera", tag: "v1", sha256: "aaa", link_id: "depth_camera" }
            ]
        }"#;
        let deps: DependsOn = serde_json5::from_str(json5).expect("should parse");
        assert!(deps.nodes.is_empty());
        assert_eq!(deps.interfaces.len(), 1);
        assert_eq!(deps.interfaces[0].name.as_str(), "depth_camera");
        assert_eq!(deps.interfaces[0].tag, "v1");
        assert_eq!(deps.interfaces[0].link_id, "depth_camera");
        assert_eq!(deps.interfaces[0].sha256.as_deref(), Some("aaa"));
    }

    #[test]
    fn depends_on_with_interfaces_no_sha256() {
        let json5 = r#"{
            nodes: [],
            interfaces: [
                { name: "depth_camera", tag: "v1", link_id: "depth_camera" }
            ]
        }"#;
        let deps: DependsOn = serde_json5::from_str(json5).expect("should parse");
        assert_eq!(deps.interfaces.len(), 1);
        assert!(deps.interfaces[0].sha256.is_none());
    }

    #[test]
    fn depends_on_interfaces_requires_name() {
        let json5 = r#"{
            nodes: [],
            interfaces: [{ tag: "v1", link_id: "depth_camera" }]
        }"#;
        assert!(serde_json5::from_str::<DependsOn>(json5).is_err());
    }

    #[test]
    fn depends_on_interfaces_requires_tag() {
        let json5 = r#"{
            nodes: [],
            interfaces: [{ name: "depth_camera", link_id: "depth_camera" }]
        }"#;
        assert!(serde_json5::from_str::<DependsOn>(json5).is_err());
    }

    #[test]
    fn depends_on_interfaces_requires_link_id() {
        let json5 = r#"{
            nodes: [],
            interfaces: [{ name: "depth_camera", tag: "v1" }]
        }"#;
        assert!(serde_json5::from_str::<DependsOn>(json5).is_err());
    }

    #[test]
    fn depends_on_interfaces_rejects_unknown_fields() {
        let json5 = r#"{
            nodes: [],
            interfaces: [
                { name: "depth_camera", tag: "v1", link_id: "depth_camera", extra: "bad" }
            ]
        }"#;
        assert!(serde_json5::from_str::<DependsOn>(json5).is_err());
    }

    #[test]
    fn depends_on_with_only_interfaces() {
        let json5 = r#"{
            interfaces: [
                { name: "uvc_camera", tag: "v1", link_id: "camera" }
            ]
        }"#;
        let deps: DependsOn = serde_json5::from_str(json5).expect("nodes should be optional");
        assert!(deps.nodes.is_empty());
        assert_eq!(deps.interfaces.len(), 1);
        assert_eq!(deps.interfaces[0].link_id, "camera");
    }

    #[test]
    fn manifest_without_depends_on() {
        let json5 = r#"{
            name: "simple_node",
            tag: "v1"
        }"#;
        let manifest: Manifest = serde_json5::from_str(json5).expect("should parse");
        assert!(manifest.depends_on.is_none());
    }

    #[test]
    fn interfaces_with_conforms_to_full() {
        let json5 = r#"{
            conforms_to: [
                { name: "depth_camera", tag: "v1", sha256: "aaaa" }
            ]
        }"#;
        let interfaces: Interfaces = serde_json5::from_str(json5).expect("should parse");
        let items = interfaces.conforms_to.expect("conforms_to should be Some");
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].name.as_str(), "depth_camera");
        assert_eq!(items[0].tag, "v1");
        assert_eq!(items[0].sha256.as_deref(), Some("aaaa"));
    }

    #[test]
    fn interfaces_with_conforms_to_no_sha256() {
        let json5 = r#"{
            conforms_to: [
                { name: "depth_camera", tag: "v1" }
            ]
        }"#;
        let interfaces: Interfaces = serde_json5::from_str(json5).expect("should parse");
        let items = interfaces.conforms_to.expect("conforms_to should be Some");
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].name.as_str(), "depth_camera");
        assert_eq!(items[0].tag, "v1");
        assert!(items[0].sha256.is_none());
    }

    #[test]
    fn interfaces_without_conforms_to() {
        let json5 = r#"{}"#;
        let interfaces: Interfaces = serde_json5::from_str(json5).expect("should parse");
        assert!(interfaces.conforms_to.is_none());
    }

    #[test]
    fn interfaces_conforms_to_requires_name() {
        let json5 = r#"{
            conforms_to: [
                { tag: "v1" }
            ]
        }"#;
        assert!(serde_json5::from_str::<Interfaces>(json5).is_err());
    }

    #[test]
    fn interfaces_conforms_to_requires_tag() {
        let json5 = r#"{
            conforms_to: [
                { name: "depth_camera" }
            ]
        }"#;
        assert!(serde_json5::from_str::<Interfaces>(json5).is_err());
    }

    #[test]
    fn interfaces_conforms_to_rejects_unknown_fields() {
        let json5 = r#"{
            conforms_to: [
                { name: "depth_camera", tag: "v1", extra: "bad" }
            ]
        }"#;
        assert!(serde_json5::from_str::<Interfaces>(json5).is_err());
    }

    #[test]
    fn interfaces_conforms_to_normalization_sorts_by_name() {
        let item_a = ConformsToItem {
            name: Name::new("alpha").unwrap(),
            tag: "v1".into(),
            sha256: None,
        };
        let item_b = ConformsToItem {
            name: Name::new("beta").unwrap(),
            tag: "v1".into(),
            sha256: Some("aaaa".into()),
        };

        let interfaces_a = Interfaces {
            topics: None,
            services: None,
            actions: None,
            conforms_to: Some(vec![item_a.clone(), item_b.clone()]),
        };
        let interfaces_b = Interfaces {
            topics: None,
            services: None,
            actions: None,
            conforms_to: Some(vec![item_b, item_a]),
        };

        assert!(interfaces_a.matches_unordered(&interfaces_b));
    }

    #[test]
    fn depends_on_rejects_unknown_fields() {
        let json5 = r#"{
            name: "node",
            tag: "v1",
            depends_on: {
                nodes: [{ name: "dep", tag: "v1", link_id: "d", extra: "bad" }]
            }
        }"#;
        assert!(serde_json5::from_str::<Manifest>(json5).is_err());
    }

    #[test]
    fn manifest_parses_minimal() {
        let json5 = r#"{
            name: "simple_node",
            tag: "v1"
        }"#;
        let manifest: Manifest = serde_json5::from_str(json5).expect("should parse");
        assert_eq!(manifest.name.as_str(), "simple_node");
        assert_eq!(manifest.tag, "v1");
    }

    #[test]
    fn node_config_rejects_unknown_fields() {
        let json5 = r#"{
            peppy_schema: "node_v1",
            manifest: { name: "node", tag: "v1" },
            execution: { language: "rust", run_cmd: ["./run"] },
            extra: "bad"
        }"#;
        assert!(serde_json5::from_str::<NodeConfig>(json5).is_err());
    }

    /// A node-shaped document that mislabels itself as a launcher must
    /// be rejected — the schema field is the source of truth, not just
    /// the field names.
    #[test]
    fn node_config_rejects_non_node_schema() {
        let json5 = r#"{
            peppy_schema: "launcher_v1",
            manifest: { name: "node", tag: "v1" },
            execution: { language: "rust", build_cmd: ["true"], run_cmd: ["true"] }
        }"#;
        let err = serde_json5::from_str::<NodeConfig>(json5)
            .expect_err("launcher_v1 schema must be rejected");
        assert!(
            err.to_string().contains("node_v1"),
            "error should mention the expected schema, got: {err}"
        );
    }

    #[test]
    fn consume_normalization_sorts_by_name_and_link_id() {
        // TopicInterfaces: two linked consumed topics with same name, different link_id
        let mut topics_a = TopicInterfaces {
            emits: None,
            consumes: Some(vec![
                ConsumedTopic {
                    link_id: "node_b".into(),
                    name: "topic".into(),
                },
                ConsumedTopic {
                    link_id: "node_a".into(),
                    name: "topic".into(),
                },
            ]),
        };
        let mut topics_b = TopicInterfaces {
            emits: None,
            consumes: Some(vec![
                ConsumedTopic {
                    link_id: "node_a".into(),
                    name: "topic".into(),
                },
                ConsumedTopic {
                    link_id: "node_b".into(),
                    name: "topic".into(),
                },
            ]),
        };
        topics_a.normalize();
        topics_b.normalize();
        assert_eq!(topics_a, topics_b);
        // Verify sorted order: node_a before node_b
        let consumes = topics_a.consumes.unwrap();
        assert_eq!(consumes[0].link_id, "node_a");
        assert_eq!(consumes[1].link_id, "node_b");

        // ServiceInterfaces: same name, different link_id
        let mut services_a = ServiceInterfaces {
            exposes: None,
            consumes: Some(vec![
                ConsumedService {
                    link_id: "node_b".into(),
                    name: "svc".into(),
                },
                ConsumedService {
                    link_id: "node_a".into(),
                    name: "svc".into(),
                },
            ]),
        };
        let mut services_b = ServiceInterfaces {
            exposes: None,
            consumes: Some(vec![
                ConsumedService {
                    link_id: "node_a".into(),
                    name: "svc".into(),
                },
                ConsumedService {
                    link_id: "node_b".into(),
                    name: "svc".into(),
                },
            ]),
        };
        services_a.normalize();
        services_b.normalize();
        assert_eq!(services_a, services_b);
        let consumes = services_a.consumes.unwrap();
        assert_eq!(consumes[0].link_id, "node_a");
        assert_eq!(consumes[1].link_id, "node_b");

        // ActionInterfaces: same name, different link_id
        let mut actions_a = ActionInterfaces {
            exposes: None,
            consumes: Some(vec![
                ConsumedAction {
                    link_id: "node_b".into(),
                    name: "act".into(),
                },
                ConsumedAction {
                    link_id: "node_a".into(),
                    name: "act".into(),
                },
            ]),
        };
        let mut actions_b = ActionInterfaces {
            exposes: None,
            consumes: Some(vec![
                ConsumedAction {
                    link_id: "node_a".into(),
                    name: "act".into(),
                },
                ConsumedAction {
                    link_id: "node_b".into(),
                    name: "act".into(),
                },
            ]),
        };
        actions_a.normalize();
        actions_b.normalize();
        assert_eq!(actions_a, actions_b);
        let consumes = actions_a.consumes.unwrap();
        assert_eq!(consumes[0].link_id, "node_a");
        assert_eq!(consumes[1].link_id, "node_b");
    }
}
