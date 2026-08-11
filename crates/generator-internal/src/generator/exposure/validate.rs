//! Publication-time validation of an exposure against its pinned contracts.
//!
//! [`build_exposure_bundle`] is a pure function: the exposure document and
//! the resolved contract documents go in, the bundle or the full list of
//! violations comes out. It runs identically in CI, at publication, and in
//! tests; resolving pinned bytes out of the repository machinery is the
//! caller's job.

use super::bundle::{
    BundleContractPin, BundleIdentity, BundleNode, BundleServer, EXPOSURE_BUNDLE_FORMAT,
    ExposureBundle, ResourceEntry, ResourcePolicies, TaskEntry, ToolEntry,
};
use super::json_schema::{
    MaxSerializedSize, SCHEMA_MAPPING_VERSION, empty_object_schema, max_serialized_json_bytes,
    message_format_to_json_schema,
};
use config::node::{
    MessageFormat, NativeEmittedTopic, NativeExposedAction, NativeExposedService, SchemaType,
    TypeToken,
};
use config::type_token_name;
use daemon_config::contract::PeppyContract;
use daemon_config::mcp_exposure::{McpExposure, ServiceExposure, TopicExposure};
use daemon_config::repository::ManifestFingerprint;
use serde_json::Value;
use std::collections::BTreeMap;
use std::fmt;

/// One contract document as resolved for validation: the parsed document and
/// the fingerprint of the exact bytes it was parsed from, so pin equality is
/// checked against content rather than trust.
#[derive(Debug, Clone)]
pub struct ResolvedContractDocument {
    pub sha256: ManifestFingerprint,
    pub document: PeppyContract,
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

/// Validate `exposure` against `contracts` and derive its bundle.
///
/// Publication succeeds only when every selected member exists in its
/// referenced contract with the expected kind, every message definition
/// translates under the canonical schema mapping, representation policies
/// name real members with the right types, `restrict` bounds fit their
/// members, size limits hold wherever a payload has a finite maximum, and
/// every pin matches the resolved document's bytes.
pub fn build_exposure_bundle(
    exposure: &McpExposure,
    contracts: &[ResolvedContractDocument],
) -> Result<ExposureBundle, ExposureValidationError> {
    let mut violations = Vec::new();

    let mut by_identity: BTreeMap<(&str, &str), &ResolvedContractDocument> = BTreeMap::new();
    for resolved in contracts {
        let manifest = &resolved.document.manifest;
        let key = (manifest.name.as_str(), manifest.tag.as_str());
        if by_identity.insert(key, resolved).is_some() {
            violations.push(format!(
                "contract `{}:{}` was provided more than once",
                manifest.name, manifest.tag
            ));
        }
    }

    let mut pins = Vec::new();
    let mut resources = Vec::new();
    let mut tools = Vec::new();
    let mut tasks = Vec::new();

    for (target_name, target) in &exposure.targets {
        let reference = &target.contract;
        let contract_label = format!("{}:{}", reference.name, reference.tag);
        pins.push(BundleContractPin {
            name: reference.name.as_str().to_string(),
            tag: reference.tag.clone(),
            sha256: reference.sha256.to_string(),
            link_id: target_name.clone(),
        });

        let Some(resolved) = by_identity.get(&(reference.name.as_str(), reference.tag.as_str()))
        else {
            violations.push(format!(
                "target `{target_name}` references contract `{contract_label}`, which was not \
                 provided"
            ));
            continue;
        };
        if resolved.sha256 != reference.sha256 {
            violations.push(format!(
                "target `{target_name}` pins contract `{contract_label}` at sha256 \
                 `{}`, but the resolved document's bytes fingerprint to `{}`",
                reference.sha256, resolved.sha256
            ));
            continue;
        }
        let contract = &resolved.document;

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
            check_topic(
                target_name,
                topic,
                declared,
                &mut violations,
                &mut resources,
            );
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
            check_service(target_name, service, declared, &mut violations, &mut tools);
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
        }
    }

    if !violations.is_empty() {
        return Err(ExposureValidationError { violations });
    }

    Ok(ExposureBundle {
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
        node: BundleNode {
            name: format!("{}_mcp", exposure.manifest.name.as_str()),
            tag: exposure.manifest.tag.clone(),
            contracts: pins,
        },
        resources,
        tools,
        tasks,
    })
}

fn check_topic(
    target_name: &str,
    topic: &TopicExposure,
    declared: &NativeEmittedTopic,
    violations: &mut Vec<String>,
    resources: &mut Vec<ResourceEntry>,
) {
    let context = format!("target `{target_name}` topic `{}`", topic.member);
    let format = declared.message_format.as_ref();
    let schema = derive_schema(format, &context, violations);

    if let Some(representation) = &topic.representation {
        check_representation(&context, &representation.fields, format, violations);
    }

    check_topic_size_policy(&context, topic, format, violations);

    let Some(schema) = schema else { return };
    resources.push(ResourceEntry {
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
    });
}

