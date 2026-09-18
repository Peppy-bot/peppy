use super::refine::{
    ActionRefinement, FieldRefinement, FormatRefinement, ResultServiceRefinement,
    ServiceRefinement, TopicRefinement,
};
use crate::{
    common::{ParameterSchema, ParameterSpec, resolve_parameter_path, type_token_name},
    error::ParsingError,
    internal::runtime::Name,
    schema::PeppySchema,
};
use indexmap::IndexMap;
use serde::{
    Deserialize, Serialize,
    de::{self, Deserializer, MapAccess, Visitor},
};
use std::{
    collections::BTreeMap,
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

/// Reject any `peppy_schema` value other than `node/v1` so a launcher
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

// NodeInfo is not part of the new schema; manifest/config/instances carry this information.

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum TypeToken {
    Bool,
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

impl TypeToken {
    /// Whether this token encodes as a Cap'n Proto scalar, meaning a field
    /// that lives in the struct's fixed data section rather than behind a
    /// pointer. `string`, `bytes`, and `time` are pointer-backed (`Text`,
    /// `Data`, and a `Timestamp` struct); every other token is a scalar.
    ///
    /// The distinction decides what a field can express. A pointer field is
    /// null when unset, which is the presence information `$optional` needs,
    /// so only pointer-backed types accept it. A scalar field has a fixed
    /// width and no pointer indirection, which is what a `$length` array
    /// needs, so only scalars are valid fixed-length array items.
    pub const fn is_scalar(&self) -> bool {
        matches!(
            self,
            TypeToken::Bool
                | TypeToken::U8
                | TypeToken::U16
                | TypeToken::U32
                | TypeToken::U64
                | TypeToken::I8
                | TypeToken::I16
                | TypeToken::I32
                | TypeToken::I64
                | TypeToken::F32
                | TypeToken::F64
        )
    }
}

/// Common wrapper for dynamic message formats in topics, services, and
/// actions.
///
/// A format holds at least one field: an empty `{}` is rejected at parse
/// time. "No payload" therefore has exactly one spelling, the omitted key,
/// and code generation reads presence of the key alone to decide whether an
/// endpoint carries a payload.
#[derive(Debug, Clone, Serialize, Default, PartialEq)]
pub struct MessageFormat(pub IndexMap<String, SchemaType>);

impl<'de> Deserialize<'de> for MessageFormat {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let fields = IndexMap::<String, SchemaType>::deserialize(deserializer)?;
        if fields.is_empty() {
            return Err(de::Error::custom(
                "a message format cannot be empty; omit the key entirely when the payload \
carries no fields",
            ));
        }
        // Only root-level fields are checked because only they can be
        // optional at all: `ObjectSchema` rejects `$optional` on its nested
        // fields and `$items` rejects it on array items, both at parse time.
        //
        // The check lives here rather than in `PrimitiveSchema` because
        // `SchemaType` is `untagged`: an error raised inside a variant is
        // discarded and reported as "data did not match any variant", which
        // tells a node author nothing. `MessageFormat` is the nearest
        // enclosing type whose errors reach them intact.
        for (name, schema) in &fields {
            let SchemaType::Primitive(primitive) = schema else {
                continue;
            };
            if primitive.optional && primitive.kind.is_scalar() {
                return Err(de::Error::custom(format!(
                    "`$optional` is not supported on field `{name}` of type `{}`: it encodes as \
a fixed-width Cap'n Proto scalar, which is always present on the wire. Only `string`, `bytes`, \
`time`, objects, and arrays can be optional",
                    type_token_name(&primitive.kind)
                )));
            }
        }
        Ok(Self(fields))
    }
}

fn is_false(value: &bool) -> bool {
    !*value
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum InterfaceKind {
    Topic,
    Service,
    Action,
}

impl InterfaceKind {
    /// Capitalized noun for this kind (`"Topic"`, `"Service"`, `"Action"`),
    /// used in user-facing error payloads such as [`MissingInterface`].
    ///
    /// [`MissingInterface`]: crate::MissingInterface
    ///
    /// This is intentionally distinct from [`Display`], which renders the
    /// lowercase form that round-trips with [`FromStr`]. Do not replace call
    /// sites with `to_string()`/`{:?}`: the former changes the casing and the
    /// latter relies on the `Debug` derive matching the variant names.
    ///
    /// [`Display`]: std::fmt::Display
    /// [`FromStr`]: std::str::FromStr
    pub const fn label(&self) -> &'static str {
        match self {
            InterfaceKind::Topic => "Topic",
            InterfaceKind::Service => "Service",
            InterfaceKind::Action => "Action",
        }
    }

    /// The manifest section this kind's consumed entries live in, for error
    /// payloads. Mirrors the `SECTION` const the produced-entry enums carry.
    pub const fn consumed_section(&self) -> &'static str {
        match self {
            InterfaceKind::Topic => "topics.consumes",
            InterfaceKind::Service => "services.consumes",
            InterfaceKind::Action => "actions.consumes",
        }
    }
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

/// A primitive field in its structured form: a `$type` token plus modifiers.
///
/// `$optional` is accepted only on the pointer-backed tokens (`string`,
/// `bytes`, `time`); [`MessageFormat`] enforces that, and
/// [`TypeToken::is_scalar`] explains why a scalar cannot carry it.
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

/// A document-backed produced-interface entry: `{link_id, name}` where
/// `link_id` names a slot and `name` selects one member of the document that
/// slot resolves to (byte-equal), plus an optional `refine` block. Shape and
/// QoS come from that document, so inline `message_format` / `qos_profile` /
/// endpoint fields are rejected at parse time; `refine` declares no shape, it
/// pins the length of arrays the document leaves generic (see
/// [`FormatRefinement`]). The refinement type `R` is the kind-specific block
/// of the section the entry lives in.
///
/// The name is direction-neutral on purpose: which document backs the entry
/// depends on the slot it names. `services.exposes` and `actions.exposes`
/// admit `manifest.implements` slots only, so their entries are always
/// contract-backed. `topics.emits` and `topics.consumes` additionally admit
/// pairing slots (`depends_on.pairings` for emits and consumes,
/// `depends_on.pairing_observers` for consumes), whose member shapes come from
/// the pairing document instead.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct LinkedEntry<R> {
    pub link_id: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refine: Option<R>,
}

/// A document-backed `topics.emits` entry.
pub type LinkedTopic = LinkedEntry<Box<TopicRefinement>>;
/// A document-backed `services.exposes` entry.
pub type LinkedService = LinkedEntry<Box<ServiceRefinement>>;
/// A document-backed `actions.exposes` entry.
pub type LinkedAction = LinkedEntry<Box<ActionRefinement>>;

// Every refinement block is boxed. A block holds one `IndexMap` per format it
// names, which is wider than the whole entry it hangs off, and it is absent on
// nearly every entry: inlining it would grow each of these entries — resident
// per node config and cloned per dependency lookup — several times over to
// carry a `None`. `ExposedAction::Native` is boxed for the same reason.

/// One document-backed produced entry, tagged with the section it is
/// declared under so its refinement is the matching kind's block.
#[derive(Debug, Clone, Copy)]
pub enum LinkedMember<'a> {
    Topic(&'a LinkedTopic),
    Service(&'a LinkedService),
    Action(&'a LinkedAction),
}

impl LinkedMember<'_> {
    pub fn kind(&self) -> InterfaceKind {
        match self {
            Self::Topic(_) => InterfaceKind::Topic,
            Self::Service(_) => InterfaceKind::Service,
            Self::Action(_) => InterfaceKind::Action,
        }
    }

    pub fn link_id(&self) -> &str {
        match self {
            Self::Topic(e) => &e.link_id,
            Self::Service(e) => &e.link_id,
            Self::Action(e) => &e.link_id,
        }
    }

    pub fn name(&self) -> &str {
        match self {
            Self::Topic(e) => &e.name,
            Self::Service(e) => &e.name,
            Self::Action(e) => &e.name,
        }
    }
}

/// A natively-declared emitted topic (no `link_id`): the node owns the shape.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct NativeEmittedTopic {
    pub name: String,
    #[serde(default)]
    pub qos_profile: QoSProfile,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message_format: Option<MessageFormat>,
}

/// One entry of `interfaces.topics.emits`. Discriminated by the presence of
/// `link_id` (mirroring the consumes convention): with `link_id` the entry is
/// document-backed, without it the entry is native and carries its own shape.
///
/// A document-backed emit names either a `manifest.implements` slot (the
/// contract declares the shape) or a `depends_on.pairings` slot (the pairing
/// document declares it, for a topic this node's role emits).
#[derive(Debug, Clone, PartialEq)]
pub enum EmittedTopic {
    Linked(LinkedTopic),
    Native(NativeEmittedTopic),
}

/// A natively-declared exposed service (no `link_id`).
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct NativeExposedService {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request_message_format: Option<MessageFormat>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response_message_format: Option<MessageFormat>,
}

/// One entry of `interfaces.services.exposes`. See [`EmittedTopic`] for the
/// `link_id` discriminator rule. Pairing documents declare topics only, so a
/// document-backed entry here is always contract-backed.
#[derive(Debug, Clone, PartialEq)]
pub enum ExposedService {
    Linked(LinkedService),
    Native(NativeExposedService),
}

/// A natively-declared exposed action (no `link_id`).
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct NativeExposedAction {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub goal_service: Option<GoalServiceEndpoint>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub feedback_topic: Option<ActionTopicEndpoint>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result_service: Option<ResultServiceEndpoint>,
}

/// One entry of `interfaces.actions.exposes`. See [`EmittedTopic`] for the
/// `link_id` discriminator rule. Pairing documents declare topics only, so a
/// document-backed entry here is always contract-backed. The native payload is
/// boxed: its three optional endpoints dwarf `LinkedEntry`, and boxing keeps
/// the enum small for the common document-backed case.
#[derive(Debug, Clone, PartialEq)]
pub enum ExposedAction {
    Linked(LinkedAction),
    Native(Box<NativeExposedAction>),
}

