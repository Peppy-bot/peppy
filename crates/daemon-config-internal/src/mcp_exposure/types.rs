use crate::internal::contract::{Manifest, deserialize_tag};
use crate::internal::repository::ManifestFingerprint;
use config::{runtime::Name, schema::PeppySchema};
use indexmap::IndexMap;
use serde::{
    Deserialize, Serialize,
    de::{self, Deserializer},
};
use std::collections::HashSet;
use std::num::NonZeroU64;

// The operational policy vocabulary is shared with the exposure bundle and
// the MCP server runtime, so it lives in the `peppy-mcp-catalog` crate and
// is re-exported here for the document model.
pub use peppy_mcp_catalog::{
    ActionOperation, FreshnessPolicy, ImageCodec, ImageFieldMap, ImageRepresentation, JpegQuality,
    MaxHz, OversizePolicy, ServiceOperation, UpdatePolicy,
};

/// Reject any `peppy_schema` value other than `mcp_exposure/v1` so a node,
/// launcher, or contract document can't slip through `PeppyMcpExposureParser`.
fn deserialize_mcp_exposure_v1_schema<'de, D>(deserializer: D) -> Result<PeppySchema, D::Error>
where
    D: Deserializer<'de>,
{
    PeppySchema::deserialize_expecting(deserializer, PeppySchema::McpExposureV1)
}

/// An MCP exposure document (`peppy_schema: "mcp_exposure/v1"`): the
/// allowlist that turns selected members of pinned Peppy contracts into a
/// public MCP surface. The document carries selection, public naming, prose,
/// and operational policy only; every request and response shape is derived
/// from the referenced contracts, so the internal and external definitions
/// cannot drift apart. Anything not named here stays private.
///
/// Exposure documents take the contract shape in the repository: a tagged
/// manifest, indexed by name and tag, sha256-pinned wherever referenced.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(try_from = "RawMcpExposure")]
pub struct McpExposure {
    pub peppy_schema: PeppySchema,
    pub manifest: Manifest,
    pub server: ServerIdentity,
    pub targets: IndexMap<String, ExposureTarget>,
}

/// Wire shape of [`McpExposure`]. Deserialization funnels through
/// `TryFrom<RawMcpExposure>` so the document-level coherence rules (non-empty
/// target set, per-target member selection, unique public names, policy
/// cross-field rules) hold on every parsed value.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawMcpExposure {
    #[serde(deserialize_with = "deserialize_mcp_exposure_v1_schema")]
    peppy_schema: PeppySchema,
    manifest: Manifest,
    server: ServerIdentity,
    #[serde(deserialize_with = "deserialize_targets")]
    targets: IndexMap<String, ExposureTarget>,
}

impl TryFrom<RawMcpExposure> for McpExposure {
    type Error = String;

    fn try_from(raw: RawMcpExposure) -> Result<Self, String> {
        if raw.targets.is_empty() {
            return Err("an MCP exposure must select at least one target".to_string());
        }
        let mut public_names: HashSet<&str> = HashSet::new();
        for (target_name, target) in &raw.targets {
            target.check_coherence(target_name)?;
            for name in target.public_names() {
                if !public_names.insert(name) {
                    return Err(format!(
                        "public name `{name}` is declared more than once; resource and tool \
                         names share one namespace across the exposure"
                    ));
                }
            }
        }
        Ok(Self {
            peppy_schema: raw.peppy_schema,
            manifest: raw.manifest,
            server: raw.server,
            targets: raw.targets,
        })
    }
}

/// Deserialize the target map, rejecting duplicate target names and names
/// that could not serve as the `link_id` of a generated contract slot.
fn deserialize_targets<'de, D>(
    deserializer: D,
) -> Result<IndexMap<String, ExposureTarget>, D::Error>
where
    D: Deserializer<'de>,
{
    deserialize_unique_map(
        deserializer,
        "a map of target names to exposure targets",
        "target",
        |name| config::repo_node_id::validate_repo_node_name(name, "target name"),
    )
}

