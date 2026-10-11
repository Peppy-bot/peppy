//! The serializable shape of a versioned exposure bundle.

use crate::policy::{
    ActionOperation, ContentPolicies, FreshnessPolicy, GoalBound, ImageRepresentation,
    OversizePolicy, ServiceOperation, UpdatePolicy,
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

/// The product of validating one exposure document against its contracts
/// and daemon interfaces: the public catalog (stable names, prose, policies,
/// derived JSON Schemas) plus the identity the endpoint advertises, the
/// contract slots its contract targets become and the interfaces its daemon
/// targets name. A server derives it when it starts, and the catalog command
/// prints it on demand; it is never an artifact of its own.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "RawExposureBundle", into = "RawExposureBundle")]
pub struct ExposureBundle {
    pub bundle_format: u32,
    pub schema_mapping_version: u32,
    pub exposure: BundleIdentity,
    pub server: BundleServer,
    pub surface: BundleSurface,
    pub resources: Vec<ResourceEntry>,
    pub tools: Vec<ToolEntry>,
    pub tasks: Vec<TaskEntry>,
    pub pictures: Vec<PictureEntry>,
    /// The record of the state-changing calls the endpoint takes, when the
    /// exposure declares one.
    pub call_record: Option<CallRecordEntry>,
}

/// The tool that answers the last calls of every tool of the bundle that is
/// not read-only, newest first. It reaches no provider.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CallRecordEntry {
    pub name: String,
    pub description: String,
    /// How many calls the endpoint keeps.
    pub keep: u32,
}

/// What a bundle serves: the contract slots and daemon targets of a fixed
/// surface, or the robots of a stack, each filling every slot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BundleSurface {
    /// Each slot is filled by the instance a launcher binds to it, each
    /// daemon target by the daemon that started the server, and a tool call
    /// carries the member's own request alone.
    Fixed {
        contracts: Vec<BundleContractPin>,
        daemon_targets: Vec<BundleDaemonTarget>,
    },
    /// Every slot is a `zero_or_more` slot the stack's robots fill, and
    /// every tool takes the robot's name.
    PerRobot {
        robots: RobotCatalog,
        contracts: Vec<RobotContractPin>,
    },
}

impl BundleSurface {
    /// The contract slot of every contract target, in catalog order. A
    /// daemon target has no slot and is not in it.
    pub fn contracts(&self) -> Vec<&BundleContractPin> {
        match self {
            Self::Fixed { contracts, .. } => contracts.iter().collect(),
            Self::PerRobot { contracts, .. } => contracts.iter().map(|pin| &pin.pin).collect(),
        }
    }

    /// Every daemon target, in catalog order: empty on a per-robot surface.
    pub fn daemon_targets(&self) -> &[BundleDaemonTarget] {
        match self {
            Self::Fixed { daemon_targets, .. } => daemon_targets,
            Self::PerRobot { .. } => &[],
        }
    }
}

/// Wire shape of [`ExposureBundle`]. Deserialization funnels through
/// `TryFrom<RawExposureBundle>` so the `argument` a slot takes reaches the
/// parsed bundle on a per-robot surface alone.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawExposureBundle {
    bundle_format: u32,
    schema_mapping_version: u32,
    exposure: BundleIdentity,
    server: BundleServer,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    robots: Option<RobotCatalog>,
    /// The contract slot each contract target becomes: one slot per pin,
    /// with the pin's `link_id` as the slot the launcher fills.
    contracts: Vec<RawBundleContractPin>,
    /// The daemon interface each daemon target names, which a fixed surface
    /// alone takes.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    daemon_targets: Vec<BundleDaemonTarget>,
    resources: Vec<ResourceEntry>,
    tools: Vec<ToolEntry>,
    tasks: Vec<TaskEntry>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pictures: Vec<PictureEntry>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    call_record: Option<CallRecordEntry>,
}

impl TryFrom<RawExposureBundle> for ExposureBundle {
    type Error = String;