/// Generates the shared surface of one produced-entry enum: accessors,
/// `Serialize`, and the discriminating `Deserialize`. The native shape
/// fields are declared once here — the raw mirror struct, the
/// inline-shape reject list, and the `Native` constructor are all derived
/// from that single list, so they cannot drift apart. Each field's mode
/// picks how a missing value materializes on a native entry: `default`
/// applies `unwrap_or_default()`, `keep` stores the `Option` as-is.
/// The wrap mode before the native type says how the `Native` variant holds
/// its payload: `plain` stores the struct inline, `boxed` stores it behind a
/// `Box` (for payloads large enough to trip `clippy::large_enum_variant`).
/// The refinement type after it is the kind-specific `refine` block a
/// document-backed entry of this section may carry; a native entry
/// carrying one is rejected, since it owns its shape and sets `$length`
/// directly.
macro_rules! impl_produced_entry {
    (@native_field default $value:expr) => {
        $value.unwrap_or_default()
    };
    (@native_field keep $value:expr) => {
        $value
    };
    (@native_ref plain $native:expr) => {
        $native
    };
    (@native_ref boxed $native:expr) => {
        &**$native
    };
    (@native_wrap plain $value:expr) => {
        $value
    };
    (@native_wrap boxed $value:expr) => {
        Box::new($value)
    };
    ($entry:ident, $wrap:tt $native:ident, $refine:ty, $section:literal,
     { $($field:ident : $ty:ty => $mode:tt),+ $(,)? }) => {
        impl $entry {
            /// Interface name: the document member selector for
            /// document-backed entries, the declared name for native ones.
            pub fn name(&self) -> &str {
                match self {
                    Self::Linked(e) => &e.name,
                    Self::Native(n) => &n.name,
                }
            }

            /// The slot this entry references (a `manifest.implements` or a
            /// pairing slot), or `None` for a native entry.
            pub fn link_id(&self) -> Option<&str> {
                match self {
                    Self::Linked(e) => Some(&e.link_id),
                    Self::Native(_) => None,
                }
            }

            pub fn as_linked(&self) -> Option<&LinkedEntry<$refine>> {
                match self {
                    Self::Linked(e) => Some(e),
                    Self::Native(_) => None,
                }
            }

            pub fn as_native(&self) -> Option<&$native> {
                match self {
                    Self::Linked(_) => None,
                    Self::Native(n) => Some(impl_produced_entry!(@native_ref $wrap n)),
                }
            }

            pub fn is_native(&self) -> bool {
                matches!(self, Self::Native(_))
            }

            /// The manifest section this entry type lives in, for error
            /// payloads (`"topics.emits"`, `"services.exposes"`,
            /// `"actions.exposes"`).
            pub const SECTION: &'static str = $section;
        }

        impl Serialize for $entry {
            fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
            where
                S: serde::Serializer,
            {
                match self {
                    Self::Linked(e) => e.serialize(serializer),
                    Self::Native(n) => n.serialize(serializer),
                }
            }
        }

        impl<'de> Deserialize<'de> for $entry {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: Deserializer<'de>,
            {
                #[derive(Deserialize)]
                #[serde(deny_unknown_fields)]
                struct Raw {
                    link_id: Option<String>,
                    name: Option<String>,
                    refine: Option<$refine>,
                    $($field: $ty,)+
                }

                let raw = Raw::deserialize(deserializer)?;
                let name = require_interface_name(raw.name, $entry::SECTION)?;
                match raw.link_id {
                    Some(link_id) => {
                        let link_id = validate_non_empty_identifier(
                            &link_id,
                            concat!(stringify!($entry), ".link_id"),
                        )
                        .map_err(de::Error::custom)?;
                        reject_inline_shape(
                            $entry::SECTION,
                            &link_id,
                            &name,
                            &[$((stringify!($field), raw.$field.is_some()),)+],
                        )?;
                        Ok(Self::Linked(LinkedEntry { link_id, name, refine: raw.refine }))
                    }
                    None => {
                        reject_refine_on_native($entry::SECTION, &name, raw.refine.is_some())?;
                        Ok(Self::Native(impl_produced_entry!(@native_wrap $wrap $native {
                            name,
                            $($field: impl_produced_entry!(@native_field $mode raw.$field),)+
                        })))
                    }
                }
            }
        }
    };
}

impl_produced_entry!(EmittedTopic, plain NativeEmittedTopic, Box<TopicRefinement>, "topics.emits", {
    qos_profile: Option<QoSProfile> => default,
    message_format: Option<MessageFormat> => keep,
});
impl_produced_entry!(ExposedService, plain NativeExposedService, Box<ServiceRefinement>, "services.exposes", {
    request_message_format: Option<MessageFormat> => keep,
    response_message_format: Option<MessageFormat> => keep,
});
impl_produced_entry!(ExposedAction, boxed NativeExposedAction, Box<ActionRefinement>, "actions.exposes", {
    goal_service: Option<GoalServiceEndpoint> => keep,
    feedback_topic: Option<ActionTopicEndpoint> => keep,
    result_service: Option<ResultServiceEndpoint> => keep,
});

/// Rejects `refine` on a native entry: a refinement pins arrays a document
/// leaves generic, and a native entry has no document behind it.
fn reject_refine_on_native<E: de::Error>(
    section: &'static str,
    name: &str,
    present: bool,
) -> Result<(), E> {
    if !present {
        return Ok(());
    }
    Err(E::custom(
        crate::error::StructuredError::RefineWithoutLinkId {
            section: section.to_owned(),
            name: name.to_owned(),
        }
        .json5_message(),
    ))
}

/// Validates the (required, non-empty) `name` of a produced-interface entry,
/// surfacing a [`StructuredError::EmptyInterfaceName`] through the serde
/// bridge on failure.
fn require_interface_name<E: de::Error>(
    name: Option<String>,
    section: &'static str,
) -> Result<String, E> {
    validate_non_empty_identifier(name.as_deref().unwrap_or_default(), section).map_err(|_| {
        E::custom(
            crate::error::StructuredError::EmptyInterfaceName {
                section: section.to_owned(),
            }
            .json5_message(),
        )
    })
}

/// Rejects inline shape/QoS fields on a document-backed entry: the shape
/// comes from the referenced document, never from the manifest. `fields` pairs
/// each field name with whether it was present in the raw entry.
fn reject_inline_shape<E: de::Error>(
    section: &'static str,
    link_id: &str,
    name: &str,
    fields: &[(&'static str, bool)],
) -> Result<(), E> {
    for (field, present) in fields {
        if *present {
            return Err(E::custom(
                crate::error::StructuredError::LinkedEntryWithInlineShape {
                    section: section.to_owned(),
                    link_id: link_id.to_owned(),
                    name: name.to_owned(),
                    field: (*field).to_owned(),
                }
                .json5_message(),
            ));
        }
    }
    Ok(())
}

/// A consumed topic: `link_id` names the `depends_on` slot it comes from and
/// `name` the topic on that slot. `refine` pins the length of arrays a
/// contract or pairing document leaves generic (see [`FormatRefinement`]);
/// it is rejected when the slot names a `depends_on.nodes` producer, whose
/// native shape is taken as is.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ConsumedTopic {
    #[serde(deserialize_with = "deserialize_consumed_topic_link_id")]
    pub link_id: String,
    #[serde(deserialize_with = "deserialize_consumed_topic_name")]
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refine: Option<Box<TopicRefinement>>,
}

/// A consumed service; `refine` follows the [`ConsumedTopic`] rule.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ConsumedService {
    #[serde(deserialize_with = "deserialize_consumed_service_link_id")]
    pub link_id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refine: Option<Box<ServiceRefinement>>,
}

/// A consumed action; `refine` follows the [`ConsumedTopic`] rule. The
/// action block is boxed for the same reason as in [`LinkedAction`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ConsumedAction {
    #[serde(deserialize_with = "deserialize_consumed_action_link_id")]
    pub link_id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refine: Option<Box<ActionRefinement>>,
}

/// The goal side of an exposed action. Both the request (the goal parameters)
/// and the response (the acceptance reply) are user-defined and optional.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct GoalServiceEndpoint {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request_message_format: Option<MessageFormat>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response_message_format: Option<MessageFormat>,
}

/// The result side of an exposed action. It has no user-defined request: a
/// `get_result` call identifies its goal by the framework-owned `goal_id`, so
/// the only declarable payload is the response the producer retains through
/// `GoalContext::complete` / `complete_cancelled`. Declaring a
/// `request_message_format` here is rejected by `deny_unknown_fields`.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ResultServiceEndpoint {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response_message_format: Option<MessageFormat>,
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

fn deserialize_contract_dependency_link_id<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: Deserializer<'de>,
{
    deserialize_non_empty_identifier(deserializer, "ContractDependency.link_id")
}

fn deserialize_pairing_dependency_link_id<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: Deserializer<'de>,
{
    deserialize_non_empty_identifier(deserializer, "PairingSlot.link_id")
}

