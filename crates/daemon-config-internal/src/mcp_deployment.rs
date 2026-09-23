//! The built-in MCP server as a deployment.
//!
//! A launcher lists exposures; the daemon runs one `peppy mcp serve` process
//! for the list. Everything that process is, from the planner's point of
//! view, derives from the exposure documents and the contracts they pin:
//! its node identity, the manifest whose contract slots the launcher's
//! `links` fill, the catalogs it serves. This module holds those
//! derivations, so the coordinator that plans a launch, the daemon that
//! registers the server and the server itself compute one answer from one
//! set of pinned bytes.

use crate::internal::contract::{PeppyContract, PeppyContractParser};
use crate::internal::launcher::MemberAddressing;
use crate::internal::repository::{ManifestFingerprint, PinKind, PinnedItem};
use crate::internal::source::ExposureRef;
use config::node::{NodeConfig, NodeConfigParser};
use config::runtime::Name;
use peppy_mcp_catalog::{
    ExposureSurface, McpExposure, ResolvedContract, ValidatedExposure, build_exposure_bundle,
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

/// The tag of every built-in node: the identity's name carries the
/// exposure set, so the tag only says what kind of node this is.
pub const BUILT_IN_TAG: &str = "builtin";

/// The one argument the built-in server takes.
pub const PORT_PARAMETER: &str = "port";

/// The port the server prefers when a launcher gives none.
pub const DEFAULT_PORT: u16 = 8900;

/// How the synthesized manifest says the server runs: the installed `peppy`
/// binary's `mcp serve` subcommand. The daemon resolves the binary itself
/// and runs the subcommand with the arguments after it.
pub const RUN_COMMAND: [&str; 3] = ["peppy", "mcp", "serve"];

/// The environment variable through which the daemon hands `peppy mcp
/// serve` the path of its [`McpServeSpec`].
pub const SPEC_ENV_VAR: &str = "PEPPY_MCP_SERVE_SPEC";

/// The node name of the server that serves `exposures`, derived from the
/// sorted list so a relaunch, a peer and a reader of `peppy stack list` all
/// name the same deployment: `mcp` followed by `_<name>_<tag>` per exposure
/// in name-then-tag order. Every built-in node is tagged [`BUILT_IN_TAG`].
pub fn built_in_identity(exposures: &[ExposureRef]) -> Name {
    let mut sorted: Vec<&ExposureRef> = exposures.iter().collect();
    sorted.sort();
    sorted.dedup();
    let mut name = String::from("mcp");
    for reference in sorted {
        name.push('_');
        name.push_str(&reference.name);
        name.push('_');
        name.push_str(&reference.tag);
    }
    Name::new(name).expect("exposure names and tags are made of name characters")
}

/// The label the synthesized manifest declares an exposure's endpoint under,
/// and the one the server announces it with: `<name>_<tag>`, the tokens the
/// deployment identity is built from, with the tag normalized the way
/// generated code spells it (`-` becomes `_`).
pub fn endpoint_label(name: &str, tag: &str) -> String {
    format!("{name}_{}", config::consts::normalize_tag(tag))
}

/// A parsed document with the pin that names its bytes.
#[derive(Debug, Clone)]
pub struct Pinned<T> {
    pub pin: PinnedItem,
    pub document: T,
}

/// An exposure document with the pin that names its bytes.
pub type PinnedExposure = Pinned<McpExposure>;

/// A contract document with the pin that names its bytes.
pub type PinnedContract = Pinned<PeppyContract>;

impl PinnedExposure {
    pub fn reference(&self) -> ExposureRef {
        ExposureRef::from(&self.pin)
    }
}

impl PinnedContract {
    /// The contract as the catalog validates an exposure against it: its
    /// pinned identity and fingerprint beside its members.
    pub fn resolved(&self) -> ResolvedContract<'_> {
        ResolvedContract {
            name: self.pin.name.as_str(),
            tag: self.pin.tag.as_str(),
            sha256: &self.pin.sha256,
            topics: &self.document.interfaces.topics,
            services: &self.document.interfaces.services,
            actions: &self.document.interfaces.actions,
        }
    }
}

/// Two exposures binding one target name to different contracts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlotConflict {
    pub target: String,
    pub first_exposure: String,
    pub first_contract: String,
    pub second_exposure: String,
    pub second_contract: String,
}

