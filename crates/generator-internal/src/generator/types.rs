use crate::error::{Error, Result};
use crate::generator::common::CrateDeployMode;
use crate::generator::naming::{array_item_type_name, to_camel_case};
use config::node::{
    ConsumedAction, ConsumedService, ConsumedTopic, EmittedTopic, ExposedAction, ExposedService,
    MessageFormat, PrimitiveSchema, SchemaType, TypeToken,
};
use daemon_config::consts::PeppyDirs;
use indexmap::IndexMap;
use std::collections::HashMap;
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InterfaceKind {
    EmittedTopic,
    ExposedService,
    ExposedAction,
    ConsumedTopic,
    ConsumedService,
    ConsumedAction,
    PeerEmittedTopic,
    PeerConsumedTopic,
}

/// The message formats a consumer needs to talk to a producer's action: the goal request, the
/// feedback topic format, and the result response. The goal acknowledgement is framework-owned
/// (see [`goal_action_response_format`]) and there is no result-request wire message, so neither
/// is carried here.
#[derive(Debug, Clone)]
pub struct ConsumedActionMessage {
    pub goal_request: Option<MessageFormat>,
    pub feedback: Option<MessageFormat>,
    pub result_response: Option<MessageFormat>,
}

/// Identifies the `conforms_to` interface a producer artifact was pulled from.
///
/// `None` on a producer variant means the artifact is the node's own (native)
/// declaration; `Some` means it was contributed by a [`PeppyContract`] pulled
/// via `interfaces.conforms_to`. The pair `(iface_name, iface_tag)` drives both
/// the generated module nesting (`emitted_topics::{iface_name}::{iface_tag}::{topic}`)
/// and the two extra Zenoh segments on the wire path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InterfaceOrigin {
    pub iface_name: String,
    pub iface_tag: String,
}

impl InterfaceOrigin {
    /// Module path for an artifact contributed by this origin:
    /// `[iface_name, sanitized_tag, leaf_name]`.
    pub fn module_path_for(&self, leaf_name: &str) -> Vec<String> {
        vec![
            self.iface_name.clone(),
            crate::generator::naming::sanitize_iface_tag(&self.iface_tag),
            leaf_name.to_string(),
        ]
    }

    /// Namespaces `local` with `iface_name` + sanitized tag so a leaf name
    /// shared across conformed origins produces distinct schema keys.
    pub fn scoped_schema_key(&self, local: &str) -> String {
        format!(
            "{}_{}_{}",
            self.iface_name,
            crate::generator::naming::sanitize_iface_tag(&self.iface_tag),
            local
        )
    }
}

/// Builds a schema key scoped to `origin` when `Some`, falling back to the
/// raw `local` key when the artifact is the node's own (native) declaration.
pub fn scoped_schema_key(origin: Option<&InterfaceOrigin>, local: &str) -> String {
    match origin {
        Some(o) => o.scoped_schema_key(local),
        None => local.to_string(),
    }
}

/// Identifies the pairing slot a peer topic belongs to. "Pairing" names the
/// mechanism/contract/slot; "peer" names the other end. Both directions of a
/// pairing live under the slot's module (`pairings/<link_id>/<topic>`), so the
/// module path is flat `[link_id, topic]` — deliberately NOT reusing
/// [`InterfaceOrigin`], whose `[name, tag, leaf]` nesting reflects contract
/// identity rather than slot identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerContext {
    /// The node's own pairing-slot link_id (`depends_on.pairings[].link_id`).
    pub link_id: String,
    pub pairing_name: String,
    pub pairing_tag: String,
}

impl PeerContext {
    /// Module path for a topic of this slot: `[link_id, topic]`.
    pub fn module_path_for(&self, topic_name: &str) -> Vec<String> {
        vec![self.link_id.clone(), topic_name.to_string()]
    }
}