fn deserialize_implements_link_id<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: Deserializer<'de>,
{
    deserialize_non_empty_identifier(deserializer, "ImplementsEntry.link_id")
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

/// An action's `feedback_topic`. Declaring the block is what opts the action
/// into a feedback stream, so `message_format` is required: a feedback message
/// exists to carry progress data, and an empty payload is reserved as the
/// framework's per-goal end-of-stream signal. An action that streams no
/// progress omits `feedback_topic` entirely.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ActionTopicEndpoint {
    #[serde(skip_serializing_if = "Option::is_none", rename = "type")]
    pub topic_type: Option<String>,
    #[serde(default)]
    pub qos_profile: QoSProfile,
    pub message_format: MessageFormat,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

/// How many sources a slot takes: bound producers for
/// `depends_on.{nodes,contracts}`, observed pairings for
/// `depends_on.pairing_observers`. On producer slots it constrains only the
/// size of the application-selected bound set, validated at plan time and
/// fixed when the node starts; it never permits undeclared or unbound
/// launched instances, and it is not live discovery. On observer slots the
/// same spellings constrain the member count the planner accepts, and the
/// member set the daemon delivers changes over the node's lifetime.
/// On participant pairing slots the same spellings size the set of pairs the
/// slot holds, and pairs join and leave a multi slot while the node runs.
///
/// All four spellings are valid on every slot kind that carries the key, and mean
/// the same thing on each. The two scalar spellings ([`Cardinality::One`],
/// [`Cardinality::ZeroOrOne`]) size a slot read through a singular accessor,
/// the two multi spellings one read as a set. A floor of zero is only ever
/// reached explicitly: a [`Cardinality::ZeroOrOne`] slot empties through a
/// written `{ vacant: "<why>" }`, so a slot that is deliberately empty stays
/// distinguishable from one a deployment forgot.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Cardinality {
    /// The slot binds exactly one producer (the default when omitted).
    #[default]
    One,
    /// The slot takes at most one source: the deployment either links one, or
    /// writes the slot vacant with the reason it stays empty. Scalar-shaped
    /// like [`Cardinality::One`], with a floor of zero.
    ZeroOrOne,
    /// The slot binds one or more producers; an empty set is a binding error.
    OneOrMore,
    /// The slot binds zero or more producers; an empty set is valid and
    /// simply produces no bound edge.
    ZeroOrMore,
}

impl Cardinality {
    pub fn is_one(&self) -> bool {
        matches!(self, Cardinality::One)
    }

    /// Whether the slot holds at most one source, so its generated accessor is
    /// singular rather than set-valued. The one shape rule, shared by the code
    /// generator, the node runtime's observation-slot handles and the launcher's
    /// error hints, so an accessor and the handle it calls can never disagree
    /// about a slot's shape. It answers the singular/set-valued split only: what
    /// each singular accessor returns is the cardinality's own business, since
    /// `one` always resolves a source and `zero_or_one` may not.
    pub fn is_scalar(&self) -> bool {
        matches!(self, Cardinality::One | Cardinality::ZeroOrOne)
    }

    /// Whether an empty bound set satisfies the slot without the deployment
    /// saying anything. `zero_or_one` is deliberately absent: it admits an
    /// empty set, but only where the deployment wrote the slot vacant, so an
    /// omitted `zero_or_one` slot stays uncovered.
    pub fn allows_empty(&self) -> bool {
        matches!(self, Cardinality::ZeroOrMore)
    }

    /// Whether a slot of this cardinality may already hold `len` members
    /// before its owner has been delivered any. Only the ceiling applies: a
    /// pairing slot's pairs arrive after the instance commits to Running, so a
    /// seed of zero is legitimate at every cardinality, `one_or_more`
    /// included. Contrast [`Self::admits`], which the seeds that arrive whole
    /// (an observer's, a producer binding's) are held to.
    pub fn admits_seed(&self, len: usize) -> bool {
        match self {
            Cardinality::One | Cardinality::ZeroOrOne => len <= 1,
            Cardinality::OneOrMore | Cardinality::ZeroOrMore => true,
        }
    }

    /// Whether a set of `len` sources satisfies the slot: exactly one for
    /// `one`, at most one for `zero_or_one`, at least one for `one_or_more`,
    /// any size for `zero_or_more`. The one size-admission rule, shared by
    /// plan-time binding validation and the node runtime's startup re-check of
    /// both bound producers and observation seeds, so a slot cannot boot holding
    /// a set its manifest does not admit.
    pub fn admits(&self, len: usize) -> bool {
        match self {
            Cardinality::One => len == 1,
            Cardinality::ZeroOrOne => len <= 1,
            Cardinality::OneOrMore => len >= 1,
            Cardinality::ZeroOrMore => true,
        }
    }

    /// The manifest spelling (`one`, `zero_or_one`, `one_or_more`,
    /// `zero_or_more`), for error messages.
    pub fn as_str(&self) -> &'static str {
        match self {
            Cardinality::One => "one",
            Cardinality::ZeroOrOne => "zero_or_one",
            Cardinality::OneOrMore => "one_or_more",
            Cardinality::ZeroOrMore => "zero_or_more",
        }
    }
}

/// The four spellings [`Cardinality::as_str`] writes, read back; anything
/// else is the spelling that was refused.
impl std::str::FromStr for Cardinality {
    type Err = String;

    fn from_str(spelling: &str) -> Result<Self, Self::Err> {
        [
            Cardinality::One,
            Cardinality::ZeroOrOne,
            Cardinality::OneOrMore,
            Cardinality::ZeroOrMore,
        ]
        .into_iter()
        .find(|cardinality| cardinality.as_str() == spelling)
        .ok_or_else(|| spelling.to_string())
    }
}

impl fmt::Display for Cardinality {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NodeDependency {
    pub name: Name,
    pub tag: String,
    #[serde(deserialize_with = "deserialize_node_dependency_link_id")]
    pub link_id: String,
    /// Size constraint on the slot's application-bound producer set: `one`
    /// binds exactly one producer, `zero_or_one` at most one, `one_or_more` at
    /// least one, `zero_or_more` any number. Serialized only when non-default
    /// so a manifest that omits it round-trips byte-identical.
    #[serde(default, skip_serializing_if = "Cardinality::is_one")]
    pub cardinality: Cardinality,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContractDependency {
    pub name: Name,
    pub tag: String,
    #[serde(deserialize_with = "deserialize_contract_dependency_link_id")]
    pub link_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
    /// Size constraint on the slot's application-bound producer set. Same
    /// values and default as [`NodeDependency::cardinality`]; the only
    /// per-kind difference is the conformance rule applied per bound target
    /// (contract implementation instead of `(name, tag)` identity).
    #[serde(default, skip_serializing_if = "Cardinality::is_one")]
    pub cardinality: Cardinality,
}

/// One bindable contract-implementation claim in `manifest.implements`: this
/// node satisfies `{name}:{tag}` slots. Produced-interface entries reference
/// the slot via `link_id`, which shares one flat namespace with
/// `depends_on.{nodes,contracts,pairings}` link_ids. `sha256` optionally pins
/// the contract document, with the same semantics as
/// [`ContractDependency::sha256`]. Shape-identical to [`ContractDependency`]
/// but deliberately a distinct type — the two point in opposite directions.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ImplementsEntry {
    pub name: Name,
    pub tag: String,
    #[serde(deserialize_with = "deserialize_implements_link_id")]
    pub link_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
}

/// A participant pairing slot, declared under `depends_on.pairings`: this node
/// plays `role`, owns one endpoint of every pair the slot holds, and one entry
/// covers both directions of the conversation (the topics the role emits AND
/// the counterpart-role topics it consumes) through `link_id`. Like contract
/// dependencies, a pairing slot contributes no DAG edge.
///
/// `cardinality` sizes the slot's set of pairs the way it sizes an observer
/// slot's member set: `one` holds exactly one pair, `zero_or_one` at most one,
/// `one_or_more` at least one, `zero_or_more` any number including none. Each
/// pair is one peer to this slot: established when the peer's instance
/// starts, exclusive between its two endpoints, dissolved when either instance
/// stops. The planner validates the count when the instance starts; pairs join
/// and leave a multi slot while the node runs.
///
/// `zero_or_one` is the node's own, deployment-independent statement that the
/// slot may run with no peer. It gates the launcher: only such a slot may be
/// written `{ vacant: "<why>" }` in a deployment's `links`, and a slot left
/// out of `links` entirely is uncovered either way, so forgetting to link a
/// slot stays an error. The manifest says "may be empty"; the deployment says
/// "is empty here, because".
#[derive(Debug, Clone, Serialize)]
pub struct PairingParticipantDependency {
    pub name: Name,
    pub tag: String,
    pub role: String,
    pub link_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
    /// Size constraint on the slot's set of pairs. Same four spellings and the
    /// same default as [`NodeDependency::cardinality`]. Serialized only when
    /// set so a manifest that omits it round-trips byte-identical.
    #[serde(default, skip_serializing_if = "Cardinality::is_one")]
    pub cardinality: Cardinality,
}

/// An observer pairing slot, declared under `depends_on.pairing_observers`:
/// this node plays no role and claims no endpoint. It passively consumes the
/// topics emitted by `role`, listed in `interfaces.topics.consumes` keyed by
/// `link_id`. The list it lives in states the referent, so `role` here names
/// the role being observed rather than one this node plays.
///
/// The `link_id` is a local-only resolution name (never stamped on the wire),
/// but it stays wire-safe like every other pairing link_id so both slot kinds
/// share one parser.
///
/// `cardinality` sizes the slot's observed member set the same way it sizes a
/// producer-binding slot's bound set: `one` observes exactly one pairing,
/// `zero_or_one` at most one, `one_or_more` at least one, `zero_or_more` any
/// number including none. The planner validates the count; the member set the
/// daemon delivers changes while the node runs.
///
/// `zero_or_one` is the node's own statement that the slot may observe
/// nothing, and it is the one observer cardinality a deployment may write
/// `{ vacant: "<why>" }` on. Like `one`, it is scalar-shaped, so the slot
/// still needs an explicit `links` entry: an omitted `zero_or_one` slot is
/// uncovered, which keeps "forgot" distinguishable from "decided".
#[derive(Debug, Clone, Serialize)]
pub struct PairingObserverDependency {
    pub name: Name,
    pub tag: String,
    pub role: String,
    pub link_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
    /// Size constraint on the slot's observed member set. Same four spellings
    /// and the same default as [`NodeDependency::cardinality`]; the per-kind
    /// difference is that an observed set changes while the node runs, so only
    /// the count the planner accepts is validated.
    #[serde(default, skip_serializing_if = "Cardinality::is_one")]
    pub cardinality: Cardinality,
}

/// The wire shape both pairing slot kinds deserialize through. `role` is
/// `Option` only so a missing one produces the targeted
/// `PairingSlotMissingRole` error instead of serde's generic message.
///
/// `optional` is read only to refuse it by name: both slot kinds say "may run
/// empty" with `cardinality: "zero_or_one"`, and the targeted
/// `OptionalOnPairingSlot` error names that spelling.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawPairingSlot {
    name: Name,
    tag: String,
    role: Option<String>,
    #[serde(deserialize_with = "deserialize_pairing_dependency_link_id")]
    link_id: String,
    sha256: Option<String>,
    cardinality: Option<Cardinality>,
    optional: Option<bool>,
}