    fn try_from(raw: RawExposureBundle) -> Result<Self, String> {
        let surface = match raw.robots {
            Some(_) if !raw.daemon_targets.is_empty() => {
                return Err(format!(
                    "daemon target `{}` is on a per-robot surface; a daemon target belongs to a \
                     bundle without `robots`",
                    raw.daemon_targets[0].target
                ));
            }
            Some(robots) => BundleSurface::PerRobot {
                robots,
                contracts: raw
                    .contracts
                    .into_iter()
                    .map(RobotContractPin::from)
                    .collect(),
            },
            None => {
                let mut contracts = Vec::with_capacity(raw.contracts.len());
                for pin in raw.contracts {
                    if pin.argument.is_some() {
                        return Err(format!(
                            "contract slot `{}` declares `argument`, which names the member a \
                             call addresses on a per-robot surface; a bundle taking one declares \
                             `robots`",
                            pin.link_id
                        ));
                    }
                    contracts.push(RobotContractPin::from(pin).pin);
                }
                BundleSurface::Fixed {
                    contracts,
                    daemon_targets: raw.daemon_targets,
                }
            }
        };
        Ok(Self {
            bundle_format: raw.bundle_format,
            schema_mapping_version: raw.schema_mapping_version,
            exposure: raw.exposure,
            server: raw.server,
            surface,
            resources: raw.resources,
            tools: raw.tools,
            tasks: raw.tasks,
            pictures: raw.pictures,
            call_record: raw.call_record,
        })
    }
}

impl From<ExposureBundle> for RawExposureBundle {
    fn from(bundle: ExposureBundle) -> Self {
        let (robots, contracts, daemon_targets) = match bundle.surface {
            BundleSurface::Fixed {
                contracts,
                daemon_targets,
            } => (
                None,
                contracts
                    .into_iter()
                    .map(|pin| RawBundleContractPin::new(pin, None))
                    .collect(),
                daemon_targets,
            ),
            BundleSurface::PerRobot { robots, contracts } => (
                Some(robots),
                contracts
                    .into_iter()
                    .map(|pin| RawBundleContractPin::new(pin.pin, pin.argument))
                    .collect(),
                Vec::new(),
            ),
        };
        Self {
            bundle_format: bundle.bundle_format,
            schema_mapping_version: bundle.schema_mapping_version,
            exposure: bundle.exposure,
            server: bundle.server,
            robots,
            contracts,
            daemon_targets,
            resources: bundle.resources,
            tools: bundle.tools,
            tasks: bundle.tasks,
            pictures: bundle.pictures,
            call_record: bundle.call_record,
        }
    }
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
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BundleContractPin {
    pub name: String,
    pub tag: String,
    pub sha256: String,
    pub link_id: String,
}

/// One daemon target: the target's name, and the daemon interface it draws
/// its members from. The daemon that started the server serves it, so it
/// takes no slot and no pin.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BundleDaemonTarget {
    pub target: String,
    pub name: String,
    pub tag: String,
}

/// One pinned contract slot of a per-robot surface, and how a robot fills it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RobotContractPin {
    pub pin: BundleContractPin,
    /// The argument naming the member a call addresses on a target a robot
    /// fills any number of times.
    pub argument: Option<String>,
}

/// Wire shape of one contract slot: a [`BundleContractPin`] plus the
/// `argument` a per-robot surface's slot takes.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawBundleContractPin {
    name: String,
    tag: String,
    sha256: String,
    link_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    argument: Option<String>,
}

impl RawBundleContractPin {
    fn new(pin: BundleContractPin, argument: Option<String>) -> Self {
        Self {
            name: pin.name,
            tag: pin.tag,
            sha256: pin.sha256,
            link_id: pin.link_id,
            argument,
        }
    }
}

impl From<RawBundleContractPin> for RobotContractPin {
    fn from(raw: RawBundleContractPin) -> Self {
        let RawBundleContractPin {
            name,
            tag,
            sha256,
            link_id,
            argument,
        } = raw;
        Self {
            pin: BundleContractPin {
                name,
                tag,
                sha256,
                link_id,
            },
            argument,
        }
    }
}

/// The per-robot surface of a bundle: the listing tool and what it reports.
/// Every tool takes the robot's name under
/// [`ROBOT_ARGUMENT`](crate::document::ROBOT_ARGUMENT).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RobotCatalog {
    pub list: ListEntry,
    /// What the listing reports of each robot, in document order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub describe: Vec<DescribeEntry>,
}

/// The listing tool: its public name and prose. Its input takes nothing and
/// its output is the runtime's, one entry per robot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ListEntry {
    pub name: String,
    pub description: String,
}

/// One field of a robot's listing entry: the response of the tool the
/// listing calls for it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DescribeEntry {
    /// The field of the listing entry.
    pub key: String,
    /// The target the value is read through.
    pub target: String,
    /// The tool published for the service member, called when the robot is
    /// listed.
    pub tool: String,
}