/// Deserialize a document-order map, running `check_key` on each key and
/// rejecting duplicates as `duplicate {key_label} `{key}``.
fn deserialize_unique_map<'de, D, V>(
    deserializer: D,
    expecting: &'static str,
    key_label: &'static str,
    check_key: impl Fn(&str) -> Result<(), String>,
) -> Result<IndexMap<String, V>, D::Error>
where
    D: Deserializer<'de>,
    V: Deserialize<'de>,
{
    struct UniqueMapVisitor<F, V> {
        expecting: &'static str,
        key_label: &'static str,
        check_key: F,
        value: std::marker::PhantomData<V>,
    }

    impl<'de, F, V> de::Visitor<'de> for UniqueMapVisitor<F, V>
    where
        F: Fn(&str) -> Result<(), String>,
        V: Deserialize<'de>,
    {
        type Value = IndexMap<String, V>;

        fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
            formatter.write_str(self.expecting)
        }

        fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
        where
            A: de::MapAccess<'de>,
        {
            let mut entries = IndexMap::new();
            while let Some((key, value)) = map.next_entry::<String, V>()? {
                (self.check_key)(&key).map_err(de::Error::custom)?;
                if entries.insert(key.clone(), value).is_some() {
                    return Err(de::Error::custom(format!(
                        "duplicate {} `{key}`",
                        self.key_label
                    )));
                }
            }
            Ok(entries)
        }
    }

    deserializer.deserialize_map(UniqueMapVisitor {
        expecting,
        key_label,
        check_key,
        value: std::marker::PhantomData,
    })
}

/// Server-level identity advertised to MCP clients through `server/discover`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ServerIdentity {
    #[serde(deserialize_with = "deserialize_prose")]
    pub title: String,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional_prose"
    )]
    pub instructions: Option<String>,
}

/// A pinned reference to the contract a target draws its members from. The
/// sha256 pin is mandatory: a `(name, tag)` alone does not fix content, and
/// the public schemas derived from the contract must be reproducible wherever
/// the exposure is resolved.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct PinnedContractRef {
    pub name: Name,
    #[serde(deserialize_with = "deserialize_tag")]
    pub tag: String,
    pub sha256: ManifestFingerprint,
}

/// One logical target: the contract it draws from and the members it makes
/// public. The target's key in [`McpExposure::targets`] becomes the `link_id`
/// of a `depends_on.contracts` slot on the generated MCP server node, so the
/// launcher decides which concrete instance fills it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ExposureTarget {
    pub contract: PinnedContractRef,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub topics: Vec<TopicExposure>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub services: Vec<ServiceExposure>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub actions: Vec<ActionExposure>,
}

impl ExposureTarget {
    /// The document-level rules a single target must satisfy, with
    /// `target_name` naming the target in every error.
    fn check_coherence(&self, target_name: &str) -> Result<(), String> {
        if self.topics.is_empty() && self.services.is_empty() && self.actions.is_empty() {
            return Err(format!(
                "target `{target_name}` selects no members; list at least one topic, service, \
                 or action"
            ));
        }
        check_unique_members(
            target_name,
            "topic",
            self.topics.iter().map(|t| t.member.as_str()),
        )?;
        check_unique_members(
            target_name,
            "service",
            self.services.iter().map(|s| s.member.as_str()),
        )?;
        check_unique_members(
            target_name,
            "action",
            self.actions.iter().map(|a| a.member.as_str()),
        )?;
        for topic in &self.topics {
            topic.check_coherence(target_name)?;
        }
        for service in &self.services {
            service.check_coherence(target_name)?;
        }
        Ok(())
    }

    /// Every public resource and tool name this target declares, in
    /// document order.
    fn public_names(&self) -> impl Iterator<Item = &str> {
        self.topics
            .iter()
            .map(|t| t.resource.as_str())
            .chain(self.services.iter().map(|s| s.tool.as_str()))
            .chain(self.actions.iter().map(|a| a.tool.as_str()))
    }
}

fn check_unique_members<'a>(
    target_name: &str,
    kind: &'static str,
    members: impl Iterator<Item = &'a str>,
) -> Result<(), String> {
    let mut seen = HashSet::new();
    for member in members {
        if !seen.insert(member) {
            return Err(format!(
                "target `{target_name}` selects {kind} member `{member}` more than once"
            ));
        }
    }
    Ok(())
}