/// Backstop for the pairing document's flat topic-name uniqueness rule:
/// two peer artifacts must never land on the same `pairings/<link_id>/<topic>`
/// module path. Shared by the Rust and Python generators so the invariant
/// cannot drift between them.
pub fn ensure_no_peer_collision(
    sections: &[InterfaceArtifact],
    module_path: &[String],
    peer: &PeerContext,
    topic: &EmittedTopic,
) -> Result<()> {
    let collides = sections.iter().any(|s| {
        matches!(
            s.kind,
            InterfaceKind::PeerEmittedTopic | InterfaceKind::PeerConsumedTopic
        ) && s.module_path == module_path
    });
    if collides {
        return Err(Error::PeerTopicNameCollision {
            link_id: peer.link_id.clone(),
            topic: topic.name.clone(),
        });
    }
    Ok(())
}

/// Identifies a dependency a consumer pulls from. `producer_name` +
/// `producer_tag` pin the labelled producer for codegen: a real node for
/// `depends_on.nodes`, or the interface's `(name, tag)` when the consumer
/// pulls in via `depends_on.contracts` (there's no producer node in that
/// case, but the labels still need a stable identity). `origin` is `Some`
/// when the consumed artifact carries the `interface`-shaped wire
/// discriminator (either because the producer node `conforms_to` an
/// interface or because the dependency itself is a contract document).
///
/// `link_id` is the consumer manifest slot whose runtime bindings resolve
/// this dependency's producers. In the harmonized wire model the consumer
/// never pins by `link_id` on the wire (producers always advertise the `_`
/// sentinel); instead the generated call sites splice
/// `processor().consumer_filter(<link_id>)` /
/// `processor().require_pinned_producer(<link_id>)` lookups.
///
/// [`SenderTarget::Interface`]: pmi::SenderTarget::Interface
/// [`SenderTarget::Node`]: pmi::SenderTarget::Node
#[derive(Debug, Clone)]
pub struct DependencyContext {
    pub producer_name: String,
    pub producer_tag: String,
    pub origin: Option<InterfaceOrigin>,
    pub link_id: String,
}

impl DependencyContext {
    /// Build a context for a node dependency that emits natively (no
    /// `conforms_to`).
    pub fn native(
        node_name: impl Into<String>,
        node_tag: impl Into<String>,
        link_id: impl Into<String>,
    ) -> Self {
        Self {
            producer_name: node_name.into(),
            producer_tag: node_tag.into(),
            origin: None,
            link_id: link_id.into(),
        }
    }

    /// Build a context for a node dependency that emits via a
    /// `conforms_to` interface.
    pub fn conformed(
        node_name: impl Into<String>,
        node_tag: impl Into<String>,
        origin: InterfaceOrigin,
        link_id: impl Into<String>,
    ) -> Self {
        Self {
            producer_name: node_name.into(),
            producer_tag: node_tag.into(),
            origin: Some(origin),
            link_id: link_id.into(),
        }
    }

    /// Build a context for a `depends_on.contracts` dependency. There is
    /// no producer node here; `producer_name` / `producer_tag` carry the
    /// interface's `(name, tag)` so codegen labels stay readable, and
    /// `origin` is set to the same `(name, tag)` so consumer-side codegen
    /// reuses the existing `SenderTarget::Interface` path.
    pub fn interface(
        iface_name: impl Into<String>,
        iface_tag: impl Into<String>,
        link_id: impl Into<String>,
    ) -> Self {
        let iface_name = iface_name.into();
        let iface_tag = iface_tag.into();
        Self {
            producer_name: iface_name.clone(),
            producer_tag: iface_tag.clone(),
            origin: Some(InterfaceOrigin {
                iface_name,
                iface_tag,
            }),
            link_id: link_id.into(),
        }
    }
}