impl RawPairingSlot {
    /// The check shared by both slot kinds: a present, non-empty `role`.
    /// Returns the validated role; the caller owns whatever its own kind
    /// additionally requires.
    fn validated_role<E: de::Error>(&self, context: &'static str) -> Result<String, E> {
        let role = self.role.as_deref().ok_or_else(|| {
            de::Error::custom(
                crate::error::StructuredError::PairingSlotMissingRole {
                    link_id: self.link_id.clone(),
                }
                .json5_message(),
            )
        })?;
        validate_non_empty_identifier(role, context).map_err(de::Error::custom)
    }

    /// Both slot kinds spell "may run empty" as `cardinality: "zero_or_one"`;
    /// an `optional` key is refused by name whatever its value.
    fn reject_optional<E: de::Error>(&self, section: &'static str) -> Result<(), E> {
        if self.optional.is_none() {
            return Ok(());
        }
        Err(de::Error::custom(
            crate::error::StructuredError::OptionalOnPairingSlot {
                link_id: self.link_id.clone(),
                section: section.to_string(),
            }
            .json5_message(),
        ))
    }
}

impl<'de> Deserialize<'de> for PairingParticipantDependency {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw = RawPairingSlot::deserialize(deserializer)?;
        let role = raw.validated_role("PairingParticipantDependency.role")?;
        raw.reject_optional("depends_on.pairings")?;
        Ok(Self {
            name: raw.name,
            tag: raw.tag,
            role,
            link_id: raw.link_id,
            sha256: raw.sha256,
            cardinality: raw.cardinality.unwrap_or_default(),
        })
    }
}