/// A topic member exposed as an MCP resource. The generated node maintains
/// the latest policy-approved snapshot of the linked Peppy topic; clients
/// read the resource to obtain it and subscribe to be told when it changes.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct TopicExposure {
    #[serde(deserialize_with = "deserialize_member")]
    pub member: String,
    pub resource: PublicName,
    #[serde(deserialize_with = "deserialize_prose")]
    pub description: String,
    pub freshness: FreshnessPolicy,
    pub update: UpdatePolicy,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub representation: Option<ImageRepresentation>,
    /// Cap on the serialized size of the published snapshot content, in
    /// bytes. Applies to the final serialized form, after representation
    /// policies run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_result_bytes: Option<NonZeroU64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on_oversize: Option<OversizePolicy>,
}

impl TopicExposure {
    fn check_coherence(&self, target_name: &str) -> Result<(), String> {
        let context = format!("target `{target_name}` topic `{}`", self.member);
        if let Some(representation) = &self.representation
            && representation.quality.is_some()
            && representation.image != ImageCodec::Jpeg
        {
            return Err(format!(
                "{context}: `quality` applies only to the `jpeg` image representation"
            ));
        }
        if self.on_oversize.is_some() && self.max_result_bytes.is_none() {
            return Err(format!(
                "{context}: `on_oversize` requires `max_result_bytes` to set the size it acts on"
            ));
        }
        if self.on_oversize == Some(OversizePolicy::Downscale)
            && !self
                .representation
                .as_ref()
                .is_some_and(|r| r.image == ImageCodec::Jpeg)
        {
            return Err(format!(
                "{context}: `on_oversize: \"downscale\"` requires a `jpeg` image representation"
            ));
        }
        Ok(())
    }
}

/// A service member exposed as an MCP tool that completes within one
/// request.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ServiceExposure {
    #[serde(deserialize_with = "deserialize_member")]
    pub member: String,
    pub tool: PublicName,
    #[serde(deserialize_with = "deserialize_prose")]
    pub description: String,
    pub operation: ServiceOperation,
    pub deadline_ms: NonZeroU64,
    /// Inclusive numeric bounds narrowing request fields of the contract
    /// shape, keyed by field name. The derived schema keeps the contract's
    /// types; the bounds are reflected into the published input schema as
    /// `minimum`/`maximum`.
    #[serde(
        default,
        skip_serializing_if = "IndexMap::is_empty",
        deserialize_with = "deserialize_restrict"
    )]
    pub restrict: IndexMap<String, RestrictBounds>,
    /// Cap on the serialized size of the tool result content, in bytes. A
    /// response exceeding it is reported as a tool error.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_result_bytes: Option<NonZeroU64>,
}

impl ServiceExposure {
    fn check_coherence(&self, target_name: &str) -> Result<(), String> {
        for (field, bounds) in &self.restrict {
            if bounds.min.is_none() && bounds.max.is_none() {
                return Err(format!(
                    "target `{target_name}` service `{}`: `restrict.{field}` must set `min`, \
                     `max`, or both",
                    self.member
                ));
            }
        }
        Ok(())
    }
}

/// Deserialize a `restrict` map, rejecting duplicate and blank field names.
fn deserialize_restrict<'de, D>(
    deserializer: D,
) -> Result<IndexMap<String, RestrictBounds>, D::Error>
where
    D: Deserializer<'de>,
{
    deserialize_unique_map(
        deserializer,
        "a map of request field names to bounds",
        "`restrict` field",
        |field| {
            if field.trim().is_empty() {
                Err("`restrict` field name cannot be empty".to_string())
            } else {
                Ok(())
            }
        },
    )
}

/// Inclusive numeric bounds on one request field. At least one bound must be
/// set.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct RestrictBounds {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min: Option<serde_json::Number>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max: Option<serde_json::Number>,
}

/// An action member exposed as an MCP tool backed by the MCP Tasks
/// extension: the call returns a task handle, action feedback drives the
/// observable task state, and cancellation forwards to the action's cancel
/// path.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ActionExposure {
    #[serde(deserialize_with = "deserialize_member")]
    pub member: String,
    pub tool: PublicName,
    #[serde(deserialize_with = "deserialize_prose")]
    pub description: String,
    pub operation: ActionOperation,
    #[serde(default, skip_serializing_if = "is_false")]
    pub safety_sensitive: bool,
    /// When set, the task pauses in `input_required` until the client
    /// confirms via `tasks/update` before the goal is sent.
    #[serde(default, skip_serializing_if = "is_false")]
    pub confirmation_required: bool,
    /// Whole-goal deadline in milliseconds.
    pub deadline_ms: NonZeroU64,
}