/// Describes a concrete subscriber/exposer interface that a deployment requires.
///
/// Construct values through the [`DeploymentInterface`] constructors
/// (`consumed_service`, `emitted_topic`, …) rather than naming these fields directly; that keeps
/// callers decoupled from the variant layout. The variants stay public so consumers can read them
/// back via [`DeploymentInterface::interface`] in an exhaustive `match`; that read shape is the
/// deliberate, semver-relevant contract.
#[derive(Debug, Clone)]
pub enum InterfaceVariant {
    EmittedTopic {
        topic: EmittedTopic,
        origin: Option<InterfaceOrigin>,
    },
    ExposedService {
        service: ExposedService,
        origin: Option<InterfaceOrigin>,
    },
    ExposedAction {
        action: ExposedAction,
        origin: Option<InterfaceOrigin>,
    },
    ConsumedTopic {
        topic: ConsumedTopic,
        message_format: MessageFormat,
        dependency: DependencyContext,
    },
    ConsumedService {
        service: ConsumedService,
        request_format: MessageFormat,
        response_format: MessageFormat,
        dependency: DependencyContext,
    },
    ConsumedAction {
        action: ConsumedAction,
        messages: ConsumedActionMessage,
        dependency: DependencyContext,
    },
    /// A pairing topic this node's role emits. The `EmittedTopic` shape
    /// carries the pairing doc's per-topic fields (name, qos,
    /// message_format); `peer` carries the slot identity.
    PeerEmittedTopic {
        topic: EmittedTopic,
        peer: PeerContext,
    },
    /// A pairing topic the counterpart role emits (this node consumes it).
    PeerConsumedTopic {
        topic: EmittedTopic,
        peer: PeerContext,
    },
}

/// Maps a deployment interface to the message format required to bind it.
#[derive(Debug, Clone)]
pub struct DeploymentInterface {
    interface: InterfaceVariant,
}

impl DeploymentInterface {
    pub fn new(interface: InterfaceVariant) -> Self {
        Self { interface }
    }

    /// A natively emitted (or `conforms_to`-contributed) topic.
    pub fn emitted_topic(topic: EmittedTopic, origin: Option<InterfaceOrigin>) -> Self {
        Self::new(InterfaceVariant::EmittedTopic { topic, origin })
    }

    /// A natively exposed (or `conforms_to`-contributed) service.
    pub fn exposed_service(service: ExposedService, origin: Option<InterfaceOrigin>) -> Self {
        Self::new(InterfaceVariant::ExposedService { service, origin })
    }

    /// A natively exposed (or `conforms_to`-contributed) action.
    pub fn exposed_action(action: ExposedAction, origin: Option<InterfaceOrigin>) -> Self {
        Self::new(InterfaceVariant::ExposedAction { action, origin })
    }

    /// A consumed topic, with its resolved message format and dependency context.
    pub fn consumed_topic(
        topic: ConsumedTopic,
        message_format: MessageFormat,
        dependency: DependencyContext,
    ) -> Self {
        Self::new(InterfaceVariant::ConsumedTopic {
            topic,
            message_format,
            dependency,
        })
    }

    /// A consumed service, with its resolved request/response formats and dependency.
    pub fn consumed_service(
        service: ConsumedService,
        request_format: MessageFormat,
        response_format: MessageFormat,
        dependency: DependencyContext,
    ) -> Self {
        Self::new(InterfaceVariant::ConsumedService {
            service,
            request_format,
            response_format,
            dependency,
        })
    }

    /// A consumed action, with its resolved message formats and dependency.
    pub fn consumed_action(
        action: ConsumedAction,
        messages: ConsumedActionMessage,
        dependency: DependencyContext,
    ) -> Self {
        Self::new(InterfaceVariant::ConsumedAction {
            action,
            messages,
            dependency,
        })
    }

    /// A pairing topic emitted by this node's role on the given slot.
    pub fn peer_emitted_topic(topic: EmittedTopic, peer: PeerContext) -> Self {
        Self::new(InterfaceVariant::PeerEmittedTopic { topic, peer })
    }

    /// A pairing topic emitted by the counterpart role (consumed here).
    pub fn peer_consumed_topic(topic: EmittedTopic, peer: PeerContext) -> Self {
        Self::new(InterfaceVariant::PeerConsumedTopic { topic, peer })
    }

    pub fn interface(&self) -> &InterfaceVariant {
        &self.interface
    }
}

#[derive(Clone)]
pub struct InterfaceArtifact {
    /// Module path under the category dir, leaf-last. Native artifacts have a
    /// single segment (the topic/service/action name); artifacts pulled in via
    /// `interfaces.conforms_to` have three segments
    /// (`[iface_name, iface_tag, leaf_name]`) so they nest as
    /// `emitted_topics/{iface_name}/{iface_tag}/{leaf_name}.rs`.
    pub module_path: Vec<String>,
    pub kind: InterfaceKind,
    pub code_output: String,
}