/// One exposed topic: an MCP resource serving the latest policy-approved
/// snapshot. A snapshot is a document, the topic's message as JSON, and
/// under a representation also a blob: the frame in the representation's
/// codec, which the document then leaves out.
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
    /// Derived JSON Schema of the snapshot's document.
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

impl ResourcePolicies {
    /// The MIME type of the blob a snapshot carries, for a resource with a
    /// representation.
    pub fn blob_mime_type(&self) -> Option<&'static str> {
        self.representation
            .as_ref()
            .map(|representation| representation.image.mime_type())
    }

    /// The policies that shape the snapshot a read serves.
    pub fn content(&self) -> ContentPolicies<'_> {
        ContentPolicies {
            representation: self.representation.as_ref(),
            max_result_bytes: self.max_result_bytes,
            on_oversize: self.on_oversize,
        }
    }
}

/// One exposed service: an MCP tool completing within a single request.
/// Under a representation the tool answers with a picture: the frame of the
/// response as an image block, the rest of the response as the document,
/// which the output schema describes.
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
    pub representation: Option<ImageRepresentation>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_result_bytes: Option<NonZeroU64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on_oversize: Option<OversizePolicy>,
    /// Derived JSON Schema of the tool input, with any `restrict` bounds
    /// reflected as `minimum`/`maximum`.
    pub input_schema: Value,
    /// Derived JSON Schema of the structured tool output.
    pub output_schema: Value,
}

impl ToolEntry {
    /// The policies that shape the answer a call receives.
    pub fn content(&self) -> ContentPolicies<'_> {
        ContentPolicies {
            representation: self.representation.as_ref(),
            max_result_bytes: self.max_result_bytes,
            on_oversize: self.on_oversize,
        }
    }
}

/// One picture tool: an MCP tool answering with the latest snapshot of a
/// resource with a `jpeg` representation, the blob as an image and the
/// document beside it. It completes within the call and reaches no
/// provider.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PictureEntry {
    pub name: String,
    pub description: String,
    pub target: String,
    /// The contract topic whose snapshot the tool answers with.
    pub member: String,
    /// The name of the resource entry whose snapshot the tool answers with.
    pub resource: String,
    /// JSON Schema of the tool input: the routing arguments of a per-robot
    /// surface, and nothing on a fixed one.
    pub input_schema: Value,
    /// Derived JSON Schema of the structured tool output, the snapshot's
    /// document.
    pub output_schema: Value,
}

/// One exposed action: an MCP tool backed by the MCP Tasks extension.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "RawTaskEntry", into = "RawTaskEntry")]
pub struct TaskEntry {
    pub name: String,
    pub description: String,
    pub target: String,
    pub member: String,
    pub operation: ActionOperation,
    pub safety_sensitive: bool,
    /// Never set together with a progress bound: parsing refuses the pair.
    pub confirmation_required: bool,
    /// What bounds the goal, written as `deadline_ms` or
    /// `progress_timeout_ms`.
    pub bound: GoalBound,
    /// Derived JSON Schema of the goal request the tool call carries.
    pub input_schema: Value,
    /// Derived JSON Schema of the structured result completing the task.
    pub output_schema: Value,
    /// Derived JSON Schema of feedback messages, for actions that declare a
    /// feedback topic.
    pub feedback_schema: Option<Value>,
}

/// Wire shape of [`TaskEntry`]: the goal's bound is one of two fields, which
/// parsing turns into a [`GoalBound`], refusing a confirmation gate in front
/// of a progress-bound goal.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawTaskEntry {
    name: String,
    description: String,
    target: String,
    member: String,
    operation: ActionOperation,
    safety_sensitive: bool,
    confirmation_required: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    deadline_ms: Option<NonZeroU64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    progress_timeout_ms: Option<NonZeroU64>,
    input_schema: Value,
    output_schema: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    feedback_schema: Option<Value>,
}

impl TryFrom<RawTaskEntry> for TaskEntry {
    type Error = String;

    fn try_from(raw: RawTaskEntry) -> Result<Self, String> {
        let owner = format!("task `{}`", raw.name);
        let bound = GoalBound::from_fields(&owner, raw.deadline_ms, raw.progress_timeout_ms)?;
        bound.check_confirmation(&owner, raw.confirmation_required)?;
        Ok(Self {
            name: raw.name,
            description: raw.description,
            target: raw.target,
            member: raw.member,
            operation: raw.operation,
            safety_sensitive: raw.safety_sensitive,
            confirmation_required: raw.confirmation_required,
            bound,
            input_schema: raw.input_schema,
            output_schema: raw.output_schema,
            feedback_schema: raw.feedback_schema,
        })
    }
}

