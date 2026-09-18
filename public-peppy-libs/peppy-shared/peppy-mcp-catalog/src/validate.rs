//! Validation of an exposure against its contracts, and the catalog it
//! derives.
//!
//! [`build_exposure_bundle`] is a pure function: the exposure document and
//! the resolved contracts go in, the validated exposure (the bundle beside
//! the contract member behind each of its entries) or the full list of
//! violations comes out. It runs identically in a hub's index check, when a
//! server starts, and in tests; resolving contract bytes out of the
//! repository machinery is the caller's job.

use crate::bundle::{
    BundleContractPin, BundleIdentity, BundleServer, EXPOSURE_BUNDLE_FORMAT, ExposureBundle,
    ResourceEntry, ResourcePolicies, SCHEMA_MAPPING_VERSION, TaskEntry, ToolEntry,
};
use crate::document::{McpExposure, ServiceExposure, TopicExposure};
use crate::policy::ImageFieldMap;
use crate::schema::{
    MaxSerializedSize, empty_object_schema, integer_bounds, max_serialized_json_bytes,
    message_format_to_json_schema,
};
use peppy_config_model::fingerprint::ManifestFingerprint;
use peppy_config_model::node::{
    MessageFormat, NativeEmittedTopic, NativeExposedAction, NativeExposedService, SchemaType,
    TypeToken,
};
use peppy_config_model::type_token_name;
use serde_json::Value;
use std::collections::BTreeMap;
use std::fmt;

/// One contract as resolved for validation: its identity, the fingerprint of
/// the exact bytes it was parsed from (so pin equality is checked against
/// content rather than trust), and the members it declares.
#[derive(Debug, Clone, Copy)]
pub struct ResolvedContract<'a> {
    pub name: &'a str,
    pub tag: &'a str,
    pub sha256: &'a ManifestFingerprint,
    pub topics: &'a [NativeEmittedTopic],
    pub services: &'a [NativeExposedService],
    pub actions: &'a [NativeExposedAction],
}

/// The verdict when an exposure does not validate: every violation found,
/// not just the first, so an author fixes the document in one pass.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExposureValidationError {
    pub violations: Vec<String>,
}

impl fmt::Display for ExposureValidationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "the exposure does not validate against its contracts:")?;
        for violation in &self.violations {
            write!(f, "\n  - {violation}")?;
        }
        Ok(())
    }
}

impl std::error::Error for ExposureValidationError {}

/// The contract member behind one catalog entry, as validation resolved
/// it: the slot the entry's target became, and the member it selects as
/// the contract declares it.
#[derive(Debug, Clone, PartialEq)]
pub struct BoundMember<M> {
    /// The contract slot behind the entry's target; its `link_id` is the
    /// entry's `target`.
    pub slot: BundleContractPin,
    pub member: M,
}

impl<M: Clone> BoundMember<M> {
    fn new(slot: &BundleContractPin, member: &M) -> Self {
        Self {
            slot: slot.clone(),
            member: member.clone(),
        }
    }
}

/// A validated exposure: its bundle, and behind each entry the contract
/// member validation checked it against, so a server binds an entry to
/// that member rather than looking it up again by name.
#[derive(Debug, Clone, PartialEq)]
pub struct ValidatedExposure {
    pub bundle: ExposureBundle,
    resources: Vec<BoundMember<NativeEmittedTopic>>,
    tools: Vec<BoundMember<NativeExposedService>>,
    tasks: Vec<BoundMember<NativeExposedAction>>,
}

impl ValidatedExposure {
    /// Each resource entry with the topic behind it.
    pub fn resources(
        &self,
    ) -> impl Iterator<Item = (&ResourceEntry, &BoundMember<NativeEmittedTopic>)> {
        self.bundle.resources.iter().zip(&self.resources)
    }

