//! Length pins a document-backed interface entry applies to the arrays of the
//! document it resolves to.
//!
//! A contract or pairing keeps an array generic when its length is a property
//! of the implementing device rather than of the interface: `joint_positions`
//! holds one entry per joint, and only the node driving a given arm knows how
//! many joints that is. Such a node pins the length through the entry's
//! `refine` block, which mirrors the member's structure down to the array and
//! carries `{ $length: N }` at the leaf:
//!
//! ```json5
//! {
//!   link_id: "limb_motion",
//!   name: "move_arm_joints",
//!   refine: {
//!     goal_service: { request_message_format: { joint_positions: { $length: 7 } } },
//!     result_service: { response_message_format: { final_joint_positions: { $length: 7 } } },
//!   },
//! }
//! ```
//!
//! Applying a refinement yields the document's shape with those lengths set,
//! and nothing else changed. A fixed and a variable array share one Cap'n
//! Proto representation, so the wire is the same on both sides: only the
//! refining node's generated types and length checks differ from an unrefined
//! one.
//!
//! Parsing checks the block's own structure (a leaf is exactly `$length`, a
//! block is never empty, modifiers are the known ones). Whether each pin lands
//! on an array the document leaves generic is known only once the document
//! resolves, so `apply` reports those problems, all of them at once.

use super::types::{
    MessageFormat, NativeEmittedTopic, NativeExposedAction, NativeExposedService, SchemaType,
};
use crate::common::type_token_name;
use indexmap::IndexMap;
use serde::{
    Deserialize, Serialize,
    de::{self, Deserializer, MapAccess, Visitor},
    ser::{SerializeMap, Serializer},
};
use std::borrow::Cow;
use std::fmt;

/// Refinements keyed by field name, mirroring the fields of one
/// `message_format`. Never empty.
#[derive(Debug, Clone, PartialEq)]
pub struct FormatRefinement(pub IndexMap<String, FieldRefinement>);

/// What a `refine` block says about one field of the document's format.
#[derive(Debug, Clone, PartialEq)]
pub enum FieldRefinement {
    /// `{ $length: N }`: the document's array at this field holds exactly
    /// `N` items.
    Length(usize),
    /// `{ $items: { ... } }`: refinements of the fields of the objects the
    /// document's array holds.
    Items(FormatRefinement),
    /// `{ field: ..., }`: refinements of the fields of the document's object.
    Object(FormatRefinement),
}

/// The `refine` block of a document-backed `topics.emits` or
/// `topics.consumes` entry.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct TopicRefinement {
    pub message_format: FormatRefinement,
}

/// The `refine` block of a document-backed `services.exposes` or
/// `services.consumes` entry, and the `goal_service` block of an
/// [`ActionRefinement`]. At least one side is named.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(try_from = "RawServiceRefinement")]
pub struct ServiceRefinement {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request_message_format: Option<FormatRefinement>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response_message_format: Option<FormatRefinement>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawServiceRefinement {
    #[serde(default)]
    request_message_format: Option<FormatRefinement>,
    #[serde(default)]
    response_message_format: Option<FormatRefinement>,
}

impl TryFrom<RawServiceRefinement> for ServiceRefinement {
    type Error = String;

    fn try_from(raw: RawServiceRefinement) -> Result<Self, Self::Error> {
        if raw.request_message_format.is_none() && raw.response_message_format.is_none() {
            return Err(
                "a service refinement names `request_message_format`, `response_message_format`, \
                 or both; an empty block pins nothing"
                    .to_string(),
            );
        }
        Ok(Self {
            request_message_format: raw.request_message_format,
            response_message_format: raw.response_message_format,
        })
    }
}

/// The `result_service` block of an [`ActionRefinement`]: the result side has
/// a response only.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ResultServiceRefinement {
    pub response_message_format: FormatRefinement,
}

/// The `refine` block of a document-backed `actions.exposes` or
/// `actions.consumes` entry. At least one endpoint is named.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(try_from = "RawActionRefinement")]
pub struct ActionRefinement {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub goal_service: Option<ServiceRefinement>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub feedback_topic: Option<TopicRefinement>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result_service: Option<ResultServiceRefinement>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawActionRefinement {
    #[serde(default)]
    goal_service: Option<ServiceRefinement>,
    #[serde(default)]
    feedback_topic: Option<TopicRefinement>,
    #[serde(default)]
    result_service: Option<ResultServiceRefinement>,
}

impl TryFrom<RawActionRefinement> for ActionRefinement {
    type Error = String;