impl From<TaskEntry> for RawTaskEntry {
    fn from(entry: TaskEntry) -> Self {
        let (deadline_ms, progress_timeout_ms) = entry.bound.to_fields();
        Self {
            name: entry.name,
            description: entry.description,
            target: entry.target,
            member: entry.member,
            operation: entry.operation,
            safety_sensitive: entry.safety_sensitive,
            confirmation_required: entry.confirmation_required,
            deadline_ms,
            progress_timeout_ms,
            input_schema: entry.input_schema,
            output_schema: entry.output_schema,
            feedback_schema: entry.feedback_schema,
        }
    }
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
        assert_eq!(bundle.surface.contracts()[0].link_id, "front_camera");
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
    fn refuses_a_routing_argument_without_a_robot_surface() {
        let content = minimal_bundle_json(1, 1).replace(
            r#""link_id": "front_camera" }"#,
            r#""link_id": "front_camera", "argument": "camera" }"#,
        );
        let error = ExposureBundle::from_json_str(&content)
            .expect_err("a fixed surface's slot takes no argument");
        assert!(
            error.contains("contract slot `front_camera` declares `argument`"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn a_per_robot_bundle_carries_the_argument_of_each_slot() {
        let content = minimal_bundle_json(1, 1)
            .replace(
                r#""link_id": "front_camera" }"#,
                r#""link_id": "front_camera", "argument": "camera" }"#,
            )
            .replace(
                r#""server": { "title": "Camera" },"#,
                r#""server": { "title": "Camera" },
  "robots": { "list": { "name": "robot.list", "description": "The robots." } },"#,
            );
        let bundle = ExposureBundle::from_json_str(&content).expect("parses");
        let BundleSurface::PerRobot { robots, contracts } = &bundle.surface else {
            panic!("a per-robot surface");
        };
        assert_eq!(robots.list.name, "robot.list");
        assert_eq!(contracts[0].argument.as_deref(), Some("camera"));
        let reparsed = ExposureBundle::from_json_str(&bundle.to_json_string())
            .expect("round trips through its wire shape");
        assert_eq!(reparsed, bundle);
    }

    /// `minimal_bundle_json` with the daemon target `stack` beside its slot.
    fn bundle_with_daemon_target() -> String {
        minimal_bundle_json(1, 1).replace(
            r#""resources": ["#,
            r#""daemon_targets": [
    { "target": "stack", "name": "stack_copies", "tag": "v1" }
  ],
  "resources": ["#,
        )
    }

    #[test]
    fn a_fixed_bundle_marks_its_daemon_targets_apart_from_its_slots() {
        let bundle = ExposureBundle::from_json_str(&bundle_with_daemon_target()).expect("parses");
        assert_eq!(
            bundle.surface.daemon_targets(),
            [BundleDaemonTarget {
                target: "stack".to_string(),
                name: "stack_copies".to_string(),
                tag: "v1".to_string(),
            }]
        );
        let slots: Vec<&str> = bundle
            .surface
            .contracts()
            .iter()
            .map(|pin| pin.link_id.as_str())
            .collect();
        assert_eq!(slots, ["front_camera"], "a daemon target is no slot");
        let reparsed = ExposureBundle::from_json_str(&bundle.to_json_string())
            .expect("round trips through its wire shape");
        assert_eq!(reparsed, bundle);

        let without = ExposureBundle::from_json_str(&minimal_bundle_json(1, 1)).expect("parses");
        assert!(without.surface.daemon_targets().is_empty());
        assert!(
            !without.to_json_string().contains("daemon_targets"),
            "a bundle with no daemon target writes no `daemon_targets`"
        );
    }

    #[test]
    fn refuses_a_daemon_target_on_a_per_robot_bundle() {
        let content = bundle_with_daemon_target().replace(
            r#""server": { "title": "Camera" },"#,
            r#""server": { "title": "Camera" },
  "robots": { "list": { "name": "robot.list", "description": "The robots." } },"#,
        );
        let error = ExposureBundle::from_json_str(&content)
            .expect_err("a per-robot bundle takes no daemon target");
        assert!(
            error.contains("daemon target `stack` is on a per-robot surface"),
            "{error}"
        );
    }

    /// `minimal_bundle_json` with one task, bounded by the `bound` fields.
    fn bundle_with_task(bound: &str) -> String {
        minimal_bundle_json(1, 1).replace(
            r#""tasks": []"#,
            &format!(
                r#""tasks": [
    {{
      "name": "recorder.record_episode",
      "description": "Record one episode.",
      "target": "front_camera",
      "member": "record_episode",
      "operation": "long_running",
      "safety_sensitive": false,
      "confirmation_required": false,
      {bound}
      "input_schema": {{ "type": "object" }},
      "output_schema": {{ "type": "object" }}
    }}
  ]"#
            ),
        )
    }

    #[test]
    fn a_task_is_bounded_by_exactly_one_of_its_two_bound_fields() {
        let progress =
            ExposureBundle::from_json_str(&bundle_with_task(r#""progress_timeout_ms": 60000,"#))
                .expect("a progress-bound task parses");
        let serialized = progress.to_json_string();
        assert!(
            serialized.contains(r#""progress_timeout_ms": 60000"#)
                && !serialized.contains("deadline_ms"),
            "{serialized}"
        );
        let reparsed = ExposureBundle::from_json_str(&serialized).expect("round trips");
        assert_eq!(reparsed, progress);
        assert_eq!(
            progress.tasks[0].bound,
            GoalBound::Progress {
                window_ms: NonZeroU64::new(60000).expect("nonzero")
            }
        );

        let both = ExposureBundle::from_json_str(&bundle_with_task(
            r#""deadline_ms": 1000, "progress_timeout_ms": 60000,"#,
        ))
        .expect_err("two bounds are refused");
        assert!(
            both.contains(
                "task `recorder.record_episode` declares both `deadline_ms` and \
                 `progress_timeout_ms`"
            ),
            "{both}"
        );
        let neither = ExposureBundle::from_json_str(&bundle_with_task(""))
            .expect_err("a task without a bound is refused");
        assert!(
            neither.contains(
                "task `recorder.record_episode` declares neither `deadline_ms` nor \
                 `progress_timeout_ms`"
            ),
            "{neither}"
        );
    }

    #[test]
    fn a_progress_bound_task_cannot_ask_for_confirmation() {
        let content = bundle_with_task(r#""progress_timeout_ms": 60000,"#).replace(
            r#""confirmation_required": false,"#,
            r#""confirmation_required": true,"#,
        );
        let error = ExposureBundle::from_json_str(&content)
            .expect_err("a confirmation gate in front of a progress-bound goal is refused");
        assert!(
            error.contains(
                "task `recorder.record_episode`: `confirmation_required` cannot go with \
                 `progress_timeout_ms`"
            ),
            "{error}"
        );
    }

    #[test]
    fn a_bundle_carries_its_picture_tools_and_writes_none_as_no_field() {
        let without = ExposureBundle::from_json_str(&minimal_bundle_json(1, 1)).expect("parses");
        assert!(without.pictures.is_empty());
        assert!(
            !without.to_json_string().contains("\"pictures\""),
            "a bundle with no picture tool writes no `pictures`"
        );

        let content = minimal_bundle_json(1, 1).replace(
            r#""tasks": []"#,
            r#""tasks": [],
  "pictures": [
    {
      "name": "front_camera.look",
      "description": "The latest frame, as a picture.",
      "target": "front_camera",
      "member": "video_stream",
      "resource": "front_camera.latest_frame",
      "input_schema": { "type": "object", "properties": {}, "additionalProperties": false },
      "output_schema": { "type": "object" }
    }
  ]"#,
        );
        let bundle = ExposureBundle::from_json_str(&content).expect("parses");
        assert_eq!(bundle.pictures.len(), 1);
        assert_eq!(bundle.pictures[0].name, "front_camera.look");
        assert_eq!(bundle.pictures[0].resource, "front_camera.latest_frame");
        let reparsed = ExposureBundle::from_json_str(&bundle.to_json_string())
            .expect("round trips through its wire shape");
        assert_eq!(reparsed, bundle);
    }

    #[test]
    fn a_resource_with_a_representation_names_the_mime_type_of_its_blob() {
        let mut bundle = ExposureBundle::from_json_str(&minimal_bundle_json(1, 1)).expect("parses");
        let policies = &mut bundle.resources[0].policies;
        assert_eq!(policies.blob_mime_type(), None);
        policies.representation = Some(
            serde_json::from_str(
                r#"{
                    "image": "png16",
                    "fields": { "data": "frame", "encoding": "encoding", "width": "w", "height": "h" }
                }"#,
            )
            .expect("a representation"),
        );
        assert_eq!(policies.blob_mime_type(), Some("image/png"));
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