    /// Each tool entry with the service behind it.
    pub fn tools(&self) -> impl Iterator<Item = (&ToolEntry, &BoundMember<NativeExposedService>)> {
        self.bundle.tools.iter().zip(&self.tools)
    }

    /// Each task entry with the action behind it.
    pub fn tasks(&self) -> impl Iterator<Item = (&TaskEntry, &BoundMember<NativeExposedAction>)> {
        self.bundle.tasks.iter().zip(&self.tasks)
    }
}

/// Validate `exposure` against `contracts` and derive its bundle.
///
/// Validation succeeds only when every selected member exists in its
/// referenced contract with the expected kind, every message definition
/// translates under the canonical schema mapping, representation policies
/// name real members with the right types, `restrict` bounds fit their
/// members, size limits hold wherever a payload has a finite maximum, and
/// every author pin matches the resolved contract's bytes. The bundle's
/// contract pins carry the resolved fingerprints, which is what a pin-less
/// reference is validated against.
pub fn build_exposure_bundle(
    exposure: &McpExposure,
    contracts: &[ResolvedContract<'_>],
) -> Result<ValidatedExposure, ExposureValidationError> {
    let mut violations = Vec::new();

    let mut by_identity: BTreeMap<(&str, &str), &ResolvedContract<'_>> = BTreeMap::new();
    for resolved in contracts {
        if by_identity
            .insert((resolved.name, resolved.tag), resolved)
            .is_some()
        {
            violations.push(format!(
                "contract `{}:{}` was provided more than once",
                resolved.name, resolved.tag
            ));
        }
    }

    let mut pins = Vec::new();
    let mut resources = Vec::new();
    let mut resource_members = Vec::new();
    let mut tools = Vec::new();
    let mut tool_members = Vec::new();
    let mut tasks = Vec::new();
    let mut task_members = Vec::new();

    for (target_name, target) in &exposure.targets {
        let reference = &target.contract;
        let contract_label = format!("{}:{}", reference.name, reference.tag);

        let Some(contract) = by_identity.get(&(reference.name.as_str(), reference.tag.as_str()))
        else {
            violations.push(format!(
                "target `{target_name}` references contract `{contract_label}`, which was not \
                 provided"
            ));
            continue;
        };
        if let Some(pinned) = &reference.sha256
            && contract.sha256 != pinned
        {
            violations.push(format!(
                "target `{target_name}` pins contract `{contract_label}` at sha256 `{pinned}`, \
                 but the resolved document's bytes fingerprint to `{}`",
                contract.sha256
            ));
            continue;
        }
        let slot = BundleContractPin {
            name: reference.name.as_str().to_string(),
            tag: reference.tag.clone(),
            sha256: contract.sha256.to_string(),
            link_id: target_name.clone(),
        };

        for topic in &target.topics {
            let Some(declared) = find_topic(contract, &topic.member) else {
                violations.push(missing_member_violation(
                    target_name,
                    MemberKind::Topic,
                    &topic.member,
                    &contract_label,
                    contract,
                ));
                continue;
            };
            if let Some(entry) = check_topic(target_name, topic, declared, &mut violations) {
                resources.push(entry);
                resource_members.push(BoundMember::new(&slot, declared));
            }
        }

        for service in &target.services {
            let Some(declared) = find_service(contract, &service.member) else {
                violations.push(missing_member_violation(
                    target_name,
                    MemberKind::Service,
                    &service.member,
                    &contract_label,
                    contract,
                ));
                continue;
            };
            if let Some(entry) = check_service(target_name, service, declared, &mut violations) {
                tools.push(entry);
                tool_members.push(BoundMember::new(&slot, declared));
            }
        }

        for action in &target.actions {
            let Some(declared) = find_action(contract, &action.member) else {
                violations.push(missing_member_violation(
                    target_name,
                    MemberKind::Action,
                    &action.member,
                    &contract_label,
                    contract,
                ));
                continue;
            };
            let context = format!("target `{target_name}` action `{}`", action.member);
            let goal_request = derive_schema(
                declared
                    .goal_service
                    .as_ref()
                    .and_then(|g| g.request_message_format.as_ref()),
                &format!("{context} goal request"),
                &mut violations,
            );
            let goal_response = derive_schema(
                declared
                    .goal_service
                    .as_ref()
                    .and_then(|g| g.response_message_format.as_ref()),
                &format!("{context} goal response"),
                &mut violations,
            );
            let feedback = declared.feedback_topic.as_ref().map(|feedback_topic| {
                derive_schema(
                    Some(&feedback_topic.message_format),
                    &format!("{context} feedback"),
                    &mut violations,
                )
            });
            let result = derive_schema(
                declared
                    .result_service
                    .as_ref()
                    .and_then(|r| r.response_message_format.as_ref()),
                &format!("{context} result"),
                &mut violations,
            );
            let (Some(input_schema), Some(_), Some(output_schema)) =
                (goal_request, goal_response, result)
            else {
                continue;
            };
            let feedback_schema = match feedback {
                Some(Some(schema)) => Some(schema),
                Some(None) => continue,
                None => None,
            };
            tasks.push(TaskEntry {
                name: action.tool.as_str().to_string(),
                description: action.description.clone(),
                target: target_name.clone(),
                member: action.member.clone(),
                operation: action.operation,
                safety_sensitive: action.safety_sensitive,
                confirmation_required: action.confirmation_required,
                deadline_ms: action.deadline_ms,
                input_schema,
                output_schema,
                feedback_schema,
            });
            task_members.push(BoundMember::new(&slot, declared));
        }
        pins.push(slot);
    }

    if !violations.is_empty() {
        return Err(ExposureValidationError { violations });
    }

    Ok(ValidatedExposure {
        bundle: ExposureBundle {
            bundle_format: EXPOSURE_BUNDLE_FORMAT,
            schema_mapping_version: SCHEMA_MAPPING_VERSION,
            exposure: BundleIdentity {
                name: exposure.manifest.name.as_str().to_string(),
                tag: exposure.manifest.tag.clone(),
            },
            server: BundleServer {
                title: exposure.server.title.clone(),
                instructions: exposure.server.instructions.clone(),
            },
            contracts: pins,
            resources,
            tools,
            tasks,
        },
        resources: resource_members,
        tools: tool_members,
        tasks: task_members,
    })
}

/// The resource entry of a topic that validates; `None` records why it
/// does not.
fn check_topic(
    target_name: &str,
    topic: &TopicExposure,
    declared: &NativeEmittedTopic,
    violations: &mut Vec<String>,
) -> Option<ResourceEntry> {
    let context = format!("target `{target_name}` topic `{}`", topic.member);
    let format = declared.message_format.as_ref();
    let schema = derive_schema(format, &context, violations);

    if let Some(representation) = &topic.representation {
        check_representation(&context, &representation.fields, format, violations);
    }

    check_topic_size_policy(&context, topic, format, violations);

    let schema = schema?;
    Some(ResourceEntry {
        name: topic.resource.as_str().to_string(),
        uri: format!("peppy://resource/{}", topic.resource),
        description: topic.description.clone(),
        target: target_name.to_string(),
        member: topic.member.clone(),
        policies: ResourcePolicies {
            freshness: topic.freshness,
            update: topic.update,
            representation: topic.representation.clone(),
            max_result_bytes: topic.max_result_bytes,
            on_oversize: topic.on_oversize,
        },
        schema,
    })
}

/// A topic with a statically bounded payload is checked at validation; a
/// topic without one must carry the runtime size policy instead. Whether a
/// payload is bounded is read from the message format alone: an image
/// representation changes the content a client sees, not the format the
/// bound is computed from.
fn check_topic_size_policy(
    context: &str,
    topic: &TopicExposure,
    format: Option<&MessageFormat>,
    violations: &mut Vec<String>,
) {
    let max = match format {
        // `{}`.
        None => MaxSerializedSize::Bounded(2),
        Some(format) => max_serialized_json_bytes(format),
    };
    match max {
        MaxSerializedSize::Unbounded => {
            if topic.max_result_bytes.is_none() || topic.on_oversize.is_none() {
                violations.push(format!(
                    "{context}: the topic's payload has no static maximum; declare \
                     `max_result_bytes` and `on_oversize` (`reject` or `downscale`)"
                ));
            }
        }
        MaxSerializedSize::Bounded(max) => {
            if topic.on_oversize.is_some() {
                violations.push(format!(
                    "{context}: the topic's payload is bounded ({max} bytes serialized at \
                     most); `on_oversize` never applies, remove it"
                ));
            }
            if let Some(limit) = topic.max_result_bytes
                && max > limit.get()
            {
                violations.push(format!(
                    "{context}: the payload's maximum serialized size ({max} bytes) exceeds \
                     `max_result_bytes` ({limit})"
                ));
            }
        }
    }
}

/// The tool entry of a service that validates; `None` records why it does
/// not.
fn check_service(
    target_name: &str,
    service: &ServiceExposure,
    declared: &NativeExposedService,
    violations: &mut Vec<String>,
) -> Option<ToolEntry> {
    let context = format!("target `{target_name}` service `{}`", service.member);
    let request_format = declared.request_message_format.as_ref();
    let response_format = declared.response_message_format.as_ref();
    let input_schema = derive_schema(request_format, &format!("{context} request"), violations);
    let output_schema = derive_schema(response_format, &format!("{context} response"), violations);

    if let Some(limit) = service.max_result_bytes
        && let Some(response_format) = response_format
        && let MaxSerializedSize::Bounded(max) = max_serialized_json_bytes(response_format)
        && max > limit.get()
    {
        violations.push(format!(
            "{context}: the response's maximum serialized size ({max} bytes) exceeds \
             `max_result_bytes` ({limit})"
        ));
    }

    let (Some(mut input_schema), Some(output_schema)) = (input_schema, output_schema) else {
        return None;
    };
    apply_restrictions(
        &context,
        service,
        request_format,
        &mut input_schema,
        violations,
    );
    Some(ToolEntry {
        name: service.tool.as_str().to_string(),
        description: service.description.clone(),
        target: target_name.to_string(),
        member: service.member.clone(),
        operation: service.operation,
        deadline_ms: service.deadline_ms,
        max_result_bytes: service.max_result_bytes,
        input_schema,
        output_schema,
    })
}

/// Reflect `restrict` bounds into the derived input schema as
/// `minimum`/`maximum`: the shape stays the contract's, the bounds are the
/// exposure's. For the integer members up to 32 bits the derived schema
/// already carries the type's own range, so a one-sided restriction tightens
/// one side and keeps the other.
fn apply_restrictions(
    context: &str,
    service: &ServiceExposure,
    request_format: Option<&MessageFormat>,
    input_schema: &mut Value,
    violations: &mut Vec<String>,
) {
    for (field, bounds) in &service.restrict {
        let bound_context = format!("{context}: `restrict.{field}`");
        let Some(schema_type) = request_format.and_then(|format| format.0.get(field)) else {
            violations.push(format!(
                "{bound_context} names no root member of the request format"
            ));
            continue;
        };
        let Some(token) = schema_type.as_type_token() else {
            violations.push(format!(
                "{bound_context} must name a numeric member, but `{field}` is an object or array"
            ));
            continue;
        };
        let range = match integer_range(token) {
            NumericKind::Integer(range) => Some(range),
            NumericKind::Float => None,
            NumericKind::DecimalString => {
                violations.push(format!(
                    "{bound_context}: bounds on a `{}` member cannot be reflected into its \
                     decimal-string schema; narrow the contract type instead",
                    type_token_name(token)
                ));
                continue;
            }
            NumericKind::NotNumeric => {
                violations.push(format!(
                    "{bound_context} must name a numeric member, but `{field}` is `{}`",
                    type_token_name(token)
                ));
                continue;
            }
        };

        // A passing bound comes back as the validated number the schema
        // should carry. An empty range (`min` above `max`) needs no contract
        // to spot, so the document model rejects it at parse time and it
        // cannot reach here.
        let mut checked =
            |bound: &Option<serde_json::Number>, side: &str| -> Option<serde_json::Number> {
                let bound = bound.as_ref()?;
                if let Some((low, high)) = range {
                    let Some(value) = bound.as_i64() else {
                        violations.push(format!(
                            "{bound_context}: `{side}` must be an integer for the `{}` member, \
                             got {bound}",
                            type_token_name(token)
                        ));
                        return None;
                    };
                    if value < low || value > high {
                        violations.push(format!(
                            "{bound_context}: `{side}` ({value}) is outside `{}`'s range \
                             [{low}, {high}]",
                            type_token_name(token)
                        ));
                        return None;
                    }
                }
                Some(bound.clone())
            };
        let min = checked(&bounds.min, "min");
        let max = checked(&bounds.max, "max");

        let Some(properties) = input_schema
            .get_mut("properties")
            .and_then(|properties| properties.get_mut(field))
            .and_then(Value::as_object_mut)
        else {
            continue;
        };
        if let Some(bound) = min {
            properties.insert("minimum".to_string(), Value::Number(bound));
        }
        if let Some(bound) = max {
            properties.insert("maximum".to_string(), Value::Number(bound));
        }
    }
}

enum NumericKind {
    Integer((i64, i64)),
    Float,
    DecimalString,
    NotNumeric,
}

fn integer_range(token: &TypeToken) -> NumericKind {
    if let Some(range) = integer_bounds(token) {
        return NumericKind::Integer(range);
    }
    match token {
        TypeToken::F32 | TypeToken::F64 => NumericKind::Float,
        TypeToken::U64 | TypeToken::I64 => NumericKind::DecimalString,
        _ => NumericKind::NotNumeric,
    }
}

struct RepresentationRole<'a> {
    name: &'static str,
    member: &'a str,
    accepts: fn(&SchemaType) -> bool,
    expected: &'static str,
}