/// A topic with a statically bounded payload is checked at publication; a
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

fn check_service(
    target_name: &str,
    service: &ServiceExposure,
    declared: &NativeExposedService,
    violations: &mut Vec<String>,
    tools: &mut Vec<ToolEntry>,
) {
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
        return;
    };
    apply_restrictions(
        &context,
        service,
        request_format,
        &mut input_schema,
        violations,
    );
    tools.push(ToolEntry {
        name: service.tool.as_str().to_string(),
        description: service.description.clone(),
        target: target_name.to_string(),
        member: service.member.clone(),
        operation: service.operation,
        deadline_ms: service.deadline_ms,
        max_result_bytes: service.max_result_bytes,
        input_schema,
        output_schema,
    });
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

        // A passing bound comes back as its comparison value paired with the
        // validated number the schema should carry.
        let mut checked =
            |bound: &Option<serde_json::Number>, side: &str| -> Option<(f64, serde_json::Number)> {
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
                    return Some((value as f64, bound.clone()));
                }
                bound.as_f64().map(|value| (value, bound.clone()))
            };
        let min = checked(&bounds.min, "min");
        let max = checked(&bounds.max, "max");
        if let (Some((min, _)), Some((max, _))) = (&min, &max)
            && min > max
        {
            violations.push(format!(
                "{bound_context}: `min` ({min}) is greater than `max` ({max})"
            ));
            continue;
        }

        let Some(properties) = input_schema
            .get_mut("properties")
            .and_then(|properties| properties.get_mut(field))
            .and_then(Value::as_object_mut)
        else {
            continue;
        };
        if let Some((_, bound)) = min {
            properties.insert("minimum".to_string(), Value::Number(bound));
        }
        if let Some((_, bound)) = max {
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
    if let Some(range) = super::json_schema::integer_bounds(token) {
        return NumericKind::Integer(range);
    }
    match token {
        TypeToken::F32 | TypeToken::F64 => NumericKind::Float,
        TypeToken::U64 | TypeToken::I64 => NumericKind::DecimalString,
        _ => NumericKind::NotNumeric,
    }
}

/// Check an image representation's field mapping against the topic's message
/// format: every role must name a required root member of the right type.
fn check_representation(
    context: &str,
    fields: &daemon_config::mcp_exposure::ImageFieldMap,
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
    let roles: [(&str, &str, fn(&SchemaType) -> bool, &str); 4] = [
        (
            "data",
            &fields.data,
            is_byte_carrier,
            "`bytes` or an array of `u8`",
        ),
        ("encoding", &fields.encoding, is_string, "`string`"),
        (
            "width",
            &fields.width,
            is_dimension,
            "`u8`, `u16`, or `u32`",
        ),
        (
            "height",
            &fields.height,
            is_dimension,
            "`u8`, `u16`, or `u32`",
        ),
    ];
    for (role, member, accepts, expected) in roles {
        let Some(schema_type) = format.0.get(member) else {
            violations.push(format!(
                "{context}: representation field `{role}` names `{member}`, but the topic's \
                 message format has no root member `{member}`"
            ));
            continue;
        };
        if schema_type.is_optional() {
            violations.push(format!(
                "{context}: representation field `{role}` names `{member}`, which is \
                 `$optional`; a frame the runtime interprets must always carry it"
            ));
            continue;
        }
        if !accepts(schema_type) {
            violations.push(format!(
                "{context}: representation field `{role}` names `{member}`, which must be \
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
        Some(format) => match message_format_to_json_schema(format, context) {
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

pub(super) fn find_topic<'a>(
    contract: &'a PeppyContract,
    member: &str,
) -> Option<&'a NativeEmittedTopic> {
    contract.interfaces.topics.iter().find(|t| t.name == member)
}

pub(super) fn find_service<'a>(
    contract: &'a PeppyContract,
    member: &str,
) -> Option<&'a NativeExposedService> {
    contract
        .interfaces
        .services
        .iter()
        .find(|s| s.name == member)
}

pub(super) fn find_action<'a>(
    contract: &'a PeppyContract,
    member: &str,
) -> Option<&'a NativeExposedAction> {
    contract
        .interfaces
        .actions
        .iter()
        .find(|a| a.name == member)
}

/// The names a contract declares under one member kind.
fn member_names(contract: &PeppyContract, kind: MemberKind) -> Vec<&str> {
    match kind {
        MemberKind::Topic => contract
            .interfaces
            .topics
            .iter()
            .map(|t| t.name.as_str())
            .collect(),
        MemberKind::Service => contract
            .interfaces
            .services
            .iter()
            .map(|s| s.name.as_str())
            .collect(),
        MemberKind::Action => contract
            .interfaces
            .actions
            .iter()
            .map(|a| a.name.as_str())
            .collect(),
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
    contract: &PeppyContract,
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