impl InterfaceArtifact {
    /// Builds an artifact for `leaf_name`, nesting under
    /// `{iface_name}/{iface_tag}/{leaf_name}` when contributed via
    /// `interfaces.conforms_to`, or a single-segment `[leaf_name]` for the
    /// node's own (native) declarations.
    pub fn for_leaf(
        origin: Option<&InterfaceOrigin>,
        leaf_name: &str,
        kind: InterfaceKind,
        code_output: String,
    ) -> Self {
        let module_path = match origin {
            Some(o) => o.module_path_for(leaf_name),
            None => vec![leaf_name.to_string()],
        };
        Self {
            module_path,
            kind,
            code_output,
        }
    }

    /// Returns the leaf segment (the topic/service/action name). Panics on an
    /// empty `module_path`, which would be a generator bug.
    pub fn leaf_name(&self) -> &str {
        self.module_path
            .last()
            .map(String::as_str)
            .expect("InterfaceArtifact::module_path must not be empty")
    }
}

/// Collects deployment interfaces and produces generated artifacts when finalized.
///
/// # Lifecycle
/// Construct a backend via `new()` / `Default`, optionally call its setters
/// (`set_parameters`, and `set_container` on Python) in **any order**, register interfaces by
/// calling the `add_*` methods (usually via [`DeploymentInterface::register_with`]), then call
/// [`build`](LanguageGenerator::build), which consumes the generator and reads the
/// previously-set configuration. There is no "must call X before Y" hazard among the setters and
/// `add_*` methods; only `build` must come last.
///
/// The `add_*` methods are the internal incremental-build seam shared by both backends; external
/// callers normally drive them indirectly through `register_with` rather than calling them
/// directly.
pub trait LanguageGenerator {
    /// `origin` is `Some` when the topic was contributed via a
    /// `conforms_to` interface (nests the artifact under
    /// `{iface_name}/{iface_tag}/{leaf}`) and `None` for the node's own
    /// native declarations.
    fn add_emitted_topic(
        &mut self,
        topic: &EmittedTopic,
        origin: Option<&InterfaceOrigin>,
    ) -> Result<()>;
    fn add_exposed_service(
        &mut self,
        service: &ExposedService,
        origin: Option<&InterfaceOrigin>,
    ) -> Result<()>;
    fn add_exposed_action(
        &mut self,
        action: &ExposedAction,
        origin: Option<&InterfaceOrigin>,
    ) -> Result<()>;
    fn add_consumed_topic(
        &mut self,
        topic: &ConsumedTopic,
        arguments: MessageFormat,
        dependency: &DependencyContext,
    ) -> Result<()>;
    fn add_consumed_service(
        &mut self,
        service: &ConsumedService,
        request_arguments: &MessageFormat,
        response_arguments: &MessageFormat,
        dependency: &DependencyContext,
    ) -> Result<()>;
    fn add_consumed_action(
        &mut self,
        action: &ConsumedAction,
        messages: &ConsumedActionMessage,
        dependency: &DependencyContext,
    ) -> Result<()>;
    /// A pairing topic emitted by this node's role: slot-scoped publisher
    /// (`link_id: Some(peer.link_id)`) under the `pairing` wire target.
    fn add_peer_emitted_topic(&mut self, topic: &EmittedTopic, peer: &PeerContext) -> Result<()>;
    /// A pairing topic the counterpart role emits: a `subscribe_peer`-backed
    /// subscription that follows the slot's live pin.
    fn add_peer_consumed_topic(&mut self, topic: &EmittedTopic, peer: &PeerContext) -> Result<()>;
    /// Finalizes the builder and return a path to the library
    fn build(
        self,
        to_path: impl AsRef<Path>,
        peppy_dirs: &PeppyDirs,
        deploy_mode: CrateDeployMode,
    ) -> Result<()>;
}

