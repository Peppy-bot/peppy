//! The serializable shape of a versioned exposure bundle.

use daemon_config::mcp_exposure::{
    ActionOperation, FreshnessPolicy, ImageRepresentation, OversizePolicy, ServiceOperation,
    UpdatePolicy,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::num::NonZeroU64;

/// Version of the bundle shape in this module.
pub const EXPOSURE_BUNDLE_FORMAT: u32 = 1;

/// The product of validating one exposure document against its pinned
/// contracts: the public catalog (stable names, prose, policies, derived
/// JSON Schemas) plus the identity and contract slots of the generated MCP
/// server node. The bundle is committed next to its exposure document and
/// regenerated on demand, so a drift check can refuse a catalog that no
/// longer matches the document it was published from.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExposureBundle {
    pub bundle_format: u32,
    pub schema_mapping_version: u32,
    pub exposure: BundleIdentity,
    pub server: BundleServer,
    pub node: BundleNode,
    pub resources: Vec<ResourceEntry>,
    pub tools: Vec<ToolEntry>,
    pub tasks: Vec<TaskEntry>,
}

impl ExposureBundle {
    /// Canonical serialized form: pretty JSON with a trailing newline. These
    /// are the bytes committed to a hub repository and the bytes the drift
    /// check compares against.
    pub fn to_json_string(&self) -> String {
        let pretty = serde_json::to_string_pretty(self).expect("bundle serializes");
        format!("{pretty}\n")
    }
}

/// Identity of the exposure document the bundle was generated from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BundleIdentity {
    pub name: String,
    pub tag: String,
}

/// Server identity advertised through `server/discover`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BundleServer {
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
}

/// The generated MCP server node: its identity and the contract slot each
/// logical target becomes. The manifest generated from this declares one
/// `depends_on.contracts` entry per pin, with the pin's `link_id` as the
/// slot the launcher fills.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BundleNode {
    pub name: String,
    pub tag: String,
    pub contracts: Vec<BundleContractPin>,
}

/// One pinned contract slot of the generated node.
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