fn is_false(value: &bool) -> bool {
    !value
}

/// A stable public resource or tool name: 1 to 128 characters drawn from
/// ASCII letters, digits, `_`, `-`, and `.`, with `.` allowed only between
/// other characters. Public names live in one namespace across the whole
/// exposure document.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(try_from = "String")]
pub struct PublicName(String);

impl PublicName {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for PublicName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl TryFrom<String> for PublicName {
    type Error = String;

    fn try_from(value: String) -> Result<Self, String> {
        if value.is_empty() {
            return Err("public name cannot be empty".to_string());
        }
        if value.len() > 128 {
            return Err(format!(
                "public name `{value}` is longer than 128 characters"
            ));
        }
        if let Some(bad) = value
            .chars()
            .find(|c| !(c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.')))
        {
            return Err(format!(
                "public name `{value}` contains disallowed character {bad:?} (allowed: ASCII \
                 letters, digits, `_`, `-`, `.`)"
            ));
        }
        if value.starts_with('.') || value.ends_with('.') || value.contains("..") {
            return Err(format!(
                "public name `{value}` cannot start or end with `.` or contain `..`"
            ));
        }
        Ok(Self(value))
    }
}

/// Reject blank member names so a selection always names something.
fn deserialize_member<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: Deserializer<'de>,
{
    let value = String::deserialize(deserializer)?;
    if value.trim().is_empty() {
        return Err(de::Error::custom("`member` cannot be empty"));
    }
    Ok(value)
}

/// Reject blank prose (titles, instructions, descriptions): the exposure
/// document is where MCP-facing text lives, so an empty string is always a
/// mistake.
fn deserialize_prose<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: Deserializer<'de>,
{
    let value = String::deserialize(deserializer)?;
    if value.trim().is_empty() {
        return Err(de::Error::custom("text cannot be empty"));
    }
    Ok(value)
}

fn deserialize_optional_prose<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: Deserializer<'de>,
{
    deserialize_prose(deserializer).map(Some)
}

#[cfg(test)]
mod tests {
    use super::*;

    const RGB_SHA: &str = "9f2c9f2c9f2c9f2c9f2c9f2c9f2c9f2c9f2c9f2c9f2c9f2c9f2c9f2c9f2c9f2c";
    const REC_SHA: &str = "4a814a814a814a814a814a814a814a814a814a814a814a814a814a814a814a81";

    fn parse(json5: &str) -> Result<McpExposure, String> {
        serde_json5::from_str::<McpExposure>(json5).map_err(|e| e.to_string())
    }

    fn parse_err(json5: &str) -> String {
        parse(json5).expect_err("expected the document to be rejected")
    }

    /// The complete surface from the design walkthrough: a camera target
    /// with a policy-rich topic and two services, and a recorder target
    /// with one confirmation-gated action.
    fn camera_and_recording() -> String {
        format!(
            r#"{{
            peppy_schema: "mcp_exposure/v1",
            manifest: {{ name: "camera_and_recording", tag: "v1" }},
            server: {{
                title: "OpenArm camera and recording",
                instructions: "Observe the front camera and record teleoperation episodes.",
            }},
            targets: {{
                front_camera: {{
                    contract: {{ name: "rgb_camera", tag: "v1", sha256: "{RGB_SHA}" }},
                    topics: [
                        {{
                            member: "video_stream",
                            resource: "front_camera.latest_frame",
                            description: "Latest frame from the front-facing camera, JPEG encoded.",
                            freshness: {{ max_age_ms: 2000 }},
                            update: {{ max_hz: 2 }},
                            representation: {{
                                image: "jpeg",
                                quality: 80,
                                fields: {{
                                    data: "frame",
                                    encoding: "encoding",
                                    width: "width",
                                    height: "height",
                                }},
                            }},
                            max_result_bytes: 524288,
                            on_oversize: "downscale",
                        }},
                    ],
                    services: [
                        {{
                            member: "video_stream_info",
                            tool: "front_camera.info",
                            description: "Report the camera's resolution, frame rate, and encoding.",
                            operation: "read_only",
                            deadline_ms: 2000,
                        }},
                        {{
                            member: "set_brightness",
                            tool: "front_camera.set_brightness",
                            description: "Set the camera brightness in device units.",
                            operation: "mutating",
                            deadline_ms: 2000,
                            restrict: {{ value: {{ min: -64, max: 64 }} }},
                        }},
                    ],
                }},
                recorder: {{
                    contract: {{ name: "episode_recording", tag: "v1", sha256: "{REC_SHA}" }},
                    actions: [
                        {{
                            member: "record_episode",
                            tool: "recorder.record_episode",
                            description: "Record one teleoperation episode to the local dataset.",
                            operation: "long_running",
                            safety_sensitive: true,
                            confirmation_required: true,
                            deadline_ms: 900000,
                        }},
                    ],
                }},
            }},
        }}"#
        )
    }