impl DeploymentInterface {
    pub fn register_with<B: LanguageGenerator + ?Sized>(&self, backend: &mut B) -> Result<()> {
        match self.interface() {
            InterfaceVariant::EmittedTopic { topic, origin } => {
                backend.add_emitted_topic(topic, origin.as_ref())
            }
            InterfaceVariant::ExposedService { service, origin } => {
                backend.add_exposed_service(service, origin.as_ref())
            }
            InterfaceVariant::ExposedAction { action, origin } => {
                backend.add_exposed_action(action, origin.as_ref())
            }
            InterfaceVariant::ConsumedTopic {
                topic,
                message_format,
                dependency,
            } => backend.add_consumed_topic(topic, message_format.clone(), dependency),
            InterfaceVariant::ConsumedService {
                service,
                request_format,
                response_format,
                dependency,
            } => backend.add_consumed_service(service, request_format, response_format, dependency),
            InterfaceVariant::ConsumedAction {
                action,
                messages,
                dependency,
            } => backend.add_consumed_action(action, messages, dependency),
            InterfaceVariant::PeerEmittedTopic { topic, peer } => {
                backend.add_peer_emitted_topic(topic, peer)
            }
            InterfaceVariant::PeerConsumedTopic { topic, peer } => {
                backend.add_peer_consumed_topic(topic, peer)
            }
        }
    }
}

/// Filters out empty `MessageFormat`s, returning `None` for formats with no fields.
pub fn non_empty_message_format(format: Option<&MessageFormat>) -> Option<&MessageFormat> {
    format.filter(|format| !format.0.is_empty())
}

const RESERVED_MESSAGE_FIELD_NAMES: &[&str] = &["instance_id"];

pub(crate) fn type_token_name(token: &TypeToken) -> &'static str {
    match token {
        TypeToken::Bool => "bool",
        TypeToken::String => "string",
        TypeToken::Bytes => "bytes",
        TypeToken::Time => "time",
        TypeToken::U8 => "u8",
        TypeToken::U16 => "u16",
        TypeToken::U32 => "u32",
        TypeToken::U64 => "u64",
        TypeToken::I8 => "i8",
        TypeToken::I16 => "i16",
        TypeToken::I32 => "i32",
        TypeToken::I64 => "i64",
        TypeToken::F32 => "f32",
        TypeToken::F64 => "f64",
    }
}

