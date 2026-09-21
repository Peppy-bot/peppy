//! The serializable shape of a versioned exposure bundle.

use crate::policy::{
    ActionOperation, FreshnessPolicy, ImageRepresentation, OversizePolicy, ServiceOperation,
    UpdatePolicy,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::num::NonZeroU64;

/// Version of the bundle shape in this module.
pub const EXPOSURE_BUNDLE_FORMAT: u32 = 1;

/// Version of the canonical `message_format` to JSON Schema mapping whose
/// output the bundle's derived schemas carry. A reader refuses a bundle
/// mapped under a version it does not implement.
pub const SCHEMA_MAPPING_VERSION: u32 = 1;

/// Canonical decimal rendering of a `u64`: no leading zeros. Published in
/// derived input schemas and enforced by the runtime bridge, which is why
/// the pattern and its predicate live here, next to the mapping version.
pub const U64_DECIMAL_PATTERN: &str = "^(0|[1-9][0-9]*)$";

/// Canonical decimal rendering of an `i64`: no leading zeros, no `-0`.
pub const I64_DECIMAL_PATTERN: &str = "^(0|-?[1-9][0-9]*)$";

/// Whether `text` matches [`U64_DECIMAL_PATTERN`]. Range is the parse's
/// concern, not this predicate's.
pub fn is_canonical_u64_decimal(text: &str) -> bool {
    match text.as_bytes() {
        [b'0'] => true,
        [b'1'..=b'9', rest @ ..] => rest.iter().all(u8::is_ascii_digit),
        _ => false,
    }
}

/// Whether `text` matches [`I64_DECIMAL_PATTERN`].
pub fn is_canonical_i64_decimal(text: &str) -> bool {
    match text.strip_prefix('-') {
        Some(digits) => digits != "0" && is_canonical_u64_decimal(digits),
        None => is_canonical_u64_decimal(text),
    }
}

/// The product of validating one exposure document against its contracts:
/// the public catalog (stable names, prose, policies, derived JSON Schemas)
/// plus the identity the endpoint advertises and the contract slots its
/// targets become. A server derives it when it starts, and the catalog
/// command prints it on demand; it is never an artifact of its own.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExposureBundle {
    pub bundle_format: u32,
    pub schema_mapping_version: u32,
    pub exposure: BundleIdentity,
    pub server: BundleServer,
    /// The contract slot each logical target becomes: one slot per pin,
    /// with the pin's `link_id` as the slot the launcher fills.
    pub contracts: Vec<BundleContractPin>,
    pub resources: Vec<ResourceEntry>,
    pub tools: Vec<ToolEntry>,
    pub tasks: Vec<TaskEntry>,
}

impl ExposureBundle {
    /// Canonical serialized form: pretty JSON with a trailing newline, the
    /// shape the catalog command prints.
    pub fn to_json_string(&self) -> String {
        let pretty = serde_json::to_string_pretty(self).expect("bundle serializes");
        format!("{pretty}\n")
    }

    /// Parses a serialized bundle, refusing content whose format or schema
    /// mapping version this reader does not implement.
    pub fn from_json_str(content: &str) -> Result<Self, String> {
        #[derive(Deserialize)]
        struct VersionProbe {
            bundle_format: u32,
            schema_mapping_version: u32,
        }

        let probe: VersionProbe = serde_json::from_str(content)
            .map_err(|error| format!("exposure bundle is not valid JSON: {error}"))?;
        if probe.bundle_format != EXPOSURE_BUNDLE_FORMAT {
            return Err(format!(
                "exposure bundle format {} is not supported; this reader implements format {}",
                probe.bundle_format, EXPOSURE_BUNDLE_FORMAT
            ));
        }
        if probe.schema_mapping_version != SCHEMA_MAPPING_VERSION {
            return Err(format!(
                "exposure bundle schema mapping version {} is not supported; this reader \
                 implements version {}",
                probe.schema_mapping_version, SCHEMA_MAPPING_VERSION
            ));
        }
        serde_json::from_str(content).map_err(|error| format!("invalid exposure bundle: {error}"))
    }
}

/// Identity of the exposure document the bundle was derived from,
/// advertised through `server/discover` as the implementation name and
/// version.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BundleIdentity {
    pub name: String,
    pub tag: String,
}

impl BundleIdentity {
    /// The path the exposure is served under on its process's listener:
    /// `/<name>/<tag>/mcp`. The path carries the whole identity, so two
    /// tags of one exposure serve side by side and clients can tell them
    /// apart.
    pub fn endpoint_path(&self) -> String {
        format!("/{}/{}/mcp", self.name, self.tag)
    }
}

/// Title and instructions advertised through `server/discover`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BundleServer {
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
}

/// One pinned contract slot: the contract bytes the exposure was validated
/// against, and the slot a launcher binds a provider to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BundleContractPin {
    pub name: String,
    pub tag: String,
    pub sha256: String,
    pub link_id: String,
}