    fn try_from(raw: RawActionRefinement) -> Result<Self, Self::Error> {
        if raw.goal_service.is_none()
            && raw.feedback_topic.is_none()
            && raw.result_service.is_none()
        {
            return Err(
                "an action refinement names `goal_service`, `feedback_topic`, `result_service`, \
                 or several of them; an empty block pins nothing"
                    .to_string(),
            );
        }
        Ok(Self {
            goal_service: raw.goal_service,
            feedback_topic: raw.feedback_topic,
            result_service: raw.result_service,
        })
    }
}

// --- Parsing --------------------------------------------------------------

impl<'de> Deserialize<'de> for FormatRefinement {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_map(FormatRefinementVisitor)
    }
}

struct FormatRefinementVisitor;

impl<'de> Visitor<'de> for FormatRefinementVisitor {
    type Value = FormatRefinement;

    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter.write_str("a non-empty map of field name to refinement")
    }

    fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut fields: IndexMap<String, FieldRefinement> = IndexMap::new();
        while let Some(key) = map.next_key::<String>()? {
            // Checked before the value is read, so a stray `$length: 7` at
            // the root is named as such rather than failing on the `7`.
            if key.starts_with('$') {
                return Err(de::Error::custom(format!(
                    "`{key}` at the root of a format refinement: a format is a map of fields, so \
                     name the array field to pin and put `{key}` under it"
                )));
            }
            let refinement: FieldRefinement = map.next_value()?;
            if fields.insert(key.clone(), refinement).is_some() {
                return Err(de::Error::custom(format!(
                    "duplicate field `{key}` in a refinement"
                )));
            }
        }
        if fields.is_empty() {
            return Err(de::Error::custom(
                "an empty format refinement pins nothing; name the field to pin or remove the block",
            ));
        }
        Ok(FormatRefinement(fields))
    }
}

impl Serialize for FormatRefinement {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        self.0.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for FieldRefinement {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_map(FieldRefinementVisitor)
    }
}

struct FieldRefinementVisitor;

impl<'de> Visitor<'de> for FieldRefinementVisitor {
    type Value = FieldRefinement;

    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter.write_str(
            "a `{ $length: N }` leaf, a `{ $items: { ... } }` block, or a map of nested field \
             refinements",
        )
    }

    fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut length: Option<usize> = None;
        let mut items: Option<FormatRefinement> = None;
        let mut fields: IndexMap<String, FieldRefinement> = IndexMap::new();
        while let Some(key) = map.next_key::<String>()? {
            match key.as_str() {
                "$length" => {
                    if length.is_some() {
                        return Err(de::Error::duplicate_field("$length"));
                    }
                    length = Some(map.next_value::<StrictLength>()?.0);
                }
                "$items" => {
                    if items.is_some() {
                        return Err(de::Error::duplicate_field("$items"));
                    }
                    items = Some(map.next_value()?);
                }
                modifier if modifier.starts_with('$') => {
                    return Err(de::Error::custom(format!(
                        "unknown modifier `{modifier}` in a refinement: a refinement carries \
                         `$length` at an array, `$items` to step into an array of objects, or \
                         field names to step into an object"
                    )));
                }
                _ => {
                    let nested: FieldRefinement = map.next_value()?;
                    if fields.insert(key.clone(), nested).is_some() {
                        return Err(de::Error::custom(format!(
                            "duplicate field `{key}` in a refinement"
                        )));
                    }
                }
            }
        }
        match (length, items, fields.is_empty()) {
            (Some(length), None, true) => Ok(FieldRefinement::Length(length)),
            (None, Some(items), true) => Ok(FieldRefinement::Items(items)),
            (None, None, false) => Ok(FieldRefinement::Object(FormatRefinement(fields))),
            (None, None, true) => Err(de::Error::custom(
                "an empty refinement pins nothing; write `{ $length: N }` at the array to pin or \
                 name a nested field",
            )),
            _ => Err(de::Error::custom(
                "`$length`, `$items`, and nested field names are mutually exclusive within one \
                 refinement: an array is pinned, stepped into, or an object is stepped into",
            )),
        }
    }
}