/// Check an image representation's field mapping against the topic's message
/// format: every role must name a required root member of the right type.
fn check_representation(
    context: &str,
    fields: &ImageFieldMap,
    format: Option<&MessageFormat>,
    violations: &mut Vec<String>,
) {
    let Some(format) = format else {
        violations.push(format!(
            "{context}: the representation names members of the topic's message format, but \
             the topic declares none"
        ));
        return;
    };
    let roles = [
        RepresentationRole {
            name: "data",
            member: &fields.data,
            accepts: is_byte_carrier,
            expected: "`bytes` or an array of `u8`",
        },
        RepresentationRole {
            name: "encoding",
            member: &fields.encoding,
            accepts: is_string,
            expected: "`string`",
        },
        RepresentationRole {
            name: "width",
            member: &fields.width,
            accepts: is_dimension,
            expected: "`u8`, `u16`, or `u32`",
        },
        RepresentationRole {
            name: "height",
            member: &fields.height,
            accepts: is_dimension,
            expected: "`u8`, `u16`, or `u32`",
        },
    ];
    for RepresentationRole {
        name,
        member,
        accepts,
        expected,
    } in roles
    {
        let Some(schema_type) = format.0.get(member) else {
            violations.push(format!(
                "{context}: representation field `{name}` names `{member}`, but the topic's \
                 message format has no root member `{member}`"
            ));
            continue;
        };
        if schema_type.is_optional() {
            violations.push(format!(
                "{context}: representation field `{name}` names `{member}`, which is \
                 `$optional`; a frame the runtime interprets must always carry it"
            ));
            continue;
        }
        if !accepts(schema_type) {
            violations.push(format!(
                "{context}: representation field `{name}` names `{member}`, which must be \
                 {expected}"
            ));
        }
    }
}