    /// A minimal valid document, as a base for the rejection tests below.
    fn minimal(services: &str) -> String {
        format!(
            r#"{{
            peppy_schema: "mcp_exposure/v1",
            manifest: {{ name: "surface", tag: "v1" }},
            server: {{ title: "Surface" }},
            targets: {{
                cam: {{
                    contract: {{ name: "rgb_camera", tag: "v1", sha256: "{RGB_SHA}" }},
                    services: [{services}],
                }},
            }},
        }}"#
        )
    }

    const INFO_SERVICE: &str = r#"{
        member: "video_stream_info",
        tool: "cam.info",
        description: "Report stream parameters.",
        operation: "read_only",
        deadline_ms: 2000,
    }"#;

    #[test]
    fn parses_the_design_walkthrough_document() {
        let exposure = parse(&camera_and_recording()).expect("parses");
        assert_eq!(exposure.manifest.name.as_str(), "camera_and_recording");
        assert_eq!(
            exposure.server.instructions.as_deref(),
            Some("Observe the front camera and record teleoperation episodes.")
        );

        let camera = &exposure.targets["front_camera"];
        assert_eq!(camera.contract.name.as_str(), "rgb_camera");
        assert_eq!(camera.contract.sha256.to_string(), RGB_SHA);
        let frame = &camera.topics[0];
        assert_eq!(frame.resource.as_str(), "front_camera.latest_frame");
        assert_eq!(frame.freshness.max_age_ms.get(), 2000);
        assert_eq!(frame.update.max_hz.get(), 2.0);
        assert_eq!(frame.on_oversize, Some(OversizePolicy::Downscale));
        let representation = frame.representation.as_ref().expect("representation");
        assert_eq!(representation.image, ImageCodec::Jpeg);
        assert_eq!(representation.quality.map(JpegQuality::get), Some(80));
        assert_eq!(representation.fields.data, "frame");

        let brightness = &camera.services[1];
        assert_eq!(brightness.operation, ServiceOperation::Mutating);
        let bounds = &brightness.restrict["value"];
        assert_eq!(bounds.min, Some(serde_json::Number::from(-64)));
        assert_eq!(bounds.max, Some(serde_json::Number::from(64)));

        let recorder = &exposure.targets["recorder"];
        let record = &recorder.actions[0];
        assert_eq!(record.operation, ActionOperation::LongRunning);
        assert!(record.safety_sensitive);
        assert!(record.confirmation_required);
        assert_eq!(record.deadline_ms.get(), 900000);
    }

    #[test]
    fn target_order_follows_the_document() {
        let exposure = parse(&camera_and_recording()).expect("parses");
        let names: Vec<&String> = exposure.targets.keys().collect();
        assert_eq!(names, ["front_camera", "recorder"]);
    }

    #[test]
    fn rejects_wrong_schema_tag() {
        let doc = camera_and_recording().replace("mcp_exposure/v1", "contract/v1");
        assert!(parse_err(&doc).contains("mcp_exposure/v1"));
    }

    #[test]
    fn rejects_unknown_top_level_fields() {
        let doc = camera_and_recording().replace("server:", "surprise: 1, server:");
        assert!(parse_err(&doc).contains("surprise"));
    }

    #[test]
    fn rejects_empty_targets() {
        let doc = r#"{
            peppy_schema: "mcp_exposure/v1",
            manifest: { name: "surface", tag: "v1" },
            server: { title: "Surface" },
            targets: {},
        }"#;
        assert!(parse_err(doc).contains("at least one target"));
    }

    #[test]
    fn rejects_a_target_selecting_no_members() {
        let doc = format!(
            r#"{{
            peppy_schema: "mcp_exposure/v1",
            manifest: {{ name: "surface", tag: "v1" }},
            server: {{ title: "Surface" }},
            targets: {{
                cam: {{ contract: {{ name: "rgb_camera", tag: "v1", sha256: "{RGB_SHA}" }} }},
            }},
        }}"#
        );
        let err = parse_err(&doc);
        assert!(
            err.contains("cam") && err.contains("selects no members"),
            "{err}"
        );
    }

    #[test]
    fn rejects_an_invalid_target_name() {
        let doc = minimal(INFO_SERVICE).replace("cam:", r#""cam era":"#);
        assert!(parse_err(&doc).contains("target name"));
    }

    #[test]
    fn rejects_a_missing_sha256_pin() {
        let doc = minimal(INFO_SERVICE).replace(&format!(r#", sha256: "{RGB_SHA}""#), "");
        assert!(parse_err(&doc).contains("sha256"));
    }

    #[test]
    fn rejects_a_malformed_sha256_pin() {
        let doc = minimal(INFO_SERVICE).replace(RGB_SHA, "9f2c");
        let err = parse_err(&doc);
        assert!(
            err.to_lowercase().contains("sha-256") || err.contains("64"),
            "{err}"
        );
    }

    #[test]
    fn rejects_a_dotted_contract_tag() {
        let doc = minimal(INFO_SERVICE).replace(r#"tag: "v1", sha256"#, r#"tag: "v0.1", sha256"#);
        assert!(parse_err(&doc).contains("disallowed character"));
    }

    #[test]
    fn rejects_duplicate_service_members_in_a_target() {
        let doc = minimal(&format!(
            "{INFO_SERVICE}, {}",
            INFO_SERVICE.replace("cam.info", "cam.info2")
        ));
        let err = parse_err(&doc);
        assert!(
            err.contains("video_stream_info") && err.contains("more than once"),
            "{err}"
        );
    }

    #[test]
    fn rejects_duplicate_public_names_across_targets() {
        let doc = format!(
            r#"{{
            peppy_schema: "mcp_exposure/v1",
            manifest: {{ name: "surface", tag: "v1" }},
            server: {{ title: "Surface" }},
            targets: {{
                cam_a: {{
                    contract: {{ name: "rgb_camera", tag: "v1", sha256: "{RGB_SHA}" }},
                    services: [{INFO_SERVICE}],
                }},
                cam_b: {{
                    contract: {{ name: "rgb_camera", tag: "v1", sha256: "{RGB_SHA}" }},
                    services: [{INFO_SERVICE}],
                }},
            }},
        }}"#
        );
        let err = parse_err(&doc);
        assert!(
            err.contains("cam.info") && err.contains("more than once"),
            "{err}"
        );
    }

    #[test]
    fn rejects_an_empty_server_title() {
        let doc = minimal(INFO_SERVICE).replace(r#"title: "Surface""#, r#"title: "  ""#);
        assert!(parse_err(&doc).contains("cannot be empty"));
    }

    #[test]
    fn rejects_empty_instructions() {
        let doc = minimal(INFO_SERVICE).replace(
            r#"title: "Surface""#,
            r#"title: "Surface", instructions: """#,
        );
        assert!(parse_err(&doc).contains("cannot be empty"));
    }

    #[test]
    fn rejects_an_empty_description() {
        let doc = minimal(INFO_SERVICE).replace(
            r#"description: "Report stream parameters.""#,
            r#"description: " ""#,
        );
        assert!(parse_err(&doc).contains("cannot be empty"));
    }

    #[test]
    fn rejects_a_zero_deadline() {
        let doc = minimal(INFO_SERVICE).replace("deadline_ms: 2000", "deadline_ms: 0");
        assert!(parse_err(&doc).contains("nonzero"));
    }

    #[test]
    fn rejects_a_non_positive_update_rate() {
        for max_hz in ["0", "-2", "Infinity"] {
            let doc = camera_and_recording().replace("max_hz: 2", &format!("max_hz: {max_hz}"));
            let err = parse_err(&doc);
            assert!(err.contains("max_hz"), "{max_hz}: {err}");
        }
    }

    #[test]
    fn rejects_out_of_range_jpeg_quality() {
        for quality in ["0", "101"] {
            let doc = camera_and_recording().replace("quality: 80", &format!("quality: {quality}"));
            let err = parse_err(&doc);
            assert!(err.contains("between 1 and 100"), "{quality}: {err}");
        }
    }

    #[test]
    fn rejects_quality_on_a_raw_representation() {
        let doc = camera_and_recording().replace(r#"image: "jpeg""#, r#"image: "raw""#);
        let err = parse_err(&doc);
        assert!(
            err.contains("`quality` applies only to the `jpeg`"),
            "{err}"
        );
    }

    #[test]
    fn rejects_on_oversize_without_a_size_limit() {
        let doc = camera_and_recording().replace("max_result_bytes: 524288,", "");
        let err = parse_err(&doc);
        assert!(
            err.contains("`on_oversize` requires `max_result_bytes`"),
            "{err}"
        );
    }

    #[test]
    fn rejects_downscale_without_a_jpeg_representation() {
        let doc = camera_and_recording()
            .replace(r#"image: "jpeg""#, r#"image: "raw""#)
            .replace("quality: 80,", "");
        let err = parse_err(&doc);
        assert!(
            err.contains(r#"`on_oversize: "downscale"` requires a `jpeg`"#),
            "{err}"
        );
    }

    #[test]
    fn accepts_reject_on_oversize_without_a_representation() {
        let doc = format!(
            r#"{{
            peppy_schema: "mcp_exposure/v1",
            manifest: {{ name: "surface", tag: "v1" }},
            server: {{ title: "Surface" }},
            targets: {{
                cam: {{
                    contract: {{ name: "rgb_camera", tag: "v1", sha256: "{RGB_SHA}" }},
                    topics: [
                        {{
                            member: "video_stream",
                            resource: "cam.latest_frame",
                            description: "Latest frame, canonical JSON rendering.",
                            freshness: {{ max_age_ms: 2000 }},
                            update: {{ max_hz: 0.5 }},
                            max_result_bytes: 524288,
                            on_oversize: "reject",
                        }},
                    ],
                }},
            }},
        }}"#
        );
        let exposure = parse(&doc).expect("a reject policy needs no image representation");
        let topic = &exposure.targets["cam"].topics[0];
        assert_eq!(topic.on_oversize, Some(OversizePolicy::Reject));
        assert_eq!(topic.update.max_hz.get(), 0.5);
        assert!(topic.representation.is_none());
    }

    #[test]
    fn rejects_a_restrict_entry_with_no_bounds() {
        let doc = camera_and_recording().replace("{ min: -64, max: 64 }", "{}");
        let err = parse_err(&doc);
        assert!(
            err.contains("`restrict.value` must set `min`, `max`, or both"),
            "{err}"
        );
    }

    #[test]
    fn rejects_a_long_running_service() {
        let doc = minimal(&INFO_SERVICE.replace("read_only", "long_running"));
        assert!(parse_err(&doc).contains("unknown variant"));
    }

    #[test]
    fn rejects_a_read_only_action() {
        let doc = camera_and_recording()
            .replace(r#"operation: "long_running""#, r#"operation: "read_only""#);
        assert!(parse_err(&doc).contains("unknown variant"));
    }

    #[test]
    fn rejects_bad_public_names() {
        for (bad, expected) in [
            (r#""""#, "cannot be empty"),
            (r#""has space""#, "disallowed character"),
            (r#"".leading""#, "cannot start or end with"),
            (r#""trailing.""#, "cannot start or end with"),
            (r#""double..dot""#, "cannot start or end with"),
        ] {
            let doc = minimal(&INFO_SERVICE.replace(r#""cam.info""#, bad));
            let err = parse_err(&doc);
            assert!(err.contains(expected), "{bad}: {err}");
        }
    }

    #[test]
    fn rejects_a_public_name_longer_than_128_characters() {
        let long = "x".repeat(129);
        let doc = minimal(&INFO_SERVICE.replace("cam.info", &long));
        assert!(parse_err(&doc).contains("longer than 128"));
    }

    #[test]
    fn round_trips_through_serde() {
        let exposure = parse(&camera_and_recording()).expect("parses");
        let serialized = serde_json::to_string(&exposure).expect("serializes");
        let reparsed: McpExposure = serde_json::from_str(&serialized).expect("reparses");
        assert_eq!(reparsed, exposure);
    }
}