/// A `$length` value read through `deserialize_any`, so the JSON5 number is
/// seen as written: an integer is accepted, a negative or fractional number
/// is rejected rather than cast.
struct StrictLength(usize);

impl<'de> Deserialize<'de> for StrictLength {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct LengthVisitor;

        impl Visitor<'_> for LengthVisitor {
            type Value = StrictLength;

            fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
                formatter.write_str("a non-negative integer `$length`")
            }

            fn visit_u64<E: de::Error>(self, value: u64) -> Result<Self::Value, E> {
                usize::try_from(value)
                    .map(StrictLength)
                    .map_err(|_| E::custom(format!("`$length` {value} does not fit a usize")))
            }

            fn visit_i64<E: de::Error>(self, value: i64) -> Result<Self::Value, E> {
                let non_negative = u64::try_from(value).map_err(|_| {
                    E::custom(format!("`$length` must not be negative, got {value}"))
                })?;
                self.visit_u64(non_negative)
            }

            fn visit_f64<E: de::Error>(self, value: f64) -> Result<Self::Value, E> {
                Err(E::custom(format!(
                    "`$length` must be an integer, got {value}"
                )))
            }
        }

        deserializer.deserialize_any(LengthVisitor)
    }
}

impl Serialize for FieldRefinement {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match self {
            Self::Length(length) => {
                let mut map = serializer.serialize_map(Some(1))?;
                map.serialize_entry("$length", length)?;
                map.end()
            }
            Self::Items(items) => {
                let mut map = serializer.serialize_map(Some(1))?;
                map.serialize_entry("$items", items)?;
                map.end()
            }
            Self::Object(fields) => fields.serialize(serializer),
        }
    }
}

// --- Applying -------------------------------------------------------------

/// One pin the resolved document does not admit, with the dotted path of
/// the pin inside the member (`goal_service.request_message_format.joint_positions`,
/// `message_format.frames[].position`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefinementProblem {
    pub path: String,
    pub kind: RefinementProblemKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RefinementProblemKind {
    /// The member declares no format at this path (a service without a
    /// request, an action without feedback).
    FormatAbsent,
    /// No field of that name in the document's format.
    UnknownField,
    /// `$length` or `$items` where the document declares something other
    /// than an array.
    NotAnArray { declared: String },
    /// Nested field refinements where the document declares something other
    /// than an object.
    NotAnObject { declared: String },
    /// `$length` on an array the document already pins.
    AlreadyFixed { length: usize },
    /// `$length` on an array whose items cannot be fixed: objects and the
    /// pointer-backed tokens (`string`, `bytes`, `time`).
    UnsupportedItems { declared: String },
    /// `$items` on an array whose items are not objects.
    ItemsNotObjects { declared: String },
}

impl fmt::Display for RefinementProblem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "`{}`: ", self.path)?;
        match &self.kind {
            RefinementProblemKind::FormatAbsent => {
                f.write_str("the document declares no such format for this member")
            }
            RefinementProblemKind::UnknownField => {
                f.write_str("the document declares no such field")
            }
            RefinementProblemKind::NotAnArray { declared } => write!(
                f,
                "`$length` and `$items` apply to arrays, but the document declares {declared}"
            ),
            RefinementProblemKind::NotAnObject { declared } => write!(
                f,
                "nested field refinements apply to objects, but the document declares {declared}"
            ),
            RefinementProblemKind::AlreadyFixed { length } => {
                write!(f, "the document already fixes the length at {length}")
            }
            RefinementProblemKind::UnsupportedItems { declared } => write!(
                f,
                "a fixed length needs scalar items, but the document declares {declared} items"
            ),
            RefinementProblemKind::ItemsNotObjects { declared } => write!(
                f,
                "`$items` steps into an array of objects, but the document declares {declared} items"
            ),
        }
    }
}

/// How the document describes a schema node, for problem messages.
fn describe(schema: &SchemaType) -> String {
    let article = match schema {
        SchemaType::Array(_) | SchemaType::Object(_) => "an",
        SchemaType::Type(_) | SchemaType::Primitive(_) => "a",
    };
    format!("{article} {}", describe_items(schema))
}