pub(crate) fn is_scalar_copy_type(token: &TypeToken) -> bool {
    matches!(
        token,
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

fn validate_fixed_array_schema(schema: &SchemaType, path: &str) -> Result<()> {
    match schema {
        SchemaType::Array(array) => {
            if array.length.is_some() {
                if matches!(array.items.as_ref(), SchemaType::Object(_)) {
                    return Err(Error::UnsupportedFixedArrayItemType {
                        field: path.to_string(),
                        item: "object",
                    });
                }
                let token = array.items.as_ref().as_type_token().ok_or_else(|| {
                    Error::UnsupportedArrayItemSchema {
                        field: path.to_string(),
                    }
                })?;
                if !is_scalar_copy_type(token) {
                    return Err(Error::UnsupportedFixedArrayItemType {
                        field: path.to_string(),
                        item: type_token_name(token),
                    });
                }
            }

            validate_fixed_array_schema(array.items.as_ref(), path)
        }
        SchemaType::Object(object) => {
            for (field_name, nested) in &object.fields {
                let nested_path = format!("{path}.{field_name}");
                validate_fixed_array_schema(nested, &nested_path)?;
            }
            Ok(())
        }
        SchemaType::Type(_) | SchemaType::Primitive(_) => Ok(()),
    }
}

pub fn validate_fixed_length_array_items(format: &MessageFormat) -> Result<()> {
    for (field_name, schema) in &format.0 {
        validate_fixed_array_schema(schema, field_name)?;
    }
    Ok(())
}

fn validate_schema_field_names(schema: &SchemaType, path: &str, context: &str) -> Result<()> {
    match schema {
        SchemaType::Object(object) => validate_field_map(object.fields.iter(), path, context),
        SchemaType::Array(array) => {
            validate_schema_field_names(array.items.as_ref(), path, context)
        }
        SchemaType::Type(_) | SchemaType::Primitive(_) => Ok(()),
    }
}

fn validate_field_map<'a, I>(fields: I, parent_path: &str, context: &str) -> Result<()>
where
    I: IntoIterator<Item = (&'a String, &'a SchemaType)>,
{
    for (field_name, schema) in fields {
        let path = if parent_path.is_empty() {
            field_name.clone()
        } else {
            format!("{parent_path}.{field_name}")
        };

        if RESERVED_MESSAGE_FIELD_NAMES.contains(&field_name.as_str()) {
            return Err(Error::UnauthorizedMessageFieldName {
                field: field_name.clone(),
                path,
                context: context.to_string(),
            });
        }

        validate_schema_field_names(schema, &path, context)?;
    }

    Ok(())
}

/// Validates payload field names used inside a message format.
///
/// Some names are reserved by transport metadata and cannot be used in payload schemas.
pub fn validate_message_format_field_names(format: &MessageFormat, context: &str) -> Result<()> {
    let normalized_context = if context.trim().is_empty() {
        "message_format"
    } else {
        context
    };
    validate_field_map(format.0.iter(), "", normalized_context)
}

/// Returns the hardcoded goal-action response format used by both Rust and Python generators.
///
/// The goal acknowledgement is framework-owned (not declared by the action
/// schema): every goal response is `accepted: bool` plus an optional
/// `error_message: Optional[String]` carrying the rejection reason. The
/// generated `GoalResponse::accept()` / `GoalResponse::reject(reason)`
/// constructors produce it. Distinct from the cancel-ack format (which now
/// carries a typed `state` instead), so `fire_goal`'s accept/reject wire is
/// unchanged.
pub fn goal_action_response_format() -> MessageFormat {
    let mut fields = IndexMap::new();
    fields.insert(String::from("accepted"), SchemaType::Type(TypeToken::Bool));
    fields.insert(
        String::from("error_message"),
        SchemaType::Primitive(PrimitiveSchema {
            kind: TypeToken::String,
            optional: true,
        }),
    );
    MessageFormat(fields)
}

/// Validates that generated type names for nested objects and array-of-object items
/// do not collide within the same message format.
///
/// For example, a field `frames` (array of objects) generates `{prefix}FramesItem`,
/// while a sibling field `frames_item` (object) also generates `{prefix}FramesItem`.
/// This function detects such collisions and returns an error.
pub fn validate_generated_type_name_collisions(
    format: &MessageFormat,
    struct_prefix: &str,
) -> Result<()> {
    validate_sibling_type_name_collisions(&format.0, struct_prefix)
}

/// Returns the generated type name and nested fields for a field that
/// produces a nested struct: an object field (`{prefix}FieldName`) or an
/// array-of-objects field (the array item type name). Fields of any other
/// shape generate no type and return `None`.
fn generated_object_child<'a>(
    struct_prefix: &str,
    field_name: &str,
    schema: &'a SchemaType,
) -> Option<(String, &'a IndexMap<String, SchemaType>)> {
    match schema {
        SchemaType::Object(object) => Some((
            format!("{struct_prefix}{}", to_camel_case(field_name)),
            &object.fields,
        )),
        SchemaType::Array(array) => match array.items.as_ref() {
            SchemaType::Object(object) => Some((
                array_item_type_name(struct_prefix, field_name),
                &object.fields,
            )),
            _ => None,
        },
        _ => None,
    }
}

fn validate_sibling_type_name_collisions(
    fields: &IndexMap<String, SchemaType>,
    struct_prefix: &str,
) -> Result<()> {
    let mut seen: HashMap<String, String> = HashMap::new();

    for (field_name, schema) in fields {
        let Some((generated_name, child_fields)) =
            generated_object_child(struct_prefix, field_name, schema)
        else {
            continue;
        };

        if let Some(previous_field) = seen.get(&generated_name) {
            return Err(Error::GeneratedTypeNameCollision {
                context: struct_prefix.to_string(),
                type_name: generated_name,
                first_field: previous_field.clone(),
                second_field: field_name.clone(),
            });
        }
        seen.insert(generated_name.clone(), field_name.clone());

        // The generated name doubles as the nested struct prefix.
        validate_sibling_type_name_collisions(child_fields, &generated_name)?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dependency_context_constructors_set_origin_and_link_id() {
        // native: no origin.
        let native = DependencyContext::native("uvc_camera", "v1", "cam_left");
        assert_eq!(native.producer_name, "uvc_camera");
        assert_eq!(native.producer_tag, "v1");
        assert!(native.origin.is_none());
        assert_eq!(native.link_id, "cam_left");

        // conformed: producer node identity plus an interface origin.
        let origin = InterfaceOrigin {
            iface_name: "camera_iface".to_string(),
            iface_tag: "v2".to_string(),
        };
        let conformed =
            DependencyContext::conformed("uvc_camera", "v1", origin.clone(), "cam_left");
        assert_eq!(conformed.producer_name, "uvc_camera");
        assert_eq!(conformed.producer_tag, "v1");
        assert_eq!(conformed.origin, Some(origin.clone()));
        assert_eq!(conformed.link_id, "cam_left");

        // interface: no producer node; (name, tag) double as producer identity and origin.
        let iface = DependencyContext::interface("camera_iface", "v2", "cam_left");
        assert_eq!(iface.producer_name, "camera_iface");
        assert_eq!(iface.producer_tag, "v2");
        assert_eq!(iface.origin, Some(origin));
        assert_eq!(iface.link_id, "cam_left");
    }

    #[test]
    fn reject_reserved_message_field_name() {
        let format: MessageFormat = serde_json5::from_str(
            r#"
            {
                instance_id: "string",
                value: "u8"
            }
            "#,
        )
        .unwrap();

        let err = validate_message_format_field_names(&format, "test.topic").unwrap_err();

        let Error::UnauthorizedMessageFieldName {
            field,
            path,
            context,
        } = err
        else {
            panic!("expected UnauthorizedMessageFieldName, got: {err:?}");
        };
        assert_eq!(field, "instance_id");
        assert_eq!(path, "instance_id");
        assert_eq!(context, "test.topic");
    }

    #[test]
    fn reject_reserved_nested_message_field_name() {
        let format: MessageFormat = serde_json5::from_str(
            r#"
            {
                header: {
                    $type: "object",
                    instance_id: "string"
                }
            }
            "#,
        )
        .unwrap();

        let err = validate_message_format_field_names(&format, "test.topic").unwrap_err();

        let Error::UnauthorizedMessageFieldName { field, path, .. } = err else {
            panic!("expected UnauthorizedMessageFieldName, got: {err:?}");
        };
        assert_eq!(field, "instance_id");
        assert_eq!(path, "header.instance_id");
    }

    #[test]
    fn reject_fixed_string_array() {
        let format: MessageFormat = serde_json5::from_str(
            r#"
            {
                labels: {
                    $type: "array",
                    $items: "string",
                    $length: 3
                }
            }
            "#,
        )
        .unwrap();

        let err = validate_fixed_length_array_items(&format).unwrap_err();
        let Error::UnsupportedFixedArrayItemType { field, item } = err else {
            panic!("expected UnsupportedFixedArrayItemType, got: {err:?}");
        };
        assert_eq!(field, "labels");
        assert_eq!(item, "string");
    }

    #[test]
    fn reject_fixed_object_array() {
        let format: MessageFormat = serde_json5::from_str(
            r#"
            {
                frames: {
                    $type: "array",
                    $items: {
                        $type: "object",
                        name: "string"
                    },
                    $length: 4
                }
            }
            "#,
        )
        .unwrap();

        let err = validate_fixed_length_array_items(&format).unwrap_err();
        let Error::UnsupportedFixedArrayItemType { field, item } = err else {
            panic!("expected UnsupportedFixedArrayItemType, got: {err:?}");
        };
        assert_eq!(field, "frames");
        assert_eq!(item, "object");
    }

    #[test]
    fn allow_fixed_i32_array() {
        let format: MessageFormat = serde_json5::from_str(
            r#"
            {
                samples: {
                    $type: "array",
                    $items: "i32",
                    $length: 4
                }
            }
            "#,
        )
        .unwrap();

        validate_fixed_length_array_items(&format).expect("fixed i32 arrays are supported");
    }

    #[test]
    fn reject_array_item_type_name_collision() {
        let format: MessageFormat = serde_json5::from_str(
            r#"
            {
                frames: {
                    $type: "array",
                    $items: {
                        $type: "object",
                        x: "i32",
                        y: "i32"
                    }
                },
                frames_item: {
                    $type: "object",
                    id: "u16"
                }
            }
            "#,
        )
        .unwrap();

        let err = validate_generated_type_name_collisions(&format, "Message").unwrap_err();
        let Error::GeneratedTypeNameCollision {
            context,
            type_name,
            first_field,
            second_field,
        } = err
        else {
            panic!("expected GeneratedTypeNameCollision, got: {err:?}");
        };
        assert_eq!(context, "Message");
        assert_eq!(type_name, "MessageFramesItem");
        assert_eq!(first_field, "frames");
        assert_eq!(second_field, "frames_item");
    }

    #[test]
    fn allow_non_colliding_array_and_object_fields() {
        let format: MessageFormat = serde_json5::from_str(
            r#"
            {
                frames: {
                    $type: "array",
                    $items: {
                        $type: "object",
                        x: "i32"
                    }
                },
                metadata: {
                    $type: "object",
                    id: "u16"
                }
            }
            "#,
        )
        .unwrap();

        validate_generated_type_name_collisions(&format, "Message")
            .expect("non-colliding fields should pass");
    }

    #[test]
    fn reject_nested_array_item_type_name_collision() {
        let format: MessageFormat = serde_json5::from_str(
            r#"
            {
                outer: {
                    $type: "object",
                    frames: {
                        $type: "array",
                        $items: {
                            $type: "object",
                            x: "i32"
                        }
                    },
                    frames_item: {
                        $type: "object",
                        id: "u16"
                    }
                }
            }
            "#,
        )
        .unwrap();

        let err = validate_generated_type_name_collisions(&format, "Message").unwrap_err();
        let Error::GeneratedTypeNameCollision {
            context,
            type_name,
            first_field,
            second_field,
        } = err
        else {
            panic!("expected GeneratedTypeNameCollision, got: {err:?}");
        };
        assert_eq!(context, "MessageOuter");
        assert_eq!(type_name, "MessageOuterFramesItem");
        assert_eq!(first_field, "frames");
        assert_eq!(second_field, "frames_item");
    }
}

#[derive(Clone)]
pub struct CapnpSchema {
    file_stem: String,
    struct_module: String,
    schema: String,
}

impl CapnpSchema {
    pub fn new(file_stem: String, struct_module: String, schema: String) -> Self {
        Self {
            file_stem,
            struct_module,
            schema,
        }
    }

    pub fn file_stem(&self) -> &str {
        &self.file_stem
    }

    pub fn struct_module(&self) -> &str {
        &self.struct_module
    }

    pub fn schema(&self) -> &str {
        &self.schema
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum ModuleCategory {
    EmittedTopics,
    ConsumedTopics,
    ExposedServices,
    ConsumedServices,
    ExposedActions,
    ConsumedActions,
    /// Both directions of every pairing slot: `pairings/<link_id>/<topic>`.
    Pairings,
}

impl ModuleCategory {
    pub const ALL: [Self; 7] = [
        Self::EmittedTopics,
        Self::ConsumedTopics,
        Self::ExposedServices,
        Self::ConsumedServices,
        Self::ExposedActions,
        Self::ConsumedActions,
        Self::Pairings,
    ];

    pub fn from_kind(kind: InterfaceKind) -> Self {
        match kind {
            InterfaceKind::EmittedTopic => Self::EmittedTopics,
            InterfaceKind::ConsumedTopic => Self::ConsumedTopics,
            InterfaceKind::ExposedService => Self::ExposedServices,
            InterfaceKind::ConsumedService => Self::ConsumedServices,
            InterfaceKind::ExposedAction => Self::ExposedActions,
            InterfaceKind::ConsumedAction => Self::ConsumedActions,
            InterfaceKind::PeerEmittedTopic | InterfaceKind::PeerConsumedTopic => Self::Pairings,
        }
    }

    pub fn dir_name(self) -> &'static str {
        match self {
            Self::EmittedTopics => "emitted_topics",
            Self::ConsumedTopics => "consumed_topics",
            Self::ExposedServices => "exposed_services",
            Self::ConsumedServices => "consumed_services",
            Self::ExposedActions => "exposed_actions",
            Self::ConsumedActions => "consumed_actions",
            Self::Pairings => "pairings",
        }
    }
}