impl<'de> Deserialize<'de> for PairingObserverDependency {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw = RawPairingSlot::deserialize(deserializer)?;
        let role = raw.validated_role("PairingObserverDependency.role")?;
        raw.reject_optional("depends_on.pairing_observers")?;
        Ok(Self {
            name: raw.name,
            tag: raw.tag,
            role,
            link_id: raw.link_id,
            sha256: raw.sha256,
            cardinality: raw.cardinality.unwrap_or_default(),
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DependsOn {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub nodes: Vec<NodeDependency>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub contracts: Vec<ContractDependency>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pairings: Vec<PairingParticipantDependency>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pairing_observers: Vec<PairingObserverDependency>,
}

impl DependsOn {
    /// Every pairing slot link_id, participants first then observers. The two
    /// lists share one link_id namespace with the rest of `depends_on`, so
    /// namespace-wide rules read them together.
    pub fn pairing_link_ids(&self) -> impl Iterator<Item = &str> {
        self.pairings
            .iter()
            .map(|p| p.link_id.as_str())
            .chain(self.pairing_observers.iter().map(|o| o.link_id.as_str()))
    }
}

/// What an operator does with the URL of an endpoint a node serves. It
/// drives how every listing groups and headlines the endpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EndpointKind {
    /// Opened in a browser.
    Page,
    /// Attached to by an MCP client.
    Mcp,
}

impl EndpointKind {
    /// The manifest spelling of the kind.
    pub fn as_str(self) -> &'static str {
        match self {
            EndpointKind::Page => "page",
            EndpointKind::Mcp => "mcp",
        }
    }

    /// The accepted manifest spellings, quoted in the refusal of any other
    /// value.
    pub const ACCEPTED: &'static str =
        "`page` (opened in a browser) or `mcp` (attached to by an MCP client)";

    fn parse(kind: &str) -> Option<Self> {
        match kind {
            "page" => Some(EndpointKind::Page),
            "mcp" => Some(EndpointKind::Mcp),
            _ => None,
        }
    }
}

impl fmt::Display for EndpointKind {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The label of an endpoint in `execution.endpoints`: ASCII lowercase
/// letters, digits and underscores, starting with a letter. A node announces
/// the socket it bound under this label, and every listing names the endpoint
/// by it.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
#[serde(into = "String")]
pub struct EndpointLabel(String);

impl EndpointLabel {
    pub fn new(label: impl Into<String>) -> std::result::Result<Self, ParsingError> {
        let label = label.into();
        if Self::is_valid(&label) {
            Ok(Self(label))
        } else {
            Err(ParsingError::InvalidEndpointLabel { label })
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    fn is_valid(label: &str) -> bool {
        let mut chars = label.chars();
        chars.next().is_some_and(|first| first.is_ascii_lowercase())
            && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
    }
}

impl From<EndpointLabel> for String {
    fn from(label: EndpointLabel) -> Self {
        label.0
    }
}

impl std::borrow::Borrow<str> for EndpointLabel {
    fn borrow(&self) -> &str {
        &self.0
    }
}

impl AsRef<str> for EndpointLabel {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for EndpointLabel {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// One socket a node binds during setup, as `execution.endpoints` declares
/// it. The declaration says that a socket is served under the label, never
/// where: the node decides the address at bind time and announces what it
/// got, and the daemon reports the URLs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct EndpointDeclaration {
    pub kind: EndpointKind,
    /// Non-empty prose saying what the endpoint serves, for the reader of
    /// the manifest. It stays in the manifest: the daemon reports an
    /// endpoint by label and kind, and no listing carries this text today.
    pub description: String,
}

/// The endpoint declarations of a manifest, keyed by label.
pub type EndpointDeclarations = BTreeMap<EndpointLabel, EndpointDeclaration>;

/// Deserializes `execution.endpoints`, refusing a label that breaks the label
/// rule, a kind other than the two accepted ones, and a missing or empty
/// description, each as a structured error naming the label.
fn deserialize_endpoints<'de, D>(deserializer: D) -> Result<EndpointDeclarations, D::Error>
where
    D: Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct RawEndpointDeclaration {
        kind: Option<String>,
        description: Option<String>,
    }

    let raw: BTreeMap<String, RawEndpointDeclaration> = BTreeMap::deserialize(deserializer)?;
    let structured =
        |error: crate::error::StructuredError| de::Error::custom(error.json5_message());
    let mut endpoints = BTreeMap::new();
    for (label, declaration) in raw {
        let label = EndpointLabel::new(label.clone()).map_err(|_| {
            structured(crate::error::StructuredError::InvalidEndpointLabel { label })
        })?;
        let kind = declaration
            .kind
            .as_deref()
            .and_then(EndpointKind::parse)
            .ok_or_else(|| {
                structured(crate::error::StructuredError::InvalidEndpointKind {
                    label: label.to_string(),
                    kind: declaration.kind.clone().unwrap_or_default(),
                })
            })?;
        let description = declaration
            .description
            .filter(|description| !description.trim().is_empty())
            .ok_or_else(|| {
                structured(crate::error::StructuredError::EmptyEndpointDescription {
                    label: label.to_string(),
                })
            })?;
        endpoints.insert(label, EndpointDeclaration { kind, description });
    }
    Ok(endpoints)
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
    /// The sockets this node binds during setup, keyed by label. Empty for a
    /// node that serves nothing.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub endpoints: EndpointDeclarations,
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
            #[serde(default, deserialize_with = "deserialize_endpoints")]
            endpoints: EndpointDeclarations,
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
            endpoints: raw.endpoints,
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
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub implements: Vec<ImplementsEntry>,
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

impl Interfaces {
    /// The produced (emits/exposes) entries of `kind`, projected to
    /// `(link_id, name)` pairs — the shared traversal used by parse-time
    /// and plan-phase validation. Only the requested kind's section
    /// contributes entries; the chain over all three exists to give the
    /// arms a single concrete iterator type.
    pub fn produced(&self, kind: InterfaceKind) -> impl Iterator<Item = (Option<&str>, &str)> {
        let topics: &[EmittedTopic] = match kind {
            InterfaceKind::Topic => self
                .topics
                .as_ref()
                .and_then(|t| t.emits.as_deref())
                .unwrap_or_default(),
            _ => &[],
        };
        let services: &[ExposedService] = match kind {
            InterfaceKind::Service => self
                .services
                .as_ref()
                .and_then(|s| s.exposes.as_deref())
                .unwrap_or_default(),
            _ => &[],
        };
        let actions: &[ExposedAction] = match kind {
            InterfaceKind::Action => self
                .actions
                .as_ref()
                .and_then(|a| a.exposes.as_deref())
                .unwrap_or_default(),
            _ => &[],
        };
        topics
            .iter()
            .map(|e| (e.link_id(), e.name()))
            .chain(services.iter().map(|e| (e.link_id(), e.name())))
            .chain(actions.iter().map(|e| (e.link_id(), e.name())))
    }

    /// The consumed entries of `kind`, projected to `(link_id, name,
    /// refines)` — `refines` says the entry carries a `refine` block, which
    /// parse-time validation checks against the slot it names. Same shape as
    /// [`Interfaces::produced`].
    pub fn consumed(&self, kind: InterfaceKind) -> impl Iterator<Item = (&str, &str, bool)> {
        let topics: &[ConsumedTopic] = match kind {
            InterfaceKind::Topic => self
                .topics
                .as_ref()
                .and_then(|t| t.consumes.as_deref())
                .unwrap_or_default(),
            _ => &[],
        };
        let services: &[ConsumedService] = match kind {
            InterfaceKind::Service => self
                .services
                .as_ref()
                .and_then(|s| s.consumes.as_deref())
                .unwrap_or_default(),
            _ => &[],
        };
        let actions: &[ConsumedAction] = match kind {
            InterfaceKind::Action => self
                .actions
                .as_ref()
                .and_then(|a| a.consumes.as_deref())
                .unwrap_or_default(),
            _ => &[],
        };
        topics
            .iter()
            .map(|c| (c.link_id.as_str(), c.name.as_str(), c.refine.is_some()))
            .chain(
                services
                    .iter()
                    .map(|c| (c.link_id.as_str(), c.name.as_str(), c.refine.is_some())),
            )
            .chain(
                actions
                    .iter()
                    .map(|c| (c.link_id.as_str(), c.name.as_str(), c.refine.is_some())),
            )
    }

    /// The natively-declared entries of `topics.emits`: the node owns these
    /// shapes. Document-backed entries are skipped — their shapes live in the
    /// contract or pairing document the entry's slot resolves to.
    pub fn native_emits(&self) -> impl Iterator<Item = &NativeEmittedTopic> {
        self.topics
            .as_ref()
            .and_then(|t| t.emits.as_deref())
            .unwrap_or_default()
            .iter()
            .filter_map(|e| e.as_native())
    }

    /// The natively-declared entries of `services.exposes`. Same
    /// native-only contract as [`Interfaces::native_emits`].
    pub fn native_service_exposes(&self) -> impl Iterator<Item = &NativeExposedService> {
        self.services
            .as_ref()
            .and_then(|s| s.exposes.as_deref())
            .unwrap_or_default()
            .iter()
            .filter_map(|e| e.as_native())
    }

    /// The natively-declared entries of `actions.exposes`. Same
    /// native-only contract as [`Interfaces::native_emits`].
    pub fn native_action_exposes(&self) -> impl Iterator<Item = &NativeExposedAction> {
        self.actions
            .as_ref()
            .and_then(|a| a.exposes.as_deref())
            .unwrap_or_default()
            .iter()
            .filter_map(|e| e.as_native())
    }

    /// Every document-backed produced entry across the three sections,
    /// tagged as the [`LinkedMember`] of the section it is declared under:
    /// the single traversal for the `manifest.implements` resolution and
    /// coverage checks, and for the pairing emit-coverage check.
    ///
    /// Both slot kinds are yielded: callers that care about only one
    /// partition the stream by looking each `link_id` up in the manifest.
    pub fn linked_entries(&self) -> impl Iterator<Item = LinkedMember<'_>> {
        let topics = self
            .topics
            .as_ref()
            .and_then(|t| t.emits.as_deref())
            .unwrap_or_default()
            .iter()
            .filter_map(|e| e.as_linked())
            .map(LinkedMember::Topic);
        let services = self
            .services
            .as_ref()
            .and_then(|s| s.exposes.as_deref())
            .unwrap_or_default()
            .iter()
            .filter_map(|e| e.as_linked())
            .map(LinkedMember::Service);
        let actions = self
            .actions
            .as_ref()
            .and_then(|a| a.exposes.as_deref())
            .unwrap_or_default()
            .iter()
            .filter_map(|e| e.as_linked())
            .map(LinkedMember::Action);
        topics.chain(services).chain(actions)
    }
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

impl Normalize for FieldRefinement {
    fn normalize(&mut self) {
        match self {
            FieldRefinement::Length(_) => {}
            FieldRefinement::Items(nested) | FieldRefinement::Object(nested) => nested.normalize(),
        }
    }
}

impl Normalize for FormatRefinement {
    fn normalize(&mut self) {
        for value in self.0.values_mut() {
            value.normalize();
        }
        self.0.sort_keys();
    }
}

impl Normalize for TopicRefinement {
    fn normalize(&mut self) {
        self.message_format.normalize();
    }
}

impl Normalize for ServiceRefinement {
    fn normalize(&mut self) {
        normalize_opt(&mut self.request_message_format);
        normalize_opt(&mut self.response_message_format);
    }
}

impl Normalize for ResultServiceRefinement {
    fn normalize(&mut self) {
        self.response_message_format.normalize();
    }
}

impl Normalize for ActionRefinement {
    fn normalize(&mut self) {
        normalize_opt(&mut self.goal_service);
        normalize_opt(&mut self.feedback_topic);
        normalize_opt(&mut self.result_service);
    }
}

impl<T: Normalize> Normalize for Box<T> {
    fn normalize(&mut self) {
        (**self).normalize();
    }
}

impl<R: Normalize> Normalize for LinkedEntry<R> {
    fn normalize(&mut self) {
        normalize_opt(&mut self.refine);
    }
}

impl Normalize for EmittedTopic {
    fn normalize(&mut self) {
        match self {
            Self::Linked(linked) => linked.normalize(),
            Self::Native(native) => normalize_opt(&mut native.message_format),
        }
    }
}

impl Normalize for ConsumedTopic {
    fn normalize(&mut self) {
        normalize_opt(&mut self.refine);
    }
}

impl Normalize for ExposedService {
    fn normalize(&mut self) {
        match self {
            Self::Linked(linked) => linked.normalize(),
            Self::Native(native) => {
                normalize_opt(&mut native.request_message_format);
                normalize_opt(&mut native.response_message_format);
            }
        }
    }
}

impl Normalize for ConsumedService {
    fn normalize(&mut self) {
        normalize_opt(&mut self.refine);
    }
}

impl Normalize for ConsumedAction {
    fn normalize(&mut self) {
        normalize_opt(&mut self.refine);
    }
}

impl Normalize for GoalServiceEndpoint {
    fn normalize(&mut self) {
        normalize_opt(&mut self.request_message_format);
        normalize_opt(&mut self.response_message_format);
    }
}

impl Normalize for ResultServiceEndpoint {
    fn normalize(&mut self) {
        normalize_opt(&mut self.response_message_format);
    }
}

impl Normalize for ActionTopicEndpoint {
    fn normalize(&mut self) {
        self.message_format.normalize();
    }
}

impl Normalize for ExposedAction {
    fn normalize(&mut self) {
        match self {
            Self::Linked(linked) => linked.normalize(),
            Self::Native(native) => {
                normalize_opt(&mut native.goal_service);
                normalize_opt(&mut native.feedback_topic);
                normalize_opt(&mut native.result_service);
            }
        }
    }
}

impl Normalize for TopicInterfaces {
    fn normalize(&mut self) {
        normalize_opt_vec(&mut self.emits, |a, b| {
            a.name()
                .cmp(b.name())
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
            a.name()
                .cmp(b.name())
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
            a.name()
                .cmp(b.name())
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

        assert_eq!(action.name(), "move_arm");
        let action = action.as_native().expect("no link_id means native");
        assert!(
            action.feedback_topic.is_none(),
            "feedback_topic is optional"
        );

        let result = action
            .result_service
            .as_ref()
            .expect("result_service is present");
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
    fn result_service_accepts_only_a_response_payload() {
        let json5 = r#"{
            name: "perform_scan",
            result_service: {
                response_message_format: {
                    success: "bool",
                },
            },
        }"#;

        let action: ExposedAction = serde_json5::from_str(json5)
            .expect("result_service with only a response payload must parse");
        let action = action.as_native().expect("no link_id means native");
        let result = action
            .result_service
            .as_ref()
            .expect("result_service is present");
        assert!(result.response_message_format.is_some());
    }

    #[test]
    fn result_service_rejects_a_declared_request_message_format() {
        // The action protocol has no user-defined result request: `get_result`
        // is correlated by the framework-owned `goal_id`. Declaring one must
        // fail at parse time rather than being silently ignored.
        let json5 = r#"{
            name: "perform_scan",
            result_service: {
                request_message_format: {
                    include_diagnostics: "bool",
                },
                response_message_format: {
                    success: "bool",
                },
            },
        }"#;

        let err = serde_json5::from_str::<ExposedAction>(json5)
            .expect_err("result_service.request_message_format must be rejected");
        let message = err.to_string();
        // The message names the offending field and shows the result endpoint's
        // only supported field, so the `result_service` context is unambiguous.
        assert!(
            message.contains("request_message_format"),
            "error should name the rejected field, got: {message}"
        );
        assert!(
            message.contains("response_message_format"),
            "error should point to the supported result payload, got: {message}"
        );
    }

    #[test]
    fn exposed_action_rejects_empty_message_format_object() {
        let json5 = r#"{
            name: "calibrate",
            goal_service: {
                request_message_format: {},
                response_message_format: { accepted: "bool" },
            },
        }"#;

        let err = serde_json5::from_str::<ExposedAction>(json5)
            .expect_err("empty `{}` message format must be rejected");
        assert!(
            err.to_string().contains("cannot be empty"),
            "error should explain the empty format, got: {err}"
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
    fn optional_scalar_is_rejected() {
        for token in [
            "bool", "u8", "u16", "u32", "u64", "i8", "i16", "i32", "i64", "f32", "f64",
        ] {
            let json5 = format!(r#"{{ maybe: {{ $type: "{token}", $optional: true }} }}"#);
            assert!(
                serde_json5::from_str::<MessageFormat>(&json5).is_err(),
                "`$optional` on `{token}` must be rejected"
            );
        }
    }

    #[test]
    fn optional_scalar_error_names_the_type() {
        let json5 = r#"{ maybe_code: { $type: "u32", $optional: true } }"#;
        let err = serde_json5::from_str::<MessageFormat>(json5)
            .expect_err("`$optional` on `u32` must be rejected");
        let message = err.to_string();
        assert!(
            message.contains("`u32`") && message.contains("$optional"),
            "error should name the offending type, got: {message}"
        );
    }

    /// `string`, `bytes`, and `time` encode as Cap'n Proto pointers, so they
    /// are null when unset and can carry `$optional`.
    #[test]
    fn optional_pointer_backed_primitives_are_accepted() {
        for token in ["string", "bytes", "time"] {
            let json5 = format!(r#"{{ maybe: {{ $type: "{token}", $optional: true }} }}"#);
            let parsed = serde_json5::from_str::<MessageFormat>(&json5)
                .unwrap_or_else(|err| panic!("`$optional` on `{token}` must parse: {err}"));
            assert!(parsed.0["maybe"].is_optional());
        }
    }

    #[test]
    fn optional_object_and_array_are_accepted() {
        let json5 = r#"{
            header: { $type: "object", $optional: true, frame_id: "u32" },
            samples: { $type: "array", $items: "f32", $optional: true }
        }"#;

        let parsed: MessageFormat =
            serde_json5::from_str(json5).expect("pointer-backed structures may be optional");
        assert!(parsed.0["header"].is_optional());
        assert!(parsed.0["samples"].is_optional());
    }

    #[test]
    fn empty_message_format_is_rejected() {
        let err = serde_json5::from_str::<MessageFormat>("{}")
            .expect_err("an empty message format must be rejected");
        assert!(
            err.to_string().contains("cannot be empty"),
            "error should explain the empty format, got: {err}"
        );
    }

    #[test]
    fn feedback_topic_requires_a_message_format() {
        let json5 = r#"{
            name: "move_arm",
            feedback_topic: { qos_profile: "sensor_data" }
        }"#;

        assert!(
            serde_json5::from_str::<ExposedAction>(json5).is_err(),
            "a declared feedback_topic must carry a message_format"
        );
    }

    #[test]
    fn feedback_topic_rejects_an_empty_message_format() {
        let json5 = r#"{
            name: "move_arm",
            feedback_topic: { message_format: {} }
        }"#;

        let err = serde_json5::from_str::<ExposedAction>(json5)
            .expect_err("an empty feedback message_format must be rejected");
        assert!(
            err.to_string().contains("cannot be empty"),
            "error should explain the empty format, got: {err}"
        );
    }

    #[test]
    fn feedback_topic_with_a_message_format_parses() {
        let json5 = r#"{
            name: "move_arm",
            feedback_topic: {
                qos_profile: "sensor_data",
                message_format: { current_position: { $type: "array", $items: "i32", $length: 3 } }
            }
        }"#;

        let action: ExposedAction = serde_json5::from_str(json5).expect("should parse");
        let feedback = action
            .as_native()
            .expect("no link_id means native")
            .feedback_topic
            .as_ref()
            .expect("feedback_topic is present");
        assert_eq!(feedback.qos_profile, QoSProfile::SensorData);
        assert!(feedback.message_format.0.contains_key("current_position"));
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
        assert!(deps.contracts.is_empty());
    }

    #[test]
    fn depends_on_with_contracts_full() {
        let json5 = r#"{
            nodes: [],
            contracts: [
                { name: "depth_camera", tag: "v1", sha256: "aaa", link_id: "depth_camera" }
            ]
        }"#;
        let deps: DependsOn = serde_json5::from_str(json5).expect("should parse");
        assert!(deps.nodes.is_empty());
        assert_eq!(deps.contracts.len(), 1);
        assert_eq!(deps.contracts[0].name.as_str(), "depth_camera");
        assert_eq!(deps.contracts[0].tag, "v1");
        assert_eq!(deps.contracts[0].link_id, "depth_camera");
        assert_eq!(deps.contracts[0].sha256.as_deref(), Some("aaa"));
    }

    #[test]
    fn depends_on_with_contracts_no_sha256() {
        let json5 = r#"{
            nodes: [],
            contracts: [
                { name: "depth_camera", tag: "v1", link_id: "depth_camera" }
            ]
        }"#;
        let deps: DependsOn = serde_json5::from_str(json5).expect("should parse");
        assert_eq!(deps.contracts.len(), 1);
        assert!(deps.contracts[0].sha256.is_none());
    }

    #[test]
    fn depends_on_contracts_requires_name() {
        let json5 = r#"{
            nodes: [],
            contracts: [{ tag: "v1", link_id: "depth_camera" }]
        }"#;
        assert!(serde_json5::from_str::<DependsOn>(json5).is_err());
    }