/// How the document describes an array's items, for problem messages. The
/// same rendering [`describe`] prefixes with an article.
fn describe_items(items: &SchemaType) -> String {
    match items {
        SchemaType::Type(token) => format!("`{}`", type_token_name(token)),
        SchemaType::Primitive(primitive) => format!("`{}`", type_token_name(&primitive.kind)),
        SchemaType::Array(_) => "array".to_string(),
        SchemaType::Object(_) => "object".to_string(),
    }
}

impl FormatRefinement {
    /// Pins every array this refinement names inside `fields`, reporting
    /// each pin the fields do not admit under `prefix`. Pins that do land
    /// are applied even when others fail, so the caller decides whether a
    /// partial application is usable (the resolvers treat any problem as
    /// fatal and discard the result).
    fn apply_to_fields(
        &self,
        fields: &mut IndexMap<String, SchemaType>,
        prefix: &str,
        problems: &mut Vec<RefinementProblem>,
    ) {
        for (field_name, refinement) in &self.0 {
            let path = format!("{prefix}.{field_name}");
            let Some(schema) = fields.get_mut(field_name) else {
                problems.push(RefinementProblem {
                    path,
                    kind: RefinementProblemKind::UnknownField,
                });
                continue;
            };
            refinement.apply_to_schema(schema, path, problems);
        }
    }
}

impl FieldRefinement {
    fn apply_to_schema(
        &self,
        schema: &mut SchemaType,
        path: String,
        problems: &mut Vec<RefinementProblem>,
    ) {
        match (self, schema) {
            (Self::Length(length), SchemaType::Array(array)) => {
                if let Some(existing) = array.length {
                    problems.push(RefinementProblem {
                        path,
                        kind: RefinementProblemKind::AlreadyFixed { length: existing },
                    });
                    return;
                }
                let fixable = array
                    .items
                    .as_type_token()
                    .is_some_and(|token| token.is_scalar());
                if !fixable {
                    problems.push(RefinementProblem {
                        path,
                        kind: RefinementProblemKind::UnsupportedItems {
                            declared: describe_items(&array.items),
                        },
                    });
                    return;
                }
                array.length = Some(*length);
            }
            (Self::Items(nested), SchemaType::Array(array)) => match array.items.as_mut() {
                SchemaType::Object(object) => {
                    nested.apply_to_fields(&mut object.fields, &format!("{path}[]"), problems);
                }
                other => problems.push(RefinementProblem {
                    path,
                    kind: RefinementProblemKind::ItemsNotObjects {
                        declared: describe_items(other),
                    },
                }),
            },
            (Self::Length(_) | Self::Items(_), other) => problems.push(RefinementProblem {
                path,
                kind: RefinementProblemKind::NotAnArray {
                    declared: describe(other),
                },
            }),
            (Self::Object(nested), SchemaType::Object(object)) => {
                nested.apply_to_fields(&mut object.fields, &path, problems);
            }
            (Self::Object(_), other) => problems.push(RefinementProblem {
                path,
                kind: RefinementProblemKind::NotAnObject {
                    declared: describe(other),
                },
            }),
        }
    }
}

/// Applies an optional refinement to an optional format slot named `path`.
/// A refinement with no format to land on is a problem; a format with no
/// refinement is left alone.
fn apply_to_slot(
    refinement: Option<&FormatRefinement>,
    format: Option<&mut MessageFormat>,
    path: &str,
    problems: &mut Vec<RefinementProblem>,
) {
    let Some(refinement) = refinement else {
        return;
    };
    let Some(format) = format else {
        problems.push(RefinementProblem {
            path: path.to_string(),
            kind: RefinementProblemKind::FormatAbsent,
        });
        return;
    };
    refinement.apply_to_fields(&mut format.0, path, problems);
}

fn finish<T>(value: T, problems: Vec<RefinementProblem>) -> Result<T, Vec<RefinementProblem>> {
    if problems.is_empty() {
        Ok(value)
    } else {
        Err(problems)
    }
}

/// Applying a `refine` block to the document member it belongs to. One impl
/// per (block, member) pair, so a call site cannot pair a service block with
/// a topic member.
pub trait Refines<M> {
    /// The member as the document declares it, with this refinement's pins
    /// applied, or every pin the document does not admit.
    fn apply(&self, member: M) -> Result<M, Vec<RefinementProblem>>;
}