fn is_byte_carrier(schema: &SchemaType) -> bool {
    match schema {
        SchemaType::Array(array) => array.items.as_ref().as_type_token() == Some(&TypeToken::U8),
        _ => schema.as_type_token() == Some(&TypeToken::Bytes),
    }
}

fn is_string(schema: &SchemaType) -> bool {
    schema.as_type_token() == Some(&TypeToken::String)
}

fn is_dimension(schema: &SchemaType) -> bool {
    matches!(
        schema.as_type_token(),
        Some(TypeToken::U8 | TypeToken::U16 | TypeToken::U32)
    )
}

/// Map a member's format under the canonical mapping, or record why it does
/// not translate. An absent format is the empty payload.
fn derive_schema(
    format: Option<&MessageFormat>,
    context: &str,
    violations: &mut Vec<String>,
) -> Option<Value> {
    match format {
        None => Some(empty_object_schema()),
        Some(format) => match message_format_to_json_schema(format) {
            Ok(schema) => Some(schema),
            Err(error) => {
                violations.push(format!("{context}: {error}"));
                None
            }
        },
    }
}

#[derive(Clone, Copy, PartialEq)]
enum MemberKind {
    Topic,
    Service,
    Action,
}

impl MemberKind {
    fn singular(self) -> &'static str {
        match self {
            Self::Topic => "topic",
            Self::Service => "service",
            Self::Action => "action",
        }
    }

    fn section(self) -> &'static str {
        match self {
            Self::Topic => "topics",
            Self::Service => "services",
            Self::Action => "actions",
        }
    }
}