/// One exposed topic: an MCP resource serving the latest policy-approved
/// snapshot.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResourceEntry {
    pub name: String,
    pub uri: String,
    pub description: String,
    /// The logical target (contract slot `link_id`) serving this resource.
    pub target: String,
    /// The contract topic the resource snapshots.
    pub member: String,
    pub policies: ResourcePolicies,
    /// Derived JSON Schema of the snapshot content.
    pub schema: Value,
}

/// The operational policies a resource read applies.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResourcePolicies {
    pub freshness: FreshnessPolicy,
    pub update: UpdatePolicy,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub representation: Option<ImageRepresentation>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_result_bytes: Option<NonZeroU64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on_oversize: Option<OversizePolicy>,
}

/// One exposed service: an MCP tool completing within a single request.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolEntry {
    pub name: String,
    pub description: String,
    pub target: String,
    pub member: String,
    pub operation: ServiceOperation,
    pub deadline_ms: NonZeroU64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_result_bytes: Option<NonZeroU64>,
    /// Derived JSON Schema of the tool input, with any `restrict` bounds
    /// reflected as `minimum`/`maximum`.
    pub input_schema: Value,
    /// Derived JSON Schema of the structured tool output.
    pub output_schema: Value,
}

/// One exposed action: an MCP tool backed by the MCP Tasks extension.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskEntry {
    pub name: String,
    pub description: String,
    pub target: String,
    pub member: String,
    pub operation: ActionOperation,
    pub safety_sensitive: bool,
    pub confirmation_required: bool,
    pub deadline_ms: NonZeroU64,
    /// Derived JSON Schema of the goal request the tool call carries.
    pub input_schema: Value,
    /// Derived JSON Schema of the structured result completing the task.
    pub output_schema: Value,
    /// Derived JSON Schema of feedback messages, for actions that declare a
    /// feedback topic.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub feedback_schema: Option<Value>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn minimal_bundle_json(bundle_format: u32, schema_mapping_version: u32) -> String {
        format!(
            r#"{{
  "bundle_format": {bundle_format},
  "schema_mapping_version": {schema_mapping_version},
  "exposure": {{ "name": "camera", "tag": "v1" }},
  "server": {{ "title": "Camera" }},
  "contracts": [
    {{ "name": "rgb_camera", "tag": "v1", "sha256": "aa", "link_id": "front_camera" }}
  ],
  "resources": [
    {{
      "name": "front_camera.status",
      "uri": "peppy://resource/front_camera.status",
      "description": "Latest camera status.",
      "target": "front_camera",
      "member": "camera_status",
      "policies": {{
        "freshness": {{ "max_age_ms": 2000 }},
        "update": {{ "max_hz": 2.0 }}
      }},
      "schema": {{ "type": "object" }}
    }}
  ],
  "tools": [],
  "tasks": []
}}"#
        )
    }

    #[test]
    fn parses_a_published_bundle_and_round_trips_it() {
        let bundle = ExposureBundle::from_json_str(&minimal_bundle_json(1, 1)).expect("parses");
        assert_eq!(bundle.exposure.name, "camera");
        assert_eq!(bundle.contracts[0].link_id, "front_camera");
        assert_eq!(bundle.resources[0].policies.update.max_hz.get(), 2.0);

        let serialized = bundle.to_json_string();
        assert!(
            serialized.ends_with("}\n"),
            "canonical form ends with a newline"
        );
        let reparsed = ExposureBundle::from_json_str(&serialized).expect("round trips");
        assert_eq!(reparsed, bundle);
    }

    #[test]
    fn the_endpoint_path_carries_the_whole_identity() {
        let identity = BundleIdentity {
            name: "arm_control".to_string(),
            tag: "v2".to_string(),
        };
        assert_eq!(identity.endpoint_path(), "/arm_control/v2/mcp");
    }

    #[test]
    fn refuses_an_unknown_bundle_format() {
        let error = ExposureBundle::from_json_str(&minimal_bundle_json(2, 1))
            .expect_err("format 2 should be refused");
        assert!(
            error.contains("bundle format 2 is not supported"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn refuses_an_unknown_schema_mapping_version() {
        let error = ExposureBundle::from_json_str(&minimal_bundle_json(1, 2))
            .expect_err("mapping version 2 should be refused");
        assert!(
            error.contains("schema mapping version 2 is not supported"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn refuses_unknown_fields_in_a_supported_format() {
        let content = minimal_bundle_json(1, 1)
            .replace("\"tools\": [],", "\"tools\": [],\n  \"unexpected\": true,");
        let error =
            ExposureBundle::from_json_str(&content).expect_err("unknown field should be refused");
        assert!(
            error.contains("invalid exposure bundle"),
            "unexpected error: {error}"
        );
    }
}