/// The member as the entry wants it: the entry's `refine` block applied when
/// it carries one, the member untouched otherwise.
pub fn refined<M, R: Refines<M>>(
    refinement: Option<&R>,
    member: M,
) -> Result<M, Vec<RefinementProblem>> {
    match refinement {
        Some(refinement) => refinement.apply(member),
        None => Ok(member),
    }
}

/// [`refined`] for a member the caller only borrows out of a resolved
/// document. The clone happens in the refining arm alone, so an entry
/// without a `refine` block — nearly every entry — pays nothing to leave
/// the document's member as it found it.
pub fn refined_ref<'a, M, R>(
    refinement: Option<&R>,
    member: &'a M,
) -> Result<Cow<'a, M>, Vec<RefinementProblem>>
where
    M: Clone,
    R: Refines<M>,
{
    match refinement {
        Some(refinement) => refinement.apply(member.clone()).map(Cow::Owned),
        None => Ok(Cow::Borrowed(member)),
    }
}

impl Refines<NativeEmittedTopic> for TopicRefinement {
    fn apply(
        &self,
        mut topic: NativeEmittedTopic,
    ) -> Result<NativeEmittedTopic, Vec<RefinementProblem>> {
        let mut problems = Vec::new();
        apply_to_slot(
            Some(&self.message_format),
            topic.message_format.as_mut(),
            "message_format",
            &mut problems,
        );
        finish(topic, problems)
    }
}

impl Refines<NativeExposedService> for ServiceRefinement {
    fn apply(
        &self,
        mut service: NativeExposedService,
    ) -> Result<NativeExposedService, Vec<RefinementProblem>> {
        let mut problems = Vec::new();
        apply_to_slot(
            self.request_message_format.as_ref(),
            service.request_message_format.as_mut(),
            "request_message_format",
            &mut problems,
        );
        apply_to_slot(
            self.response_message_format.as_ref(),
            service.response_message_format.as_mut(),
            "response_message_format",
            &mut problems,
        );
        finish(service, problems)
    }
}

impl Refines<NativeExposedAction> for ActionRefinement {
    /// The action's three endpoints are five format slots. Their paths are
    /// the action's, so they are named here rather than by the topic and
    /// service blocks, which stand alone on their own sections too.
    fn apply(
        &self,
        mut action: NativeExposedAction,
    ) -> Result<NativeExposedAction, Vec<RefinementProblem>> {
        let mut problems = Vec::new();
        if let Some(goal) = &self.goal_service {
            let (request, response) = action.goal_service.as_mut().map_or((None, None), |slot| {
                (
                    slot.request_message_format.as_mut(),
                    slot.response_message_format.as_mut(),
                )
            });
            apply_to_slot(
                goal.request_message_format.as_ref(),
                request,
                "goal_service.request_message_format",
                &mut problems,
            );
            apply_to_slot(
                goal.response_message_format.as_ref(),
                response,
                "goal_service.response_message_format",
                &mut problems,
            );
        }
        if let Some(feedback) = &self.feedback_topic {
            apply_to_slot(
                Some(&feedback.message_format),
                action
                    .feedback_topic
                    .as_mut()
                    .map(|slot| &mut slot.message_format),
                "feedback_topic.message_format",
                &mut problems,
            );
        }
        if let Some(result) = &self.result_service {
            apply_to_slot(
                Some(&result.response_message_format),
                action
                    .result_service
                    .as_mut()
                    .and_then(|slot| slot.response_message_format.as_mut()),
                "result_service.response_message_format",
                &mut problems,
            );
        }
        finish(action, problems)
    }
}

#[cfg(test)]
mod tests {
    use super::super::types::ArraySchema;
    use super::*;

    fn length_of(schema: &SchemaType) -> Option<usize> {
        let SchemaType::Array(ArraySchema { length, .. }) = schema else {
            panic!("expected an array, got {schema:?}");
        };
        *length
    }

    fn kinds(problems: &[RefinementProblem]) -> Vec<(&str, &RefinementProblemKind)> {
        problems
            .iter()
            .map(|problem| (problem.path.as_str(), &problem.kind))
            .collect()
    }