fn find_topic<'a>(contract: &ResolvedContract<'a>, member: &str) -> Option<&'a NativeEmittedTopic> {
    contract.topics.iter().find(|t| t.name == member)
}

fn find_service<'a>(
    contract: &ResolvedContract<'a>,
    member: &str,
) -> Option<&'a NativeExposedService> {
    contract.services.iter().find(|s| s.name == member)
}

fn find_action<'a>(
    contract: &ResolvedContract<'a>,
    member: &str,
) -> Option<&'a NativeExposedAction> {
    contract.actions.iter().find(|a| a.name == member)
}

/// The names a contract declares under one member kind.
fn member_names<'a>(contract: &ResolvedContract<'a>, kind: MemberKind) -> Vec<&'a str> {
    match kind {
        MemberKind::Topic => contract.topics.iter().map(|t| t.name.as_str()).collect(),
        MemberKind::Service => contract.services.iter().map(|s| s.name.as_str()).collect(),
        MemberKind::Action => contract.actions.iter().map(|a| a.name.as_str()).collect(),
    }
}

/// Name the missing member, list what the contract does declare of that
/// kind, and point at the right section when a member of the same name
/// exists under another kind.
fn missing_member_violation(
    target_name: &str,
    kind: MemberKind,
    member: &str,
    contract_label: &str,
    contract: &ResolvedContract<'_>,
) -> String {
    let declared = member_names(contract, kind);
    let mut message = format!(
        "target `{target_name}` selects {} member `{member}`, but contract `{contract_label}` \
         declares no such {}",
        kind.singular(),
        kind.singular()
    );
    if declared.is_empty() {
        message.push_str(&format!(" (it declares no {})", kind.section()));
    } else {
        message.push_str(&format!(
            " (declared {}: `{}`)",
            kind.section(),
            declared.join("`, `")
        ));
    }
    for other in [MemberKind::Topic, MemberKind::Service, MemberKind::Action] {
        if other == kind {
            continue;
        }
        if member_names(contract, other).contains(&member) {
            message.push_str(&format!(
                "; a {} with that name exists, select it under `{}`",
                other.singular(),
                other.section()
            ));
        }
    }
    message
}

#[cfg(test)]
mod tests;