/// One exposure's validation verdict against the pinned contracts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExposureViolations {
    pub exposure: String,
    pub violations: Vec<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum McpDeploymentError {
    #[error("a built-in MCP deployment needs at least one exposure")]
    NoExposures,
    #[error("exposure `{exposure}` is listed twice")]
    DuplicateExposure { exposure: String },
    #[error(
        "exposure `{exposure}` references contract `{contract}`, which the deployment does not pin"
    )]
    ContractNotPinned { exposure: String, contract: String },
    #[error(
        "exposure `{exposure}` pins contract `{contract}` at sha256 `{author}`, but the \
         deployment pins it at `{pinned}`"
    )]
    ContractPinMismatch {
        exposure: String,
        contract: String,
        author: ManifestFingerprint,
        pinned: ManifestFingerprint,
    },
    #[error(
        "target `{}` is bound to contract `{}` by exposure `{}` and to contract `{}` by exposure \
         `{}`; two exposures sharing a target name must pin the same contract",
        .0.target, .0.first_contract, .0.first_exposure, .0.second_contract, .0.second_exposure
    )]
    SlotConflict(SlotConflict),
    #[error(transparent)]
    ArgumentConflict(Box<ArgumentConflict>),
    #[error(
        "exposure `{per_robot}` is a per-robot surface and exposure `{fixed}` is not; one \
         deployment serves per-robot surfaces alone or fixed surfaces alone, so list \
         `{per_robot}` in a deployment of its own"
    )]
    MixedSurfaces { per_robot: String, fixed: String },
    #[error("{}", format_violations(.0))]
    Invalid(Vec<ExposureViolations>),
    #[error("the synthesized manifest for `{identity}` is not a valid node manifest: {reason}")]
    Manifest { identity: String, reason: String },
}

/// Two exposures of one deployment name a shared target's member
/// differently: by an argument, or once per robot with none.
#[derive(Debug, thiserror::Error)]
#[error(
    "target `{target}` takes {} in exposure `{first_exposure}` and {} in exposure \
     `{second_exposure}`; two exposures sharing a target name must name its member the same way",
    argument_phrase(.first_argument),
    argument_phrase(.second_argument)
)]
pub struct ArgumentConflict {
    pub target: String,
    pub first_exposure: String,
    pub first_argument: Option<String>,
    pub second_exposure: String,
    pub second_argument: Option<String>,
}

fn argument_phrase(argument: &Option<String>) -> String {
    match argument {
        Some(argument) => format!("argument `{argument}`"),
        None => "no argument".to_owned(),
    }
}