    // --- parsing ---

    #[test]
    fn parses_a_length_leaf() {
        let parsed: FormatRefinement =
            serde_json5::from_str(r#"{ joint_positions: { $length: 7 } }"#).unwrap();
        assert_eq!(parsed.0["joint_positions"], FieldRefinement::Length(7));
    }

    #[test]
    fn parses_nested_object_and_items_paths() {
        let parsed: FormatRefinement = serde_json5::from_str(
            r#"{
                header: { frame: { $length: 3 } },
                frames: { $items: { position: { $length: 3 } } },
            }"#,
        )
        .unwrap();
        let FieldRefinement::Object(header) = &parsed.0["header"] else {
            panic!("header should be a nested object refinement");
        };
        assert_eq!(header.0["frame"], FieldRefinement::Length(3));
        let FieldRefinement::Items(items) = &parsed.0["frames"] else {
            panic!("frames should be an items refinement");
        };
        assert_eq!(items.0["position"], FieldRefinement::Length(3));
    }

    #[test]
    fn rejects_empty_blocks_at_every_level() {
        for (json5, needle) in [
            (r#"{}"#, "empty format refinement"),
            (r#"{ joints: {} }"#, "empty refinement pins nothing"),
            (r#"{ frames: { $items: {} } }"#, "empty format refinement"),
        ] {
            let err = serde_json5::from_str::<FormatRefinement>(json5)
                .expect_err("empty blocks must be rejected");
            assert!(
                err.to_string().contains(needle),
                "{json5}: expected `{needle}` in: {err}"
            );
        }
    }

    #[test]
    fn rejects_unknown_modifiers_and_mixed_leaves() {
        for (json5, needle) in [
            (
                r#"{ joints: { $type: "array", $length: 7 } }"#,
                "unknown modifier `$type`",
            ),
            (
                r#"{ joints: { $optional: true } }"#,
                "unknown modifier `$optional`",
            ),
            (
                r#"{ joints: { $length: 7, x: { $length: 1 } } }"#,
                "mutually exclusive",
            ),
            (
                r#"{ joints: { $length: 7, $items: { x: { $length: 1 } } } }"#,
                "mutually exclusive",
            ),
            (r#"{ $length: 7 }"#, "at the root of a format refinement"),
        ] {
            let err = serde_json5::from_str::<FormatRefinement>(json5)
                .expect_err("malformed refinements must be rejected");
            assert!(
                err.to_string().contains(needle),
                "{json5}: expected `{needle}` in: {err}"
            );
        }
    }

    #[test]
    fn rejects_a_non_integer_length() {
        for json5 in [
            r#"{ joints: { $length: "seven" } }"#,
            r#"{ joints: { $length: -1 } }"#,
            r#"{ joints: { $length: 1.5 } }"#,
        ] {
            serde_json5::from_str::<FormatRefinement>(json5)
                .expect_err("a length that is not a non-negative integer must be rejected");
        }
    }

    #[test]
    fn service_and_action_blocks_require_at_least_one_endpoint() {
        let err = serde_json5::from_str::<ServiceRefinement>("{}")
            .expect_err("an empty service refinement must be rejected");
        assert!(err.to_string().contains("request_message_format"), "{err}");
        let err = serde_json5::from_str::<ActionRefinement>("{}")
            .expect_err("an empty action refinement must be rejected");
        assert!(err.to_string().contains("goal_service"), "{err}");
        let err = serde_json5::from_str::<ActionRefinement>(r#"{ cancel_service: {} }"#)
            .expect_err("an unknown endpoint must be rejected");
        assert!(err.to_string().contains("cancel_service"), "{err}");
    }

    #[test]
    fn round_trips_through_serde() {
        let original: ActionRefinement = serde_json5::from_str(
            r#"{
                goal_service: { request_message_format: { joint_positions: { $length: 7 } } },
                feedback_topic: { message_format: { frames: { $items: { position: { $length: 3 } } } } },
                result_service: { response_message_format: { pose: { position: { $length: 3 } } } },
            }"#,
        )
        .unwrap();
        let serialized = serde_json5::to_string(&original).unwrap();
        let reparsed: ActionRefinement = serde_json5::from_str(&serialized).unwrap();
        assert_eq!(original, reparsed);
    }

    // --- applying ---

    fn topic(message_format: &str) -> NativeEmittedTopic {
        serde_json5::from_str(&format!(
            r#"{{ name: "joint_states", message_format: {message_format} }}"#
        ))
        .unwrap()
    }

    fn topic_refinement(message_format: &str) -> TopicRefinement {
        serde_json5::from_str(&format!(r#"{{ message_format: {message_format} }}"#)).unwrap()
    }

    #[test]
    fn pins_a_generic_array_and_leaves_the_rest_untouched() {
        let original = topic(
            r#"{
                timestamp: "time",
                positions: { $type: "array", $items: "f64" },
                velocities: { $type: "array", $items: "f64" },
            }"#,
        );
        let refined = topic_refinement(r#"{ positions: { $length: 7 } }"#)
            .apply(original.clone())
            .expect("a generic f64 array accepts a length");
        let format = refined.message_format.as_ref().unwrap();
        assert_eq!(length_of(&format.0["positions"]), Some(7));
        assert_eq!(length_of(&format.0["velocities"]), None);
        assert_eq!(
            format.0["timestamp"],
            original.message_format.as_ref().unwrap().0["timestamp"]
        );
        assert_eq!(
            format.0.keys().collect::<Vec<_>>(),
            ["timestamp", "positions", "velocities"]
        );
    }

    #[test]
    fn pins_through_objects_and_array_items() {
        let original = topic(
            r#"{
                header: { $type: "object", frame: { $type: "array", $items: "u8" } },
                frames: {
                    $type: "array",
                    $items: { $type: "object", name: "string", position: { $type: "array", $items: "f32" } },
                },
            }"#,
        );
        let refined = topic_refinement(
            r#"{
                header: { frame: { $length: 4 } },
                frames: { $items: { position: { $length: 3 } } },
            }"#,
        )
        .apply(original)
        .expect("nested paths resolve");
        let format = refined.message_format.unwrap();
        let SchemaType::Object(header) = &format.0["header"] else {
            panic!("header stays an object");
        };
        assert_eq!(length_of(&header.fields["frame"]), Some(4));
        let SchemaType::Array(frames) = &format.0["frames"] else {
            panic!("frames stays an array");
        };
        let SchemaType::Object(item) = frames.items.as_ref() else {
            panic!("frame items stay objects");
        };
        assert_eq!(length_of(&item.fields["position"]), Some(3));
    }

    #[test]
    fn reports_every_inadmissible_pin_at_once() {
        let original = topic(
            r#"{
                positions: { $type: "array", $items: "f64", $length: 7 },
                label: "string",
                names: { $type: "array", $items: "string" },
                pose: { $type: "object", x: "f64" },
                samples: { $type: "array", $items: "f64" },
            }"#,
        );
        let problems = topic_refinement(
            r#"{
                positions: { $length: 7 },
                label: { $length: 2 },
                names: { $length: 2 },
                pose: { $length: 3 },
                samples: { $items: { x: { $length: 1 } } },
                missing: { $length: 1 },
                pose_again: { $length: 1 },
            }"#,
        )
        .apply(original)
        .expect_err("every pin above is inadmissible");
        assert_eq!(
            kinds(&problems),
            vec![
                (
                    "message_format.positions",
                    &RefinementProblemKind::AlreadyFixed { length: 7 }
                ),
                (
                    "message_format.label",
                    &RefinementProblemKind::NotAnArray {
                        declared: "a `string`".to_string()
                    }
                ),
                (
                    "message_format.names",
                    &RefinementProblemKind::UnsupportedItems {
                        declared: "`string`".to_string()
                    }
                ),
                (
                    "message_format.pose",
                    &RefinementProblemKind::NotAnArray {
                        declared: "an object".to_string()
                    }
                ),
                (
                    "message_format.samples",
                    &RefinementProblemKind::ItemsNotObjects {
                        declared: "`f64`".to_string()
                    }
                ),
                (
                    "message_format.missing",
                    &RefinementProblemKind::UnknownField
                ),
                (
                    "message_format.pose_again",
                    &RefinementProblemKind::UnknownField
                ),
            ]
        );
        let rendered = problems[0].to_string();
        assert_eq!(
            rendered,
            "`message_format.positions`: the document already fixes the length at 7"
        );
    }

    #[test]
    fn nested_refinement_on_a_non_object_is_reported() {
        let problems = topic_refinement(r#"{ positions: { x: { $length: 1 } } }"#)
            .apply(topic(r#"{ positions: { $type: "array", $items: "f64" } }"#))
            .expect_err("stepping into an array as an object is inadmissible");
        assert_eq!(
            kinds(&problems),
            vec![(
                "message_format.positions",
                &RefinementProblemKind::NotAnObject {
                    declared: "an array".to_string()
                }
            )]
        );
    }

    #[test]
    fn object_arrays_cannot_be_pinned() {
        let problems = topic_refinement(r#"{ frames: { $length: 2 } }"#)
            .apply(topic(
                r#"{ frames: { $type: "array", $items: { $type: "object", x: "f64" } } }"#,
            ))
            .expect_err("fixed arrays hold scalars only");
        assert_eq!(
            kinds(&problems),
            vec![(
                "message_format.frames",
                &RefinementProblemKind::UnsupportedItems {
                    declared: "object".to_string()
                }
            )]
        );
    }

    #[test]
    fn a_topic_without_a_format_has_nothing_to_pin() {
        let bare: NativeEmittedTopic = serde_json5::from_str(r#"{ name: "tick" }"#).unwrap();
        let problems = topic_refinement(r#"{ x: { $length: 1 } }"#)
            .apply(bare)
            .expect_err("no format, no pin");
        assert_eq!(
            kinds(&problems),
            vec![("message_format", &RefinementProblemKind::FormatAbsent)]
        );
    }

    #[test]
    fn service_refinement_pins_each_side_independently() {
        let service: NativeExposedService = serde_json5::from_str(
            r#"{
                name: "set_joints",
                request_message_format: { targets: { $type: "array", $items: "f64" } },
                response_message_format: { measured: { $type: "array", $items: "f64" } },
            }"#,
        )
        .unwrap();
        let refinement: ServiceRefinement =
            serde_json5::from_str(r#"{ response_message_format: { measured: { $length: 6 } } }"#)
                .unwrap();
        let refined = refinement.apply(service).unwrap();
        assert_eq!(
            length_of(&refined.request_message_format.as_ref().unwrap().0["targets"]),
            None
        );
        assert_eq!(
            length_of(&refined.response_message_format.as_ref().unwrap().0["measured"]),
            Some(6)
        );
    }

    #[test]
    fn action_refinement_covers_every_endpoint_and_reports_absent_ones() {
        let action: NativeExposedAction = serde_json5::from_str(
            r#"{
                name: "move_arm_joints",
                goal_service: {
                    request_message_format: { joint_positions: { $type: "array", $items: "f64" } },
                },
                result_service: {
                    response_message_format: { final_joint_positions: { $type: "array", $items: "f64" } },
                },
            }"#,
        )
        .unwrap();
        let refinement: ActionRefinement = serde_json5::from_str(
            r#"{
                goal_service: { request_message_format: { joint_positions: { $length: 7 } } },
                result_service: { response_message_format: { final_joint_positions: { $length: 7 } } },
            }"#,
        )
        .unwrap();
        let refined = refinement.apply(action.clone()).unwrap();
        let goal = refined.goal_service.as_ref().unwrap();
        assert_eq!(
            length_of(&goal.request_message_format.as_ref().unwrap().0["joint_positions"]),
            Some(7)
        );
        let result = refined.result_service.as_ref().unwrap();
        assert_eq!(
            length_of(&result.response_message_format.as_ref().unwrap().0["final_joint_positions"]),
            Some(7)
        );

        let absent: ActionRefinement = serde_json5::from_str(
            r#"{
                goal_service: { response_message_format: { accepted_at: { $length: 1 } } },
                feedback_topic: { message_format: { progress: { $length: 1 } } },
            }"#,
        )
        .unwrap();
        let problems = absent.apply(action).expect_err("neither endpoint exists");
        assert_eq!(
            kinds(&problems),
            vec![
                (
                    "goal_service.response_message_format",
                    &RefinementProblemKind::FormatAbsent
                ),
                (
                    "feedback_topic.message_format",
                    &RefinementProblemKind::FormatAbsent
                ),
            ]
        );
    }
}