    #[test]
    fn depends_on_contracts_requires_tag() {
        let json5 = r#"{
            nodes: [],
            contracts: [{ name: "depth_camera", link_id: "depth_camera" }]
        }"#;
        assert!(serde_json5::from_str::<DependsOn>(json5).is_err());
    }

    #[test]
    fn depends_on_contracts_requires_link_id() {
        let json5 = r#"{
            nodes: [],
            contracts: [{ name: "depth_camera", tag: "v1" }]
        }"#;
        assert!(serde_json5::from_str::<DependsOn>(json5).is_err());
    }

    #[test]
    fn depends_on_contracts_rejects_unknown_fields() {
        let json5 = r#"{
            nodes: [],
            contracts: [
                { name: "depth_camera", tag: "v1", link_id: "depth_camera", extra: "bad" }
            ]
        }"#;
        assert!(serde_json5::from_str::<DependsOn>(json5).is_err());
    }

    #[test]
    fn depends_on_with_only_contracts() {
        let json5 = r#"{
            contracts: [
                { name: "uvc_camera", tag: "v1", link_id: "camera" }
            ]
        }"#;
        let deps: DependsOn = serde_json5::from_str(json5).expect("nodes should be optional");
        assert!(deps.nodes.is_empty());
        assert_eq!(deps.contracts.len(), 1);
        assert_eq!(deps.contracts[0].link_id, "camera");
    }

    /// `cardinality` parses on all four slot kinds that take it, with the
    /// same values and the same default of `one` when omitted.
    #[test]
    fn depends_on_cardinality_parses_on_every_slot_kind() {
        let json5 = r#"{
            nodes: [
                { name: "robot_arm", tag: "v1", link_id: "arms", cardinality: "one_or_more" },
                { name: "logger", tag: "v1", link_id: "log" }
            ],
            contracts: [
                { name: "uvc_camera", tag: "v1", link_id: "camera", cardinality: "zero_or_more" },
                { name: "depth_camera", tag: "v1", link_id: "depth", cardinality: "one" }
            ],
            pairings: [
                { name: "arm_link", tag: "v1", role: "arm", link_id: "controllers", cardinality: "zero_or_more" },
                { name: "arm_link", tag: "v1", role: "controller", link_id: "arm" }
            ],
            pairing_observers: [
                { name: "arm_link", tag: "v1", role: "arm", link_id: "watched", cardinality: "one_or_more" },
                { name: "arm_link", tag: "v1", role: "arm", link_id: "spare", cardinality: "zero_or_more" },
                { name: "arm_link", tag: "v1", role: "arm", link_id: "sole" }
            ]
        }"#;
        let deps: DependsOn = serde_json5::from_str(json5).expect("should parse");
        assert_eq!(deps.nodes[0].cardinality, Cardinality::OneOrMore);
        assert_eq!(
            deps.nodes[1].cardinality,
            Cardinality::One,
            "omitted cardinality defaults to one"
        );
        assert_eq!(deps.contracts[0].cardinality, Cardinality::ZeroOrMore);
        assert_eq!(deps.contracts[1].cardinality, Cardinality::One);
        assert_eq!(deps.pairings[0].cardinality, Cardinality::ZeroOrMore);
        assert_eq!(
            deps.pairings[1].cardinality,
            Cardinality::One,
            "a participant slot that omits cardinality holds exactly one pair"
        );
        assert_eq!(
            deps.pairing_observers[0].cardinality,
            Cardinality::OneOrMore
        );
        assert_eq!(
            deps.pairing_observers[1].cardinality,
            Cardinality::ZeroOrMore
        );
        assert_eq!(
            deps.pairing_observers[2].cardinality,
            Cardinality::One,
            "an observer that omits cardinality observes exactly one pairing"
        );
    }

    /// Only defined spellings parse, on every slot kind that takes the key.
    #[test]
    fn depends_on_cardinality_rejects_unknown_values() {
        for value in ["many", "two", "ONE_OR_MORE", ""] {
            let contract_json5 = format!(
                r#"{{
                    contracts: [
                        {{ name: "uvc_camera", tag: "v1", link_id: "camera", cardinality: "{value}" }}
                    ]
                }}"#
            );
            assert!(
                serde_json5::from_str::<DependsOn>(&contract_json5).is_err(),
                "cardinality `{value}` should be rejected on a contract slot"
            );

            let observer_json5 = format!(
                r#"{{
                    pairing_observers: [
                        {{ name: "arm_link", tag: "v1", role: "arm", link_id: "watch", cardinality: "{value}" }}
                    ]
                }}"#
            );
            assert!(
                serde_json5::from_str::<DependsOn>(&observer_json5).is_err(),
                "cardinality `{value}` should be rejected on an observer slot"
            );

            let participant_json5 = format!(
                r#"{{
                    pairings: [
                        {{ name: "arm_link", tag: "v1", role: "controller", link_id: "arm", cardinality: "{value}" }}
                    ]
                }}"#
            );
            assert!(
                serde_json5::from_str::<DependsOn>(&participant_json5).is_err(),
                "cardinality `{value}` should be rejected on a participant slot"
            );
        }
    }

    /// `zero_or_one` sizes any slot that takes at most one source or peer,
    /// whichever list declares it: an observer slot, a participant slot, a
    /// `depends_on.nodes` slot and a `depends_on.contracts` slot all parse it
    /// into the same variant.
    #[test]
    fn zero_or_one_parses_on_every_slot_kind_that_carries_a_cardinality() {
        let participant_json5 = r#"{
            pairings: [
                { name: "arm_link", tag: "v1", role: "controller", link_id: "arm", cardinality: "zero_or_one" }
            ]
        }"#;
        let deps: DependsOn = serde_json5::from_str(participant_json5).expect("should parse");
        assert_eq!(deps.pairings[0].cardinality, Cardinality::ZeroOrOne);

        let observer_json5 = r#"{
            pairing_observers: [
                { name: "arm_link", tag: "v1", role: "arm", link_id: "watch", cardinality: "zero_or_one" }
            ]
        }"#;
        let deps: DependsOn = serde_json5::from_str(observer_json5).expect("should parse");
        assert_eq!(
            deps.pairing_observers[0].cardinality,
            Cardinality::ZeroOrOne
        );

        let node_json5 = r#"{ nodes: [ { name: "arm", tag: "v1", link_id: "arm", cardinality: "zero_or_one" } ] }"#;
        let deps: DependsOn = serde_json5::from_str(node_json5).expect("should parse");
        assert_eq!(deps.nodes[0].cardinality, Cardinality::ZeroOrOne);

        let contract_json5 = r#"{ contracts: [ { name: "uvc_camera", tag: "v1", link_id: "camera", cardinality: "zero_or_one" } ] }"#;
        let deps: DependsOn = serde_json5::from_str(contract_json5).expect("should parse");
        assert_eq!(deps.contracts[0].cardinality, Cardinality::ZeroOrOne);
    }

    /// `zero_or_one` is not the default, so it round-trips explicitly, and the
    /// observer accessor shape follows `is_scalar` rather than `is_one`.
    #[test]
    fn zero_or_one_round_trips_and_is_scalar_shaped() {
        let slot: PairingObserverDependency = serde_json5::from_str(
            r#"{ name: "arm_link", tag: "v1", role: "arm", link_id: "watch", cardinality: "zero_or_one" }"#,
        )
        .unwrap();
        let serialized = serde_json5::to_string(&slot).unwrap();
        assert!(
            serialized.contains("\"cardinality\":\"zero_or_one\""),
            "zero_or_one must round-trip: {serialized}"
        );
        let reparsed: PairingObserverDependency = serde_json5::from_str(&serialized).unwrap();
        assert_eq!(reparsed.cardinality, Cardinality::ZeroOrOne);

        assert!(Cardinality::ZeroOrOne.is_scalar());
        assert!(Cardinality::One.is_scalar());
        assert!(!Cardinality::OneOrMore.is_scalar());
        assert!(!Cardinality::ZeroOrMore.is_scalar());
        assert!(
            !Cardinality::ZeroOrOne.is_one(),
            "`zero_or_one` is not the default, so it serializes"
        );
        assert!(
            !Cardinality::ZeroOrOne.allows_empty(),
            "an omitted `zero_or_one` slot stays uncovered: only a written vacancy empties it"
        );
        assert!(Cardinality::ZeroOrOne.admits(0) && Cardinality::ZeroOrOne.admits(1));
        assert!(!Cardinality::ZeroOrOne.admits(2));

        // A seed is sized by the ceiling alone, which is what separates a slot
        // whose whole set is known up front from a pairing slot whose pairs
        // arrive after it starts.
        assert!(
            !Cardinality::OneOrMore.admits(0),
            "a whole set of none does not satisfy a floor of one"
        );
        assert!(
            Cardinality::OneOrMore.admits_seed(0),
            "a floored pairing slot still seeds empty and is paired after Running"
        );
        assert!(Cardinality::One.admits_seed(0) && Cardinality::One.admits_seed(1));
        assert!(
            !Cardinality::One.admits_seed(2),
            "a scalar slot cannot seed two pairs whatever the floor"
        );
    }

    /// An omitted cardinality is not serialized (existing manifests stay
    /// byte-identical) and an explicit non-default value round-trips.
    #[test]
    fn depends_on_cardinality_serialization_round_trip() {
        let default_dep: NodeDependency =
            serde_json5::from_str(r#"{ name: "logger", tag: "v1", link_id: "log" }"#).unwrap();
        let serialized = serde_json5::to_string(&default_dep).unwrap();
        assert!(
            !serialized.contains("cardinality"),
            "default cardinality must not be serialized: {serialized}"
        );

        let multi_dep: ContractDependency = serde_json5::from_str(
            r#"{ name: "uvc_camera", tag: "v1", link_id: "camera", cardinality: "one_or_more" }"#,
        )
        .unwrap();
        let serialized = serde_json5::to_string(&multi_dep).unwrap();
        assert!(
            serialized.contains("\"cardinality\":\"one_or_more\""),
            "non-default cardinality must round-trip: {serialized}"
        );
        let reparsed: ContractDependency = serde_json5::from_str(&serialized).unwrap();
        assert_eq!(reparsed.cardinality, Cardinality::OneOrMore);

        let optional_dep: NodeDependency = serde_json5::from_str(
            r#"{ name: "camera", tag: "v1", link_id: "wrist_camera", cardinality: "zero_or_one" }"#,
        )
        .unwrap();
        let serialized = serde_json5::to_string(&optional_dep).unwrap();
        assert!(
            serialized.contains("\"cardinality\":\"zero_or_one\""),
            "a producer slot's zero_or_one must round-trip: {serialized}"
        );
        let reparsed: NodeDependency = serde_json5::from_str(&serialized).unwrap();
        assert_eq!(reparsed.cardinality, Cardinality::ZeroOrOne);

        let default_observer: PairingObserverDependency = serde_json5::from_str(
            r#"{ name: "arm_link", tag: "v1", role: "arm", link_id: "watch" }"#,
        )
        .unwrap();
        let serialized = serde_json5::to_string(&default_observer).unwrap();
        assert!(
            !serialized.contains("cardinality"),
            "default cardinality must not be serialized on an observer: {serialized}"
        );

        let multi_observer: PairingObserverDependency = serde_json5::from_str(
            r#"{ name: "arm_link", tag: "v1", role: "arm", link_id: "watch", cardinality: "zero_or_more" }"#,
        )
        .unwrap();
        let serialized = serde_json5::to_string(&multi_observer).unwrap();
        assert!(
            serialized.contains("\"cardinality\":\"zero_or_more\""),
            "non-default observer cardinality must round-trip: {serialized}"
        );
        let reparsed: PairingObserverDependency = serde_json5::from_str(&serialized).unwrap();
        assert_eq!(reparsed.cardinality, Cardinality::ZeroOrMore);
    }

    /// An `optional` key on either pairing slot kind is a manifest error with
    /// a targeted message naming the spelling that says "may run empty".
    #[test]
    fn optional_is_rejected_on_both_pairing_slot_kinds() {
        for value in ["true", "false"] {
            for (list, section, link_id) in [
                ("pairings", "depends_on.pairings", "arm"),
                ("pairing_observers", "depends_on.pairing_observers", "watch"),
            ] {
                let role = if list == "pairings" {
                    "controller"
                } else {
                    "arm"
                };
                let json5 = format!(
                    r#"{{
                        {list}: [
                            {{ name: "arm_link", tag: "v1", role: "{role}", link_id: "{link_id}", optional: {value} }}
                        ]
                    }}"#
                );
                let parsing_err = crate::error::ParsingError::from(
                    serde_json5::from_str::<DependsOn>(&json5)
                        .expect_err("optional on a pairing slot must be rejected"),
                );
                let crate::error::ParsingError::OptionalOnPairingSlot {
                    link_id: got_link_id,
                    section: got_section,
                } = &parsing_err
                else {
                    panic!("expected OptionalOnPairingSlot, got {parsing_err:?}");
                };
                assert_eq!(
                    (got_link_id.as_str(), got_section.as_str()),
                    (link_id, section)
                );
                let msg = parsing_err.to_string();
                assert!(
                    msg.contains(r#"cardinality: "zero_or_one""#),
                    "message should name the spelling to use: {msg}"
                );
            }
        }

        // Other unknown keys still get the plain unknown-field rejection.
        let other_unknown = r#"{
            pairings: [
                { name: "arm_link", tag: "v1", role: "controller", link_id: "arm", extra: true }
            ]
        }"#;
        assert!(serde_json5::from_str::<DependsOn>(other_unknown).is_err());

        // A well-formed pairing entry still parses.
        let valid = r#"{
            pairings: [
                { name: "arm_link", tag: "v1", role: "controller", link_id: "arm" }
            ]
        }"#;
        let deps: DependsOn = serde_json5::from_str(valid).expect("valid pairing should parse");
        assert_eq!(deps.pairings[0].role, "controller");
    }

    /// Which list an entry lives in is the whole participant/observer
    /// discriminator, and both lists carry the same `role` key naming the role
    /// played or observed. Both round-trip through serialization back to the
    /// same flat shape.
    #[test]
    fn slot_kind_is_selected_by_the_depends_on_list() {
        let json5 = r#"{
            pairings: [
                { name: "arm_link", tag: "v1", role: "controller", link_id: "arm" }
            ],
            pairing_observers: [
                { name: "arm_link", tag: "v1", role: "arm", link_id: "observed_arm" }
            ]
        }"#;
        let deps: DependsOn = serde_json5::from_str(json5).expect("both lists should parse");

        assert_eq!(deps.pairings[0].role, "controller");
        assert_eq!(deps.pairings[0].link_id, "arm");
        assert_eq!(deps.pairing_observers[0].role, "arm");
        assert_eq!(deps.pairing_observers[0].link_id, "observed_arm");
        assert_eq!(
            deps.pairing_link_ids().collect::<Vec<_>>(),
            ["arm", "observed_arm"],
            "pairing_link_ids must cover both lists, participants first"
        );

        // Serialization stays flat and keeps each entry in its own list, so
        // manifests round-trip byte-identically.
        let serialized = serde_json5::to_string(&deps).expect("serialize");
        let reparsed: DependsOn = serde_json5::from_str(&serialized).expect("round-trip");
        assert_eq!(reparsed.pairings[0].role, "controller");
        assert_eq!(reparsed.pairing_observers[0].role, "arm");
    }

    /// `role` is required on both slot kinds, and gets a targeted error rather
    /// than serde's generic missing-field message.
    #[test]
    fn pairing_entry_requires_a_role() {
        for json5 in [
            r#"{ pairings: [ { name: "arm_link", tag: "v1", link_id: "x" } ] }"#,
            r#"{ pairing_observers: [ { name: "arm_link", tag: "v1", link_id: "x" } ] }"#,
        ] {
            let err = crate::error::ParsingError::from(
                serde_json5::from_str::<DependsOn>(json5)
                    .expect_err("a missing role must be rejected"),
            );
            assert!(
                matches!(&err, crate::error::ParsingError::PairingSlotMissingRole { link_id } if link_id == "x"),
                "expected PairingSlotMissingRole, got {err:?}"
            );
        }
    }

    /// An omitted `cardinality` is the default and is not serialized, so a
    /// manifest that never mentions the key round-trips byte-identical; an
    /// explicit one survives the round trip.
    #[test]
    fn participant_cardinality_serialization_round_trip() {
        let sole: PairingParticipantDependency = serde_json5::from_str(
            r#"{ name: "arm_link", tag: "v1", role: "controller", link_id: "arm" }"#,
        )
        .unwrap();
        assert_eq!(sole.cardinality, Cardinality::One);
        let serialized = serde_json5::to_string(&sole).unwrap();
        assert!(
            !serialized.contains("cardinality"),
            "the default cardinality must not be serialized: {serialized}"
        );

        let many: PairingParticipantDependency = serde_json5::from_str(
            r#"{ name: "arm_link", tag: "v1", role: "arm", link_id: "controllers", cardinality: "zero_or_more" }"#,
        )
        .unwrap();
        let serialized = serde_json5::to_string(&many).unwrap();
        assert!(
            serialized.contains("\"cardinality\":\"zero_or_more\""),
            "a non-default cardinality must round-trip: {serialized}"
        );
        let reparsed: PairingParticipantDependency = serde_json5::from_str(&serialized).unwrap();
        assert_eq!(reparsed.cardinality, Cardinality::ZeroOrMore);
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
    fn manifest_with_implements_full() {
        let json5 = r#"{
            name: "camera_node",
            tag: "v1",
            implements: [
                { name: "depth_camera", tag: "v1", link_id: "cam", sha256: "aaaa" }
            ]
        }"#;
        let manifest: Manifest = serde_json5::from_str(json5).expect("should parse");
        assert_eq!(manifest.implements.len(), 1);
        assert_eq!(manifest.implements[0].name.as_str(), "depth_camera");
        assert_eq!(manifest.implements[0].tag, "v1");
        assert_eq!(manifest.implements[0].link_id, "cam");
        assert_eq!(manifest.implements[0].sha256.as_deref(), Some("aaaa"));
    }

    #[test]
    fn manifest_implements_no_sha256() {
        let json5 = r#"{
            name: "camera_node",
            tag: "v1",
            implements: [
                { name: "depth_camera", tag: "v1", link_id: "cam" }
            ]
        }"#;
        let manifest: Manifest = serde_json5::from_str(json5).expect("should parse");
        assert!(manifest.implements[0].sha256.is_none());
    }

    #[test]
    fn manifest_without_implements_defaults_empty() {
        let json5 = r#"{ name: "plain_node", tag: "v1" }"#;
        let manifest: Manifest = serde_json5::from_str(json5).expect("should parse");
        assert!(manifest.implements.is_empty());
    }

    #[test]
    fn implements_requires_name_tag_and_link_id() {
        for entry in [
            r#"{ tag: "v1", link_id: "cam" }"#,
            r#"{ name: "depth_camera", link_id: "cam" }"#,
            r#"{ name: "depth_camera", tag: "v1" }"#,
            r#"{ name: "depth_camera", tag: "v1", link_id: "" }"#,
            r#"{ name: "depth_camera", tag: "v1", link_id: "--" }"#,
        ] {
            let json5 = format!(r#"{{ name: "n", tag: "v1", implements: [{entry}] }}"#);
            assert!(
                serde_json5::from_str::<Manifest>(&json5).is_err(),
                "implements entry should be rejected: {entry}"
            );
        }
    }

    #[test]
    fn implements_rejects_unknown_fields() {
        let json5 = r#"{
            name: "n",
            tag: "v1",
            implements: [
                { name: "depth_camera", tag: "v1", link_id: "cam", extra: "bad" }
            ]
        }"#;
        assert!(serde_json5::from_str::<Manifest>(json5).is_err());
    }

    #[test]
    fn interfaces_rejects_conforms_to_as_unknown_field() {
        // The old field is not recognized at all — plain serde unknown-field
        // rejection is the intended UX (no alias, no friendlier error).
        let json5 = r#"{
            conforms_to: [
                { name: "depth_camera", tag: "v1" }
            ]
        }"#;
        assert!(serde_json5::from_str::<Interfaces>(json5).is_err());
    }

    #[test]
    fn emitted_topic_two_variants_parse() {
        let contract: EmittedTopic =
            serde_json5::from_str(r#"{ link_id: "cam", name: "video_stream" }"#)
                .expect("contract-backed emit should parse");
        assert_eq!(contract.name(), "video_stream");
        assert_eq!(contract.link_id(), Some("cam"));
        assert!(!contract.is_native());

        let native: EmittedTopic = serde_json5::from_str(
            r#"{ name: "debug_stream", qos_profile: "sensor_data", message_format: { x: "f64" } }"#,
        )
        .expect("native emit should parse");
        assert_eq!(native.name(), "debug_stream");
        assert!(native.link_id().is_none());
        let native = native.as_native().unwrap();
        assert_eq!(native.qos_profile, QoSProfile::SensorData);
        assert!(native.message_format.is_some());
    }

    #[test]
    fn contract_backed_emit_rejects_inline_shape() {
        for entry in [
            r#"{ link_id: "cam", name: "video_stream", message_format: { x: "f64" } }"#,
            r#"{ link_id: "cam", name: "video_stream", qos_profile: "sensor_data" }"#,
        ] {
            assert!(
                serde_json5::from_str::<EmittedTopic>(entry).is_err(),
                "inline shape on contract-backed emit should be rejected: {entry}"
            );
        }
    }

    #[test]
    fn contract_backed_service_and_action_reject_inline_shape() {
        assert!(
            serde_json5::from_str::<ExposedService>(
                r#"{ link_id: "cam", name: "is_ready", request_message_format: {} }"#
            )
            .is_err()
        );
        assert!(
            serde_json5::from_str::<ExposedAction>(
                r#"{ link_id: "arm", name: "do_move", goal_service: {} }"#
            )
            .is_err()
        );
    }

    #[test]
    fn produced_entries_require_non_empty_name() {
        assert!(serde_json5::from_str::<EmittedTopic>(r#"{ name: "" }"#).is_err());
        assert!(serde_json5::from_str::<EmittedTopic>(r#"{}"#).is_err());
        assert!(serde_json5::from_str::<EmittedTopic>(r#"{ link_id: "cam", name: "" }"#).is_err());
        assert!(serde_json5::from_str::<ExposedService>(r#"{ name: "   " }"#).is_err());
        assert!(serde_json5::from_str::<ExposedAction>(r#"{ name: "" }"#).is_err());
    }

    #[test]
    fn produced_entries_round_trip_both_variants() {
        for source in [
            r#"{ link_id: "cam", name: "video_stream" }"#,
            r#"{ name: "debug_stream", qos_profile: "sensor_data", message_format: { x: "f64" } }"#,
        ] {
            let parsed: EmittedTopic = serde_json5::from_str(source).expect("should parse");
            let serialized = serde_json5::to_string(&parsed).expect("should serialize");
            let reparsed: EmittedTopic =
                serde_json5::from_str(&serialized).expect("round-trip should re-parse");
            assert_eq!(parsed, reparsed, "round-trip mismatch for {source}");
        }

        let svc: ExposedService = serde_json5::from_str(
            r#"{ name: "native_svc", request_message_format: { id: "u8" }, response_message_format: { ok: "bool" } }"#,
        )
        .expect("should parse");
        let reparsed: ExposedService =
            serde_json5::from_str(&serde_json5::to_string(&svc).unwrap()).unwrap();
        assert_eq!(svc, reparsed);

        let action: ExposedAction =
            serde_json5::from_str(r#"{ link_id: "arm", name: "do_move" }"#).expect("should parse");
        let reparsed: ExposedAction =
            serde_json5::from_str(&serde_json5::to_string(&action).unwrap()).unwrap();
        assert_eq!(action, reparsed);
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
            peppy_schema: "node/v1",
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
            peppy_schema: "launcher/v1",
            manifest: { name: "node", tag: "v1" },
            execution: { language: "rust", build_cmd: ["true"], run_cmd: ["true"] }
        }"#;
        let err = serde_json5::from_str::<NodeConfig>(json5)
            .expect_err("launcher/v1 schema must be rejected");
        assert!(
            err.to_string().contains("node/v1"),
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
                    refine: None,
                },
                ConsumedTopic {
                    link_id: "node_a".into(),
                    name: "topic".into(),
                    refine: None,
                },
            ]),
        };
        let mut topics_b = TopicInterfaces {
            emits: None,
            consumes: Some(vec![
                ConsumedTopic {
                    link_id: "node_a".into(),
                    name: "topic".into(),
                    refine: None,
                },
                ConsumedTopic {
                    link_id: "node_b".into(),
                    name: "topic".into(),
                    refine: None,
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
                    refine: None,
                },
                ConsumedService {
                    link_id: "node_a".into(),
                    name: "svc".into(),
                    refine: None,
                },
            ]),
        };
        let mut services_b = ServiceInterfaces {
            exposes: None,
            consumes: Some(vec![
                ConsumedService {
                    link_id: "node_a".into(),
                    name: "svc".into(),
                    refine: None,
                },
                ConsumedService {
                    link_id: "node_b".into(),
                    name: "svc".into(),
                    refine: None,
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
                    refine: None,
                },
                ConsumedAction {
                    link_id: "node_a".into(),
                    name: "act".into(),
                    refine: None,
                },
            ]),
        };
        let mut actions_b = ActionInterfaces {
            exposes: None,
            consumes: Some(vec![
                ConsumedAction {
                    link_id: "node_a".into(),
                    name: "act".into(),
                    refine: None,
                },
                ConsumedAction {
                    link_id: "node_b".into(),
                    name: "act".into(),
                    refine: None,
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