fn format_violations(reports: &[ExposureViolations]) -> String {
    reports
        .iter()
        .map(|report| {
            format!(
                "exposure `{}` does not validate against its contracts:\n{}",
                report.exposure,
                crate::error::format_bulleted(&report.violations)
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The planned deployment: its identity, the manifest the planner binds
/// and pins, and every exposure validated against the pinned contracts, in
/// identity order: the catalog each advertises, and behind each entry the
/// contract member the server's bridges bind to.
#[derive(Debug, Clone)]
pub struct McpDeploymentPlan {
    pub name: Name,
    pub tag: String,
    pub config: NodeConfig,
    pub exposures: Vec<ValidatedExposure>,
    /// How the server reads the sets its contract slots hold, which the
    /// binding validator holds the launcher to.
    pub addressing: MemberAddressing,
}

/// One contract slot of the synthesized manifest and who filled it.
struct Slot<'a> {
    contract: &'a PinnedItem,
    exposure: String,
    /// On a per-robot surface, the argument naming the member a call
    /// addresses on the target.
    argument: Option<String>,
}

/// Derives the built-in server's deployment from its pinned documents.
///
/// Every exposure is validated against the contracts it references, all
/// violations reported at once. The manifest declares one contract slot per
/// target name: two exposures naming the same target with the same contract
/// share the slot, the same target name with a different contract is a
/// [`McpDeploymentError::SlotConflict`]. An exposure's own `sha256` pin, when
/// present, must agree with the deployment's contract pin; absent, the
/// deployment's pin fixes the bytes.
pub fn plan_deployment(
    exposures: &[PinnedExposure],
    contracts: &[PinnedContract],
) -> Result<McpDeploymentPlan, McpDeploymentError> {
    if exposures.is_empty() {
        return Err(McpDeploymentError::NoExposures);
    }
    let mut ordered: Vec<&PinnedExposure> = exposures.iter().collect();
    ordered.sort_by_key(|exposure| exposure.reference());
    for (index, exposure) in ordered.iter().enumerate() {
        if ordered[..index]
            .iter()
            .any(|other| other.reference() == exposure.reference())
        {
            return Err(McpDeploymentError::DuplicateExposure {
                exposure: exposure.reference().to_string(),
            });
        }
    }
    let references: Vec<ExposureRef> = ordered.iter().map(|e| e.reference()).collect();
    let name = built_in_identity(&references);
    let tag = BUILT_IN_TAG.to_owned();

    // A deployment serves one kind of surface: every target of a per-robot
    // surface is a set slot the stack's robots fill, every target of a fixed
    // surface a scalar slot the launcher's `links` fill.
    let per_robot = ordered
        .iter()
        .find(|exposure| matches!(exposure.document.surface, ExposureSurface::PerRobot { .. }));
    let fixed = ordered
        .iter()
        .find(|exposure| matches!(exposure.document.surface, ExposureSurface::Fixed { .. }));
    if let (Some(per_robot), Some(fixed)) = (per_robot, fixed) {
        return Err(McpDeploymentError::MixedSurfaces {
            per_robot: per_robot.reference().to_string(),
            fixed: fixed.reference().to_string(),
        });
    }
    let per_robot = per_robot.is_some();

    let mut slots: BTreeMap<String, Slot> = BTreeMap::new();
    let mut topics: BTreeSet<(String, String)> = BTreeSet::new();
    let mut services: BTreeSet<(String, String)> = BTreeSet::new();
    let mut actions: BTreeSet<(String, String)> = BTreeSet::new();
    let mut validated = Vec::with_capacity(ordered.len());
    let mut invalid: Vec<ExposureViolations> = Vec::new();

    for exposure in &ordered {
        let label = exposure.reference().to_string();
        let mut resolved: Vec<ResolvedContract<'_>> = Vec::new();
        for (target, spec, spec_argument) in exposure.document.surface.targets() {
            let reference = &spec.contract;
            let contract_label = format!("{}:{}", reference.name.as_str(), reference.tag);
            let pinned = contracts
                .iter()
                .find(|contract| {
                    contract.pin.name == reference.name.as_str()
                        && contract.pin.tag == reference.tag.as_str()
                })
                .ok_or_else(|| McpDeploymentError::ContractNotPinned {
                    exposure: label.clone(),
                    contract: contract_label.clone(),
                })?;
            if let Some(author) = &reference.sha256
                && author != &pinned.pin.sha256
            {
                return Err(McpDeploymentError::ContractPinMismatch {
                    exposure: label.clone(),
                    contract: contract_label.clone(),
                    author: author.clone(),
                    pinned: pinned.pin.sha256.clone(),
                });
            }
            let argument = spec_argument.map(ToString::to_string);
            match slots.get(target) {
                Some(slot)
                    if slot.contract.name != reference.name.as_str()
                        || slot.contract.tag != reference.tag.as_str() =>
                {
                    return Err(McpDeploymentError::SlotConflict(SlotConflict {
                        target: target.clone(),
                        first_exposure: slot.exposure.clone(),
                        first_contract: format!("{}:{}", slot.contract.name, slot.contract.tag),
                        second_exposure: label.clone(),
                        second_contract: contract_label.clone(),
                    }));
                }
                Some(slot) if slot.argument != argument => {
                    return Err(McpDeploymentError::ArgumentConflict(Box::new(
                        ArgumentConflict {
                            target: target.clone(),
                            first_exposure: slot.exposure.clone(),
                            first_argument: slot.argument.clone(),
                            second_exposure: label.clone(),
                            second_argument: argument.clone(),
                        },
                    )));
                }
                Some(_) => {}
                None => {
                    slots.insert(
                        target.clone(),
                        Slot {
                            contract: &pinned.pin,
                            exposure: label.clone(),
                            argument,
                        },
                    );
                }
            }
            if !resolved.iter().any(|contract| {
                contract.name == reference.name.as_str() && contract.tag == reference.tag
            }) {
                resolved.push(pinned.resolved());
            }
        }
        match build_exposure_bundle(&exposure.document, &resolved) {
            Ok(checked) => {
                for resource in &checked.bundle.resources {
                    topics.insert((resource.target.clone(), resource.member.clone()));
                }
                for tool in &checked.bundle.tools {
                    services.insert((tool.target.clone(), tool.member.clone()));
                }
                for task in &checked.bundle.tasks {
                    actions.insert((task.target.clone(), task.member.clone()));
                }
                validated.push(checked);
            }
            Err(error) => invalid.push(ExposureViolations {
                exposure: label,
                violations: error.violations,
            }),
        }
    }
    if !invalid.is_empty() {
        return Err(McpDeploymentError::Invalid(invalid));
    }

    let consumed = |members: &BTreeSet<(String, String)>| -> Vec<serde_json::Value> {
        members
            .iter()
            .map(|(target, member)| serde_json::json!({ "link_id": target, "name": member }))
            .collect()
    };
    let mut interfaces = serde_json::Map::new();
    if !topics.is_empty() {
        interfaces.insert(
            "topics".to_owned(),
            serde_json::json!({ "consumes": consumed(&topics) }),
        );
    }
    if !services.is_empty() {
        interfaces.insert(
            "services".to_owned(),
            serde_json::json!({ "consumes": consumed(&services) }),
        );
    }
    if !actions.is_empty() {
        interfaces.insert(
            "actions".to_owned(),
            serde_json::json!({ "consumes": consumed(&actions) }),
        );
    }
    // The robots of a per-robot surface are the stack's copies, so the
    // server addresses every member by its copy, and a target declaring no
    // `argument` is one a robot fills once. `per_robot` gates both this and
    // the `zero_or_more` cardinality below, so the two agree.
    let addressing = if per_robot {
        MemberAddressing::ByCopy {
            one_per_copy: slots
                .iter()
                .filter(|(_, slot)| slot.argument.is_none())
                .map(|(target, _)| target.clone())
                .collect(),
        }
    } else {
        MemberAddressing::WholeSet
    };
    let contract_slots: Vec<serde_json::Value> = slots
        .iter()
        .map(|(target, slot)| {
            let mut declared = serde_json::json!({
                "name": slot.contract.name.as_str(),
                "tag": slot.contract.tag.as_str(),
                "link_id": target,
                "sha256": slot.contract.sha256.as_str(),
            });
            if per_robot {
                declared["cardinality"] = serde_json::json!("zero_or_more");
            }
            declared
        })
        .collect();
    // One endpoint per exposure, labelled from the same tokens the identity
    // is built from so two tags of one exposure name stay distinct; the
    // server announces each one under this label when it binds.
    let endpoints: serde_json::Map<String, serde_json::Value> = ordered
        .iter()
        .map(|exposure| {
            (
                endpoint_label(exposure.pin.name.as_str(), exposure.pin.tag.as_str()),
                serde_json::json!({
                    "kind": "mcp",
                    "description": exposure.document.server.title,
                }),
            )
        })
        .collect();
    let document = serde_json::json!({
        "peppy_schema": "node/v1",
        "manifest": {
            "name": name.as_str(),
            "tag": tag,
            "depends_on": { "contracts": contract_slots },
        },
        "interfaces": interfaces,
        "execution": {
            "language": "rust",
            "run_cmd": RUN_COMMAND,
            "parameters": {
                PORT_PARAMETER: { "$type": "u16", "$default": DEFAULT_PORT },
            },
            "endpoints": endpoints,
        },
    });
    // The manifest goes through the node document parser rather than being
    // assembled field by field, so every rule a hand-written manifest meets
    // (link id grammar, consumed member coherence, a stated way to run)
    // holds for this one.
    let config = NodeConfigParser::from_content(&document.to_string()).map_err(|error| {
        McpDeploymentError::Manifest {
            identity: format!("{}:{tag}", name.as_str()),
            reason: error.to_string(),
        }
    })?;

    Ok(McpDeploymentPlan {
        name,
        tag,
        config,
        exposures: validated,
        addressing,
    })
}

/// A document the daemon materialized from a pin, carried by its bytes so
/// the reader needs no cache: the fingerprint of `content` must equal the
/// pin's.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PinnedDocument {
    pub pin: PinnedItem,
    pub content: String,
}

impl PinnedDocument {
    pub fn new(pin: PinnedItem, content: String) -> Self {
        Self { pin, content }
    }

    fn verified_content(&self) -> Result<&str, String> {
        let actual = ManifestFingerprint::of_bytes(self.content.as_bytes());
        if actual != self.pin.sha256 {
            return Err(format!(
                "{} carries bytes fingerprinting to `{actual}`, not the pinned `{}`",
                self.pin.label(),
                self.pin.sha256
            ));
        }
        Ok(&self.content)
    }
}

/// What the daemon hands `peppy mcp serve`: the exposure documents to serve
/// and the contract documents they reference, each beside the pin that
/// names its bytes. The server derives its manifest and catalogs from this
/// file alone.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpServeSpec {
    pub exposures: Vec<PinnedDocument>,
    pub contracts: Vec<PinnedDocument>,
}

impl McpServeSpec {
    pub fn from_path(path: &Path) -> Result<Self, String> {
        crate::parsing::read_non_empty_file(path)
            .and_then(|content| crate::error::deserialize_json5_with_path(&content))
            .map_err(|error| format!("{} is not a serve spec: {error}", path.display()))
    }

    /// Writes the spec atomically, so a reader never sees a partial file.
    pub fn write(&self, path: &Path) -> Result<(), String> {
        let content = serde_json5::to_string(self)
            .map_err(|error| format!("cannot serialize the serve spec: {error}"))?;
        crate::internal::atomic_write::publish_atomic(path, |tmp| std::fs::write(tmp, &content))
            .map(|_| ())
            .map_err(|error| format!("cannot write {}: {error}", path.display()))
    }

    /// Verifies every document against its pin and parses it.
    pub fn resolve(&self) -> Result<(Vec<PinnedExposure>, Vec<PinnedContract>), String> {
        let exposures = parse_all(
            &self.exposures,
            PinKind::McpExposure,
            "an exposure",
            |content| crate::error::deserialize_json5_with_path(content).map_err(|e| e.to_string()),
        )?;
        let contracts = parse_all(
            &self.contracts,
            PinKind::Contract,
            "a contract",
            |content| PeppyContractParser::from_content(content).map_err(|e| e.to_string()),
        )?;
        Ok((exposures, contracts))
    }
}

/// Parses every document of one kind: the pin must be of that kind and the
/// bytes must fingerprint to it before `parse` sees them.
fn parse_all<T>(
    documents: &[PinnedDocument],
    kind: PinKind,
    what: &str,
    parse: impl Fn(&str) -> Result<T, String>,
) -> Result<Vec<Pinned<T>>, String> {
    documents
        .iter()
        .map(|document| {
            if document.pin.kind != kind {
                return Err(format!("{} is not {what}", document.pin.label()));
            }
            let parsed = parse(document.verified_content()?)
                .map_err(|error| format!("{} does not parse: {error}", document.pin.label()))?;
            Ok(Pinned {
                pin: document.pin.clone(),
                document: parsed,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::internal::repository::{
        EntryOrigin, GitCommit, ItemName, ItemTag, RepoRelativePath,
    };
    use peppy_mcp_catalog::McpExposure;

    const CAMERA_CONTRACT: &str = r#"{
        peppy_schema: "contract/v1",
        manifest: { name: "rgb_camera", tag: "v1" },
        interfaces: {
            topics: [
                {
                    name: "video_stream",
                    message_format: {
                        frame: { $type: "array", $items: "u8" },
                        encoding: "string",
                        width: "u32",
                        height: "u32",
                    },
                },
            ],
            services: [
                { name: "video_stream_info", response_message_format: { width: "u32" } },
            ],
        },
    }"#;

    const RECORDING_CONTRACT: &str = r#"{
        peppy_schema: "contract/v1",
        manifest: { name: "episode_recording", tag: "v1" },
        interfaces: {
            actions: [
                {
                    name: "record_episode",
                    goal_service: { request_message_format: { episode_name: "string" } },
                    result_service: { response_message_format: { frames: "u32" } },
                },
            ],
        },
    }"#;

    fn exposure_document(
        name: &str,
        tag: &str,
        target: &str,
        contract: &str,
        sha256: Option<&str>,
        member: &str,
    ) -> String {
        let pin = sha256
            .map(|sha| format!(", sha256: \"{sha}\""))
            .unwrap_or_default();
        format!(
            r#"{{
            peppy_schema: "mcp_exposure/v1",
            manifest: {{ name: "{name}", tag: "{tag}" }},
            server: {{ title: "{name}" }},
            targets: {{
                {target}: {{
                    contract: {{ name: "{contract}", tag: "v1"{pin} }},
                    services: [
                        {{
                            member: "{member}",
                            tool: "{target}.info",
                            description: "Report.",
                            operation: "read_only",
                            deadline_ms: 2000,
                        }},
                    ],
                }},
            }},
        }}"#
        )
    }

    fn pin(kind: PinKind, name: &str, tag: &str, content: &str) -> PinnedItem {
        PinnedItem {
            kind,
            name: ItemName::parse(name).expect("valid name"),
            tag: ItemTag::parse(tag).expect("valid tag"),
            sha256: ManifestFingerprint::of_bytes(content.as_bytes()),
            origin: EntryOrigin::Git {
                repo_url: "https://github.com/acme/hub".to_owned(),
                repo_ref: None,
                commit: GitCommit::parse(&"b".repeat(40)).expect("valid commit"),
                path: RepoRelativePath::parse(&format!("{name}.json5")).expect("valid path"),
            },
        }
    }

    fn contract(content: &str) -> PinnedContract {
        let document = PeppyContractParser::from_content(content).expect("contract parses");
        PinnedContract {
            pin: pin(
                PinKind::Contract,
                document.manifest.name.as_str(),
                &document.manifest.tag,
                content,
            ),
            document,
        }
    }

    fn exposure(content: &str) -> PinnedExposure {
        let document: McpExposure =
            crate::error::deserialize_json5_with_path(content).expect("exposure parses");
        PinnedExposure {
            pin: pin(
                PinKind::McpExposure,
                document.manifest.name.as_str(),
                &document.manifest.tag,
                content,
            ),
            document,
        }
    }

    fn reference(name: &str, tag: &str) -> ExposureRef {
        ExposureRef {
            name: name.to_owned(),
            tag: tag.to_owned(),
        }
    }

    #[test]
    fn the_identity_sorts_the_exposures_and_ignores_listing_order() {
        let name = built_in_identity(&[
            reference("camera_and_recording", "v2"),
            reference("arm_control", "v1"),
            reference("camera_and_recording", "v1"),
        ]);
        assert_eq!(
            name.as_str(),
            "mcp_arm_control_v1_camera_and_recording_v1_camera_and_recording_v2"
        );
        let reordered = built_in_identity(&[
            reference("camera_and_recording", "v1"),
            reference("camera_and_recording", "v2"),
            reference("arm_control", "v1"),
        ]);
        assert_eq!(reordered, name);
    }

    #[test]
    fn a_plan_merges_shared_targets_into_one_slot_and_unions_the_members() {
        let camera_exposure = exposure(&exposure_document(
            "camera_only",
            "v1",
            "front_camera",
            "rgb_camera",
            None,
            "video_stream_info",
        ));
        let both = exposure(&format!(
            r#"{{
            peppy_schema: "mcp_exposure/v1",
            manifest: {{ name: "camera_and_recording", tag: "v1" }},
            server: {{ title: "Both" }},
            targets: {{
                front_camera: {{
                    contract: {{ name: "rgb_camera", tag: "v1", sha256: "{}" }},
                    topics: [
                        {{
                            member: "video_stream",
                            resource: "front_camera.latest_frame",
                            description: "Latest frame.",
                            freshness: {{ max_age_ms: 1000 }},
                            update: {{ max_hz: 10 }},
                            representation: {{
                                image: "jpeg",
                                fields: {{ data: "frame", encoding: "encoding", width: "width", height: "height" }},
                            }},
                            max_result_bytes: 524288,
                            on_oversize: "downscale",
                        }},
                    ],
                    services: [
                        {{
                            member: "video_stream_info",
                            tool: "front_camera.info",
                            description: "Report.",
                            operation: "read_only",
                            deadline_ms: 2000,
                        }},
                    ],
                }},
                recorder: {{
                    contract: {{ name: "episode_recording", tag: "v1" }},
                    actions: [
                        {{
                            member: "record_episode",
                            tool: "recorder.record_episode",
                            description: "Record.",
                            operation: "long_running",
                            deadline_ms: 60000,
                        }},
                    ],
                }},
            }},
        }}"#,
            ManifestFingerprint::of_bytes(CAMERA_CONTRACT.as_bytes())
        ));
        let plan = plan_deployment(
            &[both, camera_exposure],
            &[contract(CAMERA_CONTRACT), contract(RECORDING_CONTRACT)],
        )
        .expect("the exposures plan");

        assert_eq!(
            plan.name.as_str(),
            "mcp_camera_and_recording_v1_camera_only_v1"
        );
        assert_eq!(plan.tag, "builtin");
        let manifest = serde_json::to_value(&plan.config).expect("serializes");
        assert_eq!(
            manifest["manifest"]["depends_on"]["contracts"],
            serde_json::json!([
                {
                    "name": "rgb_camera",
                    "tag": "v1",
                    "link_id": "front_camera",
                    "sha256": ManifestFingerprint::of_bytes(CAMERA_CONTRACT.as_bytes()).as_str(),
                },
                {
                    "name": "episode_recording",
                    "tag": "v1",
                    "link_id": "recorder",
                    "sha256": ManifestFingerprint::of_bytes(RECORDING_CONTRACT.as_bytes()).as_str(),
                },
            ])
        );
        assert_eq!(
            manifest["interfaces"]["services"]["consumes"],
            serde_json::json!([{ "link_id": "front_camera", "name": "video_stream_info" }]),
            "a member two exposures consume is declared once"
        );
        assert_eq!(
            manifest["interfaces"]["topics"]["consumes"],
            serde_json::json!([{ "link_id": "front_camera", "name": "video_stream" }])
        );
        assert_eq!(
            manifest["interfaces"]["actions"]["consumes"],
            serde_json::json!([{ "link_id": "recorder", "name": "record_episode" }])
        );
        assert_eq!(
            manifest["execution"]["parameters"]["port"],
            serde_json::json!({ "$type": "u16", "$default": 8900 })
        );
        assert_eq!(
            manifest["execution"]["run_cmd"],
            serde_json::json!(["peppy", "mcp", "serve"])
        );
        assert_eq!(
            manifest["execution"]["endpoints"],
            serde_json::json!({
                "camera_and_recording_v1": { "kind": "mcp", "description": "Both" },
                "camera_only_v1": { "kind": "mcp", "description": "camera_only" },
            }),
            "one endpoint per exposure, labelled `<name>_<tag>` with the exposure's title"
        );
        let served: Vec<String> = plan
            .exposures
            .iter()
            .map(|exposure| exposure.bundle.exposure.endpoint_path())
            .collect();
        assert_eq!(
            served,
            ["/camera_and_recording/v1/mcp", "/camera_only/v1/mcp"],
            "bundles follow the identity order"
        );
    }

    /// Two tags of one exposure name serve side by side, so their endpoints
    /// get distinct labels, in identity order.
    #[test]
    fn two_tags_of_one_exposure_get_distinct_endpoint_labels() {
        let v2 = exposure(&exposure_document(
            "camera",
            "v2",
            "cam",
            "rgb_camera",
            None,
            "video_stream_info",
        ));
        let v1 = exposure(&exposure_document(
            "camera",
            "v1",
            "cam",
            "rgb_camera",
            None,
            "video_stream_info",
        ));
        let plan = plan_deployment(&[v2, v1], &[contract(CAMERA_CONTRACT)]).expect("plans");
        let labels: Vec<String> = plan
            .config
            .execution
            .endpoints
            .keys()
            .map(|label| label.to_string())
            .collect();
        assert_eq!(labels, ["camera_v1", "camera_v2"]);
        assert!(
            plan.config
                .execution
                .endpoints
                .values()
                .all(|declaration| declaration.kind == config::node::EndpointKind::Mcp)
        );
        assert_eq!(
            endpoint_label("camera", "v1-beta"),
            "camera_v1_beta",
            "a tag is normalized the way generated code spells it"
        );
    }

    #[test]
    fn a_plan_is_stable_across_listing_order() {
        let a = exposure(&exposure_document(
            "a",
            "v1",
            "cam",
            "rgb_camera",
            None,
            "video_stream_info",
        ));
        let b = exposure(&exposure_document(
            "b",
            "v1",
            "cam",
            "rgb_camera",
            None,
            "video_stream_info",
        ));
        let contracts = [contract(CAMERA_CONTRACT)];
        let first = plan_deployment(&[a.clone(), b.clone()], &contracts).expect("plans");
        let second = plan_deployment(&[b, a], &contracts).expect("plans");
        assert_eq!(
            serde_json::to_value(&first.config).unwrap(),
            serde_json::to_value(&second.config).unwrap()
        );
        assert_eq!(first.name, second.name);
    }

    #[test]
    fn a_target_bound_to_two_contracts_is_refused_naming_both_exposures() {
        let camera = exposure(&exposure_document(
            "a",
            "v1",
            "front",
            "rgb_camera",
            None,
            "video_stream_info",
        ));
        let other = exposure(
            r#"{
            peppy_schema: "mcp_exposure/v1",
            manifest: { name: "b", tag: "v1" },
            server: { title: "B" },
            targets: {
                front: {
                    contract: { name: "episode_recording", tag: "v1" },
                    actions: [
                        {
                            member: "record_episode",
                            tool: "front.record",
                            description: "Record.",
                            operation: "long_running",
                            deadline_ms: 60000,
                        },
                    ],
                },
            },
        }"#,
        );
        let error = plan_deployment(
            &[camera, other],
            &[contract(CAMERA_CONTRACT), contract(RECORDING_CONTRACT)],
        )
        .expect_err("one target, two contracts");
        let McpDeploymentError::SlotConflict(conflict) = error else {
            panic!("expected a slot conflict, got {error}");
        };
        assert_eq!(
            conflict,
            SlotConflict {
                target: "front".to_owned(),
                first_exposure: "a:v1".to_owned(),
                first_contract: "rgb_camera:v1".to_owned(),
                second_exposure: "b:v1".to_owned(),
                second_contract: "episode_recording:v1".to_owned(),
            }
        );
    }

    #[test]
    fn an_author_pin_must_match_the_deployment_pin() {
        let wrong = "a".repeat(64);
        let mismatched = exposure(&exposure_document(
            "a",
            "v1",
            "cam",
            "rgb_camera",
            Some(&wrong),
            "video_stream_info",
        ));
        let error = plan_deployment(&[mismatched], &[contract(CAMERA_CONTRACT)])
            .expect_err("the author pin disagrees");
        assert!(
            matches!(error, McpDeploymentError::ContractPinMismatch { ref exposure, ref contract, .. } if exposure == "a:v1" && contract == "rgb_camera:v1"),
            "{error}"
        );

        let matching = exposure(&exposure_document(
            "a",
            "v1",
            "cam",
            "rgb_camera",
            Some(ManifestFingerprint::of_bytes(CAMERA_CONTRACT.as_bytes()).as_str()),
            "video_stream_info",
        ));
        plan_deployment(&[matching], &[contract(CAMERA_CONTRACT)]).expect("the author pin agrees");
    }

    #[test]
    fn a_contract_the_deployment_does_not_pin_is_refused_by_name() {
        let orphan = exposure(&exposure_document(
            "a",
            "v1",
            "cam",
            "rgb_camera",
            None,
            "video_stream_info",
        ));
        let error =
            plan_deployment(&[orphan], &[contract(RECORDING_CONTRACT)]).expect_err("no camera pin");
        assert!(
            matches!(error, McpDeploymentError::ContractNotPinned { ref contract, .. } if contract == "rgb_camera:v1"),
            "{error}"
        );
    }

    /// A per-robot surface over the camera contract: one target a robot
    /// fills any number of times, named by `camera`.
    fn per_robot_document(name: &str, argument: &str) -> String {
        format!(
            r#"{{
            peppy_schema: "mcp_exposure/v1",
            manifest: {{ name: "{name}", tag: "v1" }},
            server: {{ title: "{name}" }},
            robots: {{
                list: {{ tool: "robot.list", description: "The robots." }},
            }},
            targets: {{
                camera: {{
                    contract: {{ name: "rgb_camera", tag: "v1" }},
                    argument: "{argument}",
                    services: [
                        {{
                            member: "video_stream_info",
                            tool: "camera.info",
                            description: "Report.",
                            operation: "read_only",
                            deadline_ms: 2000,
                        }},
                    ],
                }},
                main_camera: {{
                    contract: {{ name: "rgb_camera", tag: "v1" }},
                    services: [
                        {{
                            member: "video_stream_info",
                            tool: "main_camera.info",
                            description: "Report.",
                            operation: "read_only",
                            deadline_ms: 2000,
                        }},
                    ],
                }},
            }},
        }}"#
        )
    }

    #[test]
    fn a_per_robot_surface_declares_every_target_a_zero_or_more_slot() {
        let plan = plan_deployment(
            &[exposure(&per_robot_document("robot_control", "camera"))],
            &[contract(CAMERA_CONTRACT)],
        )
        .expect("plans");
        let slots = &plan.config.manifest.depends_on.as_ref().unwrap().contracts;
        assert_eq!(slots.len(), 2);
        assert_eq!(slots[0].link_id, "camera");
        assert_eq!(slots[0].cardinality, config::node::Cardinality::ZeroOrMore);
        assert_eq!(slots[1].link_id, "main_camera");
        assert_eq!(slots[1].cardinality, config::node::Cardinality::ZeroOrMore);
        // The robots are the stack's copies: they address the members of
        // `camera` by its argument and fill `main_camera` once each.
        assert_eq!(
            plan.addressing,
            MemberAddressing::ByCopy {
                one_per_copy: ["main_camera".to_owned()].into_iter().collect(),
            }
        );
        let peppy_mcp_catalog::BundleSurface::PerRobot { contracts, .. } =
            &plan.exposures[0].bundle.surface
        else {
            panic!("expected a per-robot bundle");
        };
        assert_eq!(contracts[0].argument.as_deref(), Some("camera"));
        let fixed = plan_deployment(
            &[exposure(&exposure_document(
                "camera_only",
                "v1",
                "camera",
                "rgb_camera",
                None,
                "video_stream_info",
            ))],
            &[contract(CAMERA_CONTRACT)],
        )
        .expect("plans");
        let slots = &fixed.config.manifest.depends_on.as_ref().unwrap().contracts;
        assert_eq!(slots[0].cardinality, config::node::Cardinality::One);
        assert_eq!(fixed.addressing, MemberAddressing::WholeSet);
    }

    #[test]
    fn a_deployment_mixing_a_per_robot_and_a_fixed_surface_is_refused() {
        let error = plan_deployment(
            &[
                exposure(&per_robot_document("robot_control", "camera")),
                exposure(&exposure_document(
                    "camera_only",
                    "v1",
                    "front_camera",
                    "rgb_camera",
                    None,
                    "video_stream_info",
                )),
            ],
            &[contract(CAMERA_CONTRACT)],
        )
        .expect_err("mixed surfaces are refused");
        assert_eq!(
            error.to_string(),
            "exposure `robot_control:v1` is a per-robot surface and exposure `camera_only:v1` \
             is not; one deployment serves per-robot surfaces alone or fixed surfaces alone, so \
             list `robot_control:v1` in a deployment of its own"
        );
    }

    #[test]
    fn two_surfaces_sharing_a_target_must_name_its_member_alike() {
        let error = plan_deployment(
            &[
                exposure(&per_robot_document("robot_control", "camera")),
                exposure(&per_robot_document("robot_cameras", "device")),
            ],
            &[contract(CAMERA_CONTRACT)],
        )
        .expect_err("differing arguments are refused");
        assert!(
            error.to_string().starts_with(
                "target `camera` takes argument `device` in exposure `robot_cameras:v1` and \
                 argument `camera` in exposure `robot_control:v1`"
            ),
            "{error}"
        );
    }

    #[test]
    fn every_invalid_exposure_is_reported_at_once() {
        let first = exposure(&exposure_document(
            "a",
            "v1",
            "cam",
            "rgb_camera",
            None,
            "no_such_service",
        ));
        let second = exposure(&exposure_document(
            "b",
            "v1",
            "cam",
            "rgb_camera",
            None,
            "still_missing",
        ));
        let error = plan_deployment(&[first, second], &[contract(CAMERA_CONTRACT)])
            .expect_err("both exposures are invalid");
        let McpDeploymentError::Invalid(reports) = &error else {
            panic!("expected validation reports, got {error}");
        };
        let exposures: Vec<&str> = reports.iter().map(|r| r.exposure.as_str()).collect();
        assert_eq!(exposures, ["a:v1", "b:v1"]);
        let rendered = error.to_string();
        assert!(rendered.contains("no_such_service"), "{rendered}");
        assert!(rendered.contains("still_missing"), "{rendered}");
    }

    #[test]
    fn an_empty_list_and_a_repeated_exposure_are_refused() {
        assert!(matches!(
            plan_deployment(&[], &[]).expect_err("nothing to serve"),
            McpDeploymentError::NoExposures
        ));
        let one = exposure(&exposure_document(
            "a",
            "v1",
            "cam",
            "rgb_camera",
            None,
            "video_stream_info",
        ));
        let error = plan_deployment(&[one.clone(), one], &[contract(CAMERA_CONTRACT)])
            .expect_err("listed twice");
        assert!(
            matches!(error, McpDeploymentError::DuplicateExposure { ref exposure } if exposure == "a:v1"),
            "{error}"
        );
    }

    #[test]
    fn a_serve_spec_round_trips_and_verifies_its_bytes() {
        let exposure_text =
            exposure_document("a", "v1", "cam", "rgb_camera", None, "video_stream_info");
        let spec = McpServeSpec {
            exposures: vec![PinnedDocument::new(
                pin(PinKind::McpExposure, "a", "v1", &exposure_text),
                exposure_text.clone(),
            )],
            contracts: vec![PinnedDocument::new(
                pin(PinKind::Contract, "rgb_camera", "v1", CAMERA_CONTRACT),
                CAMERA_CONTRACT.to_owned(),
            )],
        };
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("mcp_serve.json5");
        spec.write(&path).expect("writes");
        let read = McpServeSpec::from_path(&path).expect("reads");
        assert_eq!(read, spec);
        let (exposures, contracts) = read.resolve().expect("every document verifies");
        assert_eq!(exposures.len(), 1);
        assert_eq!(contracts.len(), 1);
        plan_deployment(&exposures, &contracts).expect("the spec plans");

        let tampered = McpServeSpec {
            contracts: vec![PinnedDocument::new(
                pin(PinKind::Contract, "rgb_camera", "v1", CAMERA_CONTRACT),
                RECORDING_CONTRACT.to_owned(),
            )],
            ..spec
        };
        let error = tampered
            .resolve()
            .expect_err("bytes that do not fingerprint to the pin");
        assert!(error.contains("contract `rgb_camera:v1`"), "{error}");
        assert!(error.contains("not the pinned"), "{error}");
    }
}
