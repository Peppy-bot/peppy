//! Plan-phase validation for a node's `depends_on` block and its
//! consumed interfaces. Lives next to the types it validates so any
//! consumer that parses a [`NodeConfig`] graph (launchers, the daemon's
//! node stack, code-generation) can run the same checks without
//! depending on a runtime crate.
//!
//! The validator is structural — it does not care which crate orchestrates
//! the lookup. Callers supply a closure that resolves a `(name, tag)`
//! pair to a [`NodeConfig`] from whatever store they own (an in-memory
//! graph, a working stack snapshot, a parsed launcher batch, etc.).

use std::collections::{HashMap, HashSet};

use crate::error::{ConsumedInterfaceOnlyContractBacked, MissingInterface, ParsingError};
use crate::node::{ImplementsEntry, InterfaceKind, Interfaces, Manifest, NodeConfig};

/// Minimal `(name, tag)` view of a single `depends_on.nodes` entry,
/// stripped of the link_id noise that callers don't need
/// once dependency resolution has already happened. Used by
/// [`collect_dependency_specs`] so callers can walk the dep set without
/// re-deriving it from the manifest.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct DependencySpec {
    pub node_name: String,
    pub node_tag: String,
}

pub fn collect_dependency_specs(node: &NodeConfig) -> Vec<DependencySpec> {
    let Some(depends_on) = &node.manifest.depends_on else {
        return Vec::new();
    };

    depends_on
        .nodes
        .iter()
        .map(|dep| DependencySpec {
            node_name: dep.name.as_str().to_owned(),
            node_tag: dep.tag.clone(),
        })
        .collect()
}

/// Does this node claim to implement contract `(name, tag)`? Contract
/// providers are matched solely by `manifest.implements`, never by node-name
/// identity, consistent with the binding validator's `slot_matches_producer`.
/// This is the one source of truth for "node X provides contract Y", shared by
/// the node stack, the benchmark, and the service/action cycle check.
pub fn node_implements(node: &NodeConfig, name: &str, tag: &str) -> bool {
    node.manifest
        .implements
        .iter()
        .any(|item| item.name.as_str() == name && item.tag == tag)
}

/// One resolved contract-implementation dependency edge: `consumer` declares
/// `depends_on.contracts` for `contract`, and `provider` declares that
/// contract in `manifest.implements`. Distinct from a direct node dependency
/// (captured by [`collect_dependency_specs`]) — contract deps are deliberately
/// kept out of the node-dependency DAG, so this is the only place they surface
/// for display/measurement purposes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ContractImplementationEdge {
    pub consumer_name: String,
    pub consumer_tag: String,
    pub provider_name: String,
    pub provider_tag: String,
    pub contract_name: String,
    pub contract_tag: String,
}

/// Resolve every contract-implementation edge among `configs`: for each
/// consumer's `depends_on.contracts` entry, emit one edge per config that
/// [`node_implements`] that contract. A contract dep with no provider in the
/// set yields no edge (mirroring how a node dep absent from the graph produces
/// no edge); a contract with several implementers fans out to all of them.
pub fn collect_contract_implementation_edges(
    configs: &[&NodeConfig],
) -> Vec<ContractImplementationEdge> {
    let mut edges = Vec::new();
    for consumer in configs {
        let Some(depends_on) = consumer.manifest.depends_on.as_ref() else {
            continue;
        };
        for dep in &depends_on.contracts {
            let contract_name = dep.name.as_str();
            let contract_tag = dep.tag.as_str();
            for provider in configs {
                if node_implements(provider, contract_name, contract_tag) {
                    edges.push(ContractImplementationEdge {
                        consumer_name: consumer.manifest.name.as_str().to_owned(),
                        consumer_tag: consumer.manifest.tag.clone(),
                        provider_name: provider.manifest.name.as_str().to_owned(),
                        provider_tag: provider.manifest.tag.clone(),
                        contract_name: contract_name.to_owned(),
                        contract_tag: contract_tag.to_owned(),
                    });
                }
            }
        }
    }
    edges
}

/// What an unresolvable `depends_on.nodes` entry means to a
/// [`validate_dependency_specs`] caller: code generation needs every
/// dependency's interfaces no matter how many instances a deployment runs,
/// while a running stack only owes an instance to slots whose cardinality
/// demands one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MissingDependencyPolicy {
    /// Build/codegen contexts: every declared dependency must resolve.
    RequireResolvable,
    /// Runtime-presence contexts: absence is legal where the slot's
    /// cardinality admits an empty bound set. Whether the empty slot is
    /// properly covered stays binding validation's question.
    AllowAbsentWhenSlotAdmitsEmpty,
}

/// Validates that all dependencies of a node config exist and expose the required interfaces.
///
/// Uses the provided `resolve` closure to look up a dependency's `NodeConfig` by name and tag.
/// Returns a list of all validation errors found (empty if all dependencies are satisfied).
///
/// Validation is two-phase:
/// 1. **Node existence**: Each entry in `manifest.depends_on.nodes` must resolve to an existing
///    node, except where `missing_dependency_policy` admits its absence.
/// 2. **Interface exposure**: Each consumed/expected interface must reference a valid `link_id`
///    declared in `depends_on.nodes`, `depends_on.contracts`, or (for
///    `topics.consumes` only) `depends_on.pairings` /
///    `depends_on.pairing_observers`. For node-backed link_ids
///    the producer must expose the required interface. An unresolved
///    dependency's consumed interfaces are skipped, whether its absence was
///    reported or admitted; exposure is checked wherever the dependency
///    does resolve (node add/sync).
///
/// Contract- and pairing-backed link_ids only get the declaration check here.
/// Neither document is reachable from this crate, so whether the entry names a
/// real member is checked at node add/sync, where they resolve.
pub fn validate_dependency_specs(
    manifest: &Manifest,
    interfaces: &Interfaces,
    dependant_name: &str,
    dependant_tag: &str,
    missing_dependency_policy: MissingDependencyPolicy,
    resolve: impl Fn(&str, &str) -> Option<NodeConfig>,
) -> Vec<ParsingError> {
    let mut errors = Vec::new();

    // Build link_id → (name, tag, resolved_config) lookup from depends_on.nodes
    let mut resolved_deps: HashMap<String, (String, String, NodeConfig)> = HashMap::new();

    // Phase 1: Validate all declared dependency nodes exist
    if let Some(depends_on) = &manifest.depends_on {
        for dep in &depends_on.nodes {
            let Some(dependency_config) = resolve(dep.name.as_str(), &dep.tag) else {
                if missing_dependency_policy
                    == MissingDependencyPolicy::AllowAbsentWhenSlotAdmitsEmpty
                    && dep.cardinality.admits(0)
                {
                    continue;
                }
                errors.push(ParsingError::MissingDependency {
                    dependant: dependant_name.to_owned(),
                    dependant_tag: dependant_tag.to_owned(),
                    dependency: dep.name.as_str().to_owned(),
                    dependency_tag: dep.tag.clone(),
                });
                continue;
            };
            resolved_deps.insert(
                dep.link_id.clone(),
                (
                    dep.name.as_str().to_owned(),
                    dep.tag.clone(),
                    dependency_config,
                ),
            );
        }
    }

    // Collect all declared link_ids so we can distinguish "declared but unresolved"
    // (a reported MissingDependency, an admitted absence, or a contract-backed
    // dep whose members resolve at node add/sync) from "never declared" (typo).
    let declared_link_ids: HashSet<&str> = manifest
        .depends_on
        .as_ref()
        .map(|d| {
            d.nodes
                .iter()
                .map(|n| n.link_id.as_str())
                .chain(d.contracts.iter().map(|i| i.link_id.as_str()))
                .collect()
        })
        .unwrap_or_default();

    // A pairing slot (participant or observer) is a legal target for
    // `topics.consumes`: a participant names the counterpart role's topics it
    // wants a module for, an observer names the observed role's own topics.
    // Neither is ever legal for `services.consumes` or `actions.consumes`,
    // because a pairing document declares topics only. Kept separate from
    // `declared_link_ids` so a service or action naming one gets a dedicated
    // error rather than `UndeclaredLinkId`, and so a topic naming one skips the
    // node-dependency exposure check that cannot apply to it. The topic
    // direction (counterpart vs observed role) is resolved against the pairing
    // document at node add/sync, not here.
    let pairing_link_ids: HashSet<&str> = manifest
        .depends_on
        .as_ref()
        .map(|d| d.pairing_link_ids().collect())
        .unwrap_or_default();

    // Implements slots are produced, not consumed — a consumed item naming
    // one gets a dedicated error too. Parse-time validation already rejects
    // this for configs that went through `NodeConfigParser`, but this
    // validator also runs on programmatically assembled configs.
    let implements_link_ids: HashSet<&str> = manifest
        .implements
        .iter()
        .map(|e| e.link_id.as_str())
        .collect();

    // Phase 2: Validate consumed interfaces reference valid link_ids
    // and that the dependency exposes the required interface
    for kind in [
        InterfaceKind::Topic,
        InterfaceKind::Service,
        InterfaceKind::Action,
    ] {
        validate_consumed_items(
            interfaces
                .consumed(kind)
                .map(|(link_id, name, _)| (link_id, name)),
            kind,
            &resolved_deps,
            &declared_link_ids,
            &pairing_link_ids,
            &implements_link_ids,
            dependant_name,
            dependant_tag,
            &mut errors,
        );
    }

    errors
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct InterfaceRequirement {
    kind: InterfaceKind,
    name: String,
}

impl InterfaceRequirement {
    fn new(kind: InterfaceKind, name: &str) -> Self {
        Self {
            kind,
            name: name.trim().to_owned(),
        }
    }

    fn kind(&self) -> InterfaceKind {
        self.kind
    }

    fn name(&self) -> &str {
        &self.name
    }
}

/// Validates a set of consumed interfaces, checking that each `link_id` is declared
/// and that the referenced dependency exposes the required interface.
#[allow(clippy::too_many_arguments)]
fn validate_consumed_items<'a>(
    items: impl Iterator<Item = (&'a str, &'a str)>,
    kind: InterfaceKind,
    resolved_deps: &HashMap<String, (String, String, NodeConfig)>,
    declared_link_ids: &HashSet<&str>,
    pairing_link_ids: &HashSet<&str>,
    implements_link_ids: &HashSet<&str>,
    dependant_name: &str,
    dependant_tag: &str,
    errors: &mut Vec<ParsingError>,
) {
    for (link_id, name) in items {
        if pairing_link_ids.contains(link_id) {
            if kind != InterfaceKind::Topic {
                errors.push(ParsingError::ConsumedItemReferencesPairingLinkId {
                    dependant: dependant_name.to_owned(),
                    dependant_tag: dependant_tag.to_owned(),
                    link_id: link_id.to_owned(),
                    section: kind.consumed_section().to_owned(),
                });
            }
            // A pairing-backed topic resolves against the pairing document at
            // node add/sync, not against a node dependency.
            continue;
        }
        if implements_link_ids.contains(link_id) {
            errors.push(ParsingError::ConsumedItemReferencesImplementsLinkId {
                link_id: link_id.to_owned(),
            });
            continue;
        }
        if !resolved_deps.contains_key(link_id) {
            if !declared_link_ids.contains(link_id) {
                errors.push(ParsingError::UndeclaredLinkId {
                    dependant: dependant_name.to_owned(),
                    dependant_tag: dependant_tag.to_owned(),
                    link_id: link_id.to_owned(),
                });
            }
            continue;
        }
        validate_consumed_interface(
            link_id,
            name,
            kind,
            resolved_deps,
            dependant_name,
            dependant_tag,
            errors,
        );
    }
}

/// Validates that a consumed interface's `link_id` resolves to a dependency
/// that natively exposes the required interface. Node dependencies expose
/// native interfaces only: a name the producer provides solely as part of an
/// implemented contract gets a dedicated error pointing at
/// `depends_on.contracts` instead of a generic "not exposed".
fn validate_consumed_interface(
    link_id: &str,
    interface_name: &str,
    kind: InterfaceKind,
    resolved_deps: &HashMap<String, (String, String, NodeConfig)>,
    dependant_name: &str,
    dependant_tag: &str,
    errors: &mut Vec<ParsingError>,
) {
    let Some((dep_name, dep_tag, dep_config)) = resolved_deps.get(link_id) else {
        // The link_id doesn't map to any resolved dependency.
        // This path is only reached when the dependency was declared but did
        // not resolve (reported as MissingDependency in Phase 1, or admitted
        // absent by the policy).
        // Undeclared link_ids are caught before this function is called.
        return;
    };

    let requirement = InterfaceRequirement::new(kind, interface_name);
    match find_exposure(dep_config, &requirement) {
        Exposure::Native => {}
        Exposure::ContractBackedOnly(entry) => {
            errors.push(ParsingError::ConsumedInterfaceOnlyContractBacked(Box::new(
                ConsumedInterfaceOnlyContractBacked {
                    dependant: dependant_name.to_owned(),
                    dependant_tag: dependant_tag.to_owned(),
                    dependency: dep_name.clone(),
                    dependency_tag: dep_tag.clone(),
                    interface_kind: kind.label().to_owned(),
                    interface_name: interface_name.to_owned(),
                    link_id: link_id.to_owned(),
                    contract_name: entry.name.as_str().to_owned(),
                    contract_tag: entry.tag.clone(),
                },
            )));
        }
        Exposure::None => {
            errors.push(ParsingError::MissingInterface(Box::new(MissingInterface {
                dependant: dependant_name.to_owned(),
                dependant_tag: dependant_tag.to_owned(),
                dependency: dep_name.clone(),
                dependency_tag: dep_tag.clone(),
                interface_kind: kind.label().to_owned(),
                interface_name: interface_name.to_owned(),
            })));
        }
    }
}

/// How (and whether) a producer provides a required interface to node-dep
/// consumers. `ContractBackedOnly` carries the implements slot the matching
/// contract-backed entry references, for the dedicated error's payload.
enum Exposure<'a> {
    Native,
    ContractBackedOnly(&'a ImplementsEntry),
    None,
}

fn find_exposure<'a>(node: &'a NodeConfig, requirement: &InterfaceRequirement) -> Exposure<'a> {
    let (native_match, contract_link_id) = scan_entries(
        node.interfaces.produced(requirement.kind()),
        requirement.name(),
    );

    if native_match {
        return Exposure::Native;
    }
    if let Some(link_id) = contract_link_id
        && let Some(entry) = node
            .manifest
            .implements
            .iter()
            .find(|e| e.link_id == link_id)
    {
        return Exposure::ContractBackedOnly(entry);
    }
    Exposure::None
}

/// Scans produced entries of one kind for `name`: returns whether a native
/// entry matches, and the link_id of a matching contract-backed entry (if
/// any) for the dedicated-error payload.
fn scan_entries<'a>(
    entries: impl Iterator<Item = (Option<&'a str>, &'a str)>,
    name: &str,
) -> (bool, Option<&'a str>) {
    let mut contract_link_id = None;
    for (link_id, entry_name) in entries {
        if entry_name.trim() != name {
            continue;
        }
        match link_id {
            None => return (true, None),
            Some(id) => contract_link_id = Some(id),
        }
    }
    (false, contract_link_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::node::NodeConfigParser;

    fn parse(content: &str) -> NodeConfig {
        NodeConfigParser::from_content(content).expect("parse node config")
    }

    /// A node that implements the `uvc_camera:v1` contract (the provider).
    fn camera_mock() -> NodeConfig {
        parse(
            r#"{
                peppy_schema: "node/v1",
                manifest: {
                    name: "uvc_camera_python_mock", tag: "v1",
                    implements: [ { name: "uvc_camera", tag: "v1", link_id: "cam" } ]
                },
                execution: { language: "rust", run_cmd: ["camera"] },
                interfaces: {
                    topics: { emits: [ { link_id: "cam", name: "video_stream" } ] },
                    services: { exposes: [ { link_id: "cam", name: "video_stream_info" } ] }
                }
            }"#,
        )
    }

    /// A node that depends on the `uvc_camera:v1` contract (the consumer),
    /// consuming it as both a topic and a service over link `camera`.
    fn brain() -> NodeConfig {
        parse(
            r#"{
                peppy_schema: "node/v1",
                manifest: {
                    name: "brain", tag: "v1",
                    depends_on: {
                        contracts: [ { name: "uvc_camera", tag: "v1", link_id: "camera" } ]
                    }
                },
                execution: { language: "rust", run_cmd: ["brain"] },
                interfaces: {
                    topics: { consumes: [ { link_id: "camera", name: "video_stream" } ] },
                    services: { consumes: [ { link_id: "camera", name: "video_stream_info" } ] }
                }
            }"#,
        )
    }

    fn unrelated() -> NodeConfig {
        parse(
            r#"{
                peppy_schema: "node/v1",
                manifest: { name: "other", tag: "v1" },
                execution: { language: "rust", run_cmd: ["other"] }
            }"#,
        )
    }

    /// Wraps an interfaces block in a node holding one `arm_link/v1` pairing
    /// slot under link_id `controller`, playing the `arm` role.
    fn node_with_pairing(interfaces: &str) -> NodeConfig {
        parse(&format!(
            r#"{{
                peppy_schema: "node/v1",
                manifest: {{
                    name: "robot_arm", tag: "v1",
                    depends_on: {{
                        pairings: [ {{ name: "arm_link", tag: "v1", role: "arm", link_id: "controller" }} ]
                    }}
                }},
                execution: {{ language: "rust", run_cmd: ["robot_arm"] }},
                interfaces: {interfaces}
            }}"#
        ))
    }

    /// A consumed topic naming a pairing slot is legal: it selects one of the
    /// counterpart role's topics. Membership resolves against the pairing
    /// document at node add/sync, which this layer cannot reach, so it must
    /// report nothing at all here.
    #[test]
    fn consumed_topic_referencing_pairing_link_id_is_accepted() {
        let node = node_with_pairing(
            r#"{ topics: { consumes: [ { link_id: "controller", name: "joint_commands" } ] } }"#,
        );
        let errors = validate_dependency_specs(
            &node.manifest,
            &node.interfaces,
            "robot_arm",
            "v1",
            MissingDependencyPolicy::RequireResolvable,
            |_, _| None,
        );
        assert!(
            errors.is_empty(),
            "a pairing-backed consumed topic must not be reported here: {errors:?}"
        );
    }

    /// Services and actions keep the dedicated rejection: a pairing document
    /// declares topics only, so there is nothing for such an entry to name.
    #[test]
    fn consumed_service_or_action_referencing_pairing_link_id_gets_dedicated_error() {
        for (section, interfaces) in [
            (
                "services.consumes",
                r#"{ services: { consumes: [ { link_id: "controller", name: "calibrate" } ] } }"#,
            ),
            (
                "actions.consumes",
                r#"{ actions: { consumes: [ { link_id: "controller", name: "home" } ] } }"#,
            ),
        ] {
            let node = node_with_pairing(interfaces);
            let errors = validate_dependency_specs(
                &node.manifest,
                &node.interfaces,
                "robot_arm",
                "v1",
                MissingDependencyPolicy::RequireResolvable,
                |_, _| None,
            );
            assert_eq!(errors.len(), 1, "errors: {errors:?}");
            assert!(
                matches!(
                    &errors[0],
                    ParsingError::ConsumedItemReferencesPairingLinkId { link_id, section: s, .. }
                        if link_id == "controller" && s == section
                ),
                "expected ConsumedItemReferencesPairingLinkId({section}), got: {:?}",
                errors[0]
            );
        }
    }

    /// The section-scoping rule is identical for an observer slot: it may be
    /// named in `topics.consumes` (it taps the observed role's topics) but
    /// never in `services.consumes` or `actions.consumes`, since a pairing
    /// document declares topics only.
    #[test]
    fn consumed_service_or_action_referencing_observer_link_id_gets_dedicated_error() {
        for (section, extra) in [
            (
                "services.consumes",
                r#"services: { consumes: [ { link_id: "observed_arm", name: "calibrate" } ] },"#,
            ),
            (
                "actions.consumes",
                r#"actions: { consumes: [ { link_id: "observed_arm", name: "home" } ] },"#,
            ),
        ] {
            // The observer slot is consumed in topics.consumes (satisfying the
            // "observer must be consumed" rule) AND named in a service/action
            // section, which is the rejected combination under test.
            let node = parse(&format!(
                r#"{{
                    peppy_schema: "node/v1",
                    manifest: {{
                        name: "lerobot_recorder", tag: "v1",
                        depends_on: {{
                            pairing_observers: [ {{ name: "arm_link", tag: "v1", role: "arm", link_id: "observed_arm" }} ]
                        }}
                    }},
                    execution: {{ language: "rust", run_cmd: ["recorder"] }},
                    interfaces: {{
                        topics: {{ consumes: [ {{ link_id: "observed_arm", name: "joint_states" }} ] }},
                        {extra}
                    }}
                }}"#
            ));
            let errors = validate_dependency_specs(
                &node.manifest,
                &node.interfaces,
                "lerobot_recorder",
                "v1",
                MissingDependencyPolicy::RequireResolvable,
                |_, _| None,
            );
            assert!(
                errors.iter().any(|e| matches!(
                    e,
                    ParsingError::ConsumedItemReferencesPairingLinkId { link_id, section: s, .. }
                        if link_id == "observed_arm" && s == section
                )),
                "expected ConsumedItemReferencesPairingLinkId({section}) for observer slot, got: {errors:?}"
            );
        }
    }

    #[test]
    fn node_implements_matches_declared_contract_only() {
        let cam = camera_mock();
        assert!(node_implements(&cam, "uvc_camera", "v1"));
        assert!(!node_implements(&cam, "uvc_camera", "v2"));
        assert!(!node_implements(&cam, "other_contract", "v1"));
        // A node that declares no `implements` matches nothing.
        assert!(!node_implements(&unrelated(), "uvc_camera", "v1"));
    }

    #[test]
    fn implementation_edges_resolve_consumer_to_implementing_provider() {
        let brain = brain();
        let cam = camera_mock();
        let other = unrelated();
        let configs = [&brain, &cam, &other];

        let edges = collect_contract_implementation_edges(&configs);

        // One edge despite the consumer using the contract as two artifacts:
        // the edge is per (consumer, contract dep, provider), not per artifact.
        assert_eq!(edges.len(), 1, "edges: {edges:?}");
        let e = &edges[0];
        assert_eq!(e.consumer_name, "brain");
        assert_eq!(e.consumer_tag, "v1");
        assert_eq!(e.provider_name, "uvc_camera_python_mock");
        assert_eq!(e.provider_tag, "v1");
        assert_eq!(e.contract_name, "uvc_camera");
        assert_eq!(e.contract_tag, "v1");
    }

    #[test]
    fn implementation_edges_empty_when_no_provider_present() {
        let brain = brain();
        let other = unrelated();
        let configs = [&brain, &other];
        assert!(collect_contract_implementation_edges(&configs).is_empty());
    }

    #[test]
    fn implementation_edges_fan_out_to_every_implementing_provider() {
        let brain = brain();
        let cam = camera_mock();
        // A second, differently-named node that also implements uvc_camera:v1.
        let cam2 = parse(
            r#"{
                peppy_schema: "node/v1",
                manifest: {
                    name: "uvc_camera_other_mock", tag: "v1",
                    implements: [ { name: "uvc_camera", tag: "v1", link_id: "cam" } ]
                },
                execution: { language: "rust", run_cmd: ["camera2"] },
                interfaces: {
                    topics: { emits: [ { link_id: "cam", name: "video_stream" } ] }
                }
            }"#,
        );
        let configs = [&brain, &cam, &cam2];
        let edges = collect_contract_implementation_edges(&configs);
        assert_eq!(
            edges.len(),
            2,
            "should fan out to both implementers: {edges:?}"
        );
        assert!(
            edges
                .iter()
                .any(|e| e.provider_name == "uvc_camera_python_mock")
        );
        assert!(
            edges
                .iter()
                .any(|e| e.provider_name == "uvc_camera_other_mock")
        );
    }

    /// A producer with one native topic and one contract-backed topic.
    fn hybrid_producer() -> NodeConfig {
        parse(
            r#"{
                peppy_schema: "node/v1",
                manifest: {
                    name: "hybrid", tag: "v1",
                    implements: [ { name: "uvc_camera", tag: "v1", link_id: "cam" } ]
                },
                execution: { language: "rust", run_cmd: ["hybrid"] },
                interfaces: {
                    topics: { emits: [
                        { link_id: "cam", name: "video_stream" },
                        { name: "debug_stream", message_format: { x: "f64" } }
                    ] }
                }
            }"#,
        )
    }

    /// A consumer of `producer_topic` over a `depends_on.nodes` slot.
    fn node_dep_consumer(topic: &str) -> NodeConfig {
        parse(&format!(
            r#"{{
                peppy_schema: "node/v1",
                manifest: {{
                    name: "consumer", tag: "v1",
                    depends_on: {{
                        nodes: [ {{ name: "hybrid", tag: "v1", link_id: "producer" }} ]
                    }}
                }},
                execution: {{ language: "rust", run_cmd: ["consumer"] }},
                interfaces: {{
                    topics: {{ consumes: [ {{ link_id: "producer", name: "{topic}" }} ] }}
                }}
            }}"#
        ))
    }

    #[test]
    fn node_dep_consumer_of_native_interface_passes() {
        let consumer = node_dep_consumer("debug_stream");
        let producer = hybrid_producer();
        let errors = validate_dependency_specs(
            &consumer.manifest,
            &consumer.interfaces,
            "consumer",
            "v1",
            MissingDependencyPolicy::RequireResolvable,
            |name, _| (name == "hybrid").then(|| producer.clone()),
        );
        assert!(errors.is_empty(), "errors: {errors:?}");
    }

    #[test]
    fn node_dep_consumer_of_contract_backed_only_interface_gets_dedicated_error() {
        let consumer = node_dep_consumer("video_stream");
        let producer = hybrid_producer();
        let errors = validate_dependency_specs(
            &consumer.manifest,
            &consumer.interfaces,
            "consumer",
            "v1",
            MissingDependencyPolicy::RequireResolvable,
            |name, _| (name == "hybrid").then(|| producer.clone()),
        );
        assert_eq!(errors.len(), 1, "errors: {errors:?}");
        let ParsingError::ConsumedInterfaceOnlyContractBacked(payload) = &errors[0] else {
            panic!(
                "expected ConsumedInterfaceOnlyContractBacked, got: {:?}",
                errors[0]
            );
        };
        assert_eq!(payload.contract_name, "uvc_camera");
        assert_eq!(payload.contract_tag, "v1");
        assert_eq!(payload.interface_name, "video_stream");
    }

    #[test]
    fn node_dep_consumer_of_missing_interface_still_gets_missing_interface() {
        let consumer = node_dep_consumer("nonexistent");
        let producer = hybrid_producer();
        let errors = validate_dependency_specs(
            &consumer.manifest,
            &consumer.interfaces,
            "consumer",
            "v1",
            MissingDependencyPolicy::RequireResolvable,
            |name, _| (name == "hybrid").then(|| producer.clone()),
        );
        assert_eq!(errors.len(), 1, "errors: {errors:?}");
        assert!(matches!(&errors[0], ParsingError::MissingInterface(_)));
    }

    /// Producer providing the same name both natively and contract-backed:
    /// node-dep consumers resolve the native one by scope (no precedence rule
    /// involved), so validation passes.
    #[test]
    fn node_dep_consumer_resolves_native_when_producer_has_both() {
        let producer = parse(
            r#"{
                peppy_schema: "node/v1",
                manifest: {
                    name: "hybrid", tag: "v1",
                    implements: [ { name: "uvc_camera", tag: "v1", link_id: "cam" } ]
                },
                execution: { language: "rust", run_cmd: ["hybrid"] },
                interfaces: {
                    topics: { emits: [
                        { link_id: "cam", name: "video_stream" },
                        { name: "video_stream", message_format: { x: "f64" } }
                    ] }
                }
            }"#,
        );
        let consumer = node_dep_consumer("video_stream");
        let errors = validate_dependency_specs(
            &consumer.manifest,
            &consumer.interfaces,
            "consumer",
            "v1",
            MissingDependencyPolicy::RequireResolvable,
            |name, _| (name == "hybrid").then(|| producer.clone()),
        );
        assert!(errors.is_empty(), "errors: {errors:?}");
    }

    /// A consumer of `finish` over a node-dep slot of the given cardinality,
    /// for the missing-dependency policy tests.
    fn optional_dep_consumer(cardinality: &str) -> NodeConfig {
        parse(&format!(
            r#"{{
                peppy_schema: "node/v1",
                manifest: {{
                    name: "panel", tag: "v1",
                    depends_on: {{
                        nodes: [ {{ name: "recorder", tag: "v1", link_id: "rec", cardinality: "{cardinality}" }} ]
                    }}
                }},
                execution: {{ language: "rust", run_cmd: ["panel"] }},
                interfaces: {{
                    services: {{ consumes: [ {{ link_id: "rec", name: "finish" }} ] }}
                }}
            }}"#
        ))
    }

    fn optional_dep_errors(
        consumer: &NodeConfig,
        policy: MissingDependencyPolicy,
        resolve: impl Fn(&str, &str) -> Option<NodeConfig>,
    ) -> Vec<ParsingError> {
        validate_dependency_specs(
            &consumer.manifest,
            &consumer.interfaces,
            "panel",
            "v1",
            policy,
            resolve,
        )
    }

    #[track_caller]
    fn assert_sole_missing_recorder(cardinality: &str, errors: &[ParsingError]) {
        assert_eq!(
            errors.len(),
            1,
            "`{cardinality}`: exactly the missing-node error: {errors:?}"
        );
        assert!(
            matches!(
                &errors[0],
                ParsingError::MissingDependency { dependency, .. } if dependency == "recorder"
            ),
            "`{cardinality}`: an absent dependency stays a missing node: {:?}",
            errors[0]
        );
    }

    /// The presence policy admits an absent dependency exactly where its slot
    /// admits the empty bound set, and the absence also mutes the exposure
    /// check for that slot's consumed items (nothing exists to check against;
    /// exposure is validated wherever the dependency does resolve).
    #[test]
    fn absent_dependency_is_admitted_only_where_the_slot_admits_empty() {
        let policy = MissingDependencyPolicy::AllowAbsentWhenSlotAdmitsEmpty;
        for cardinality in ["zero_or_more", "zero_or_one"] {
            let consumer = optional_dep_consumer(cardinality);
            let errors = optional_dep_errors(&consumer, policy, |_, _| None);
            assert!(
                errors.is_empty(),
                "an absent `{cardinality}` dependency is the admitted empty set: {errors:?}"
            );
        }
        for cardinality in ["one", "one_or_more"] {
            let consumer = optional_dep_consumer(cardinality);
            let errors = optional_dep_errors(&consumer, policy, |_, _| None);
            assert_sole_missing_recorder(cardinality, &errors);
        }
    }

    /// The build policy reports every absent dependency regardless of
    /// cardinality: codegen needs the dependency's interfaces even for a slot
    /// that may run empty.
    #[test]
    fn require_resolvable_reports_absent_dependencies_of_every_cardinality() {
        for cardinality in ["one", "zero_or_one", "one_or_more", "zero_or_more"] {
            let consumer = optional_dep_consumer(cardinality);
            let errors = optional_dep_errors(
                &consumer,
                MissingDependencyPolicy::RequireResolvable,
                |_, _| None,
            );
            assert_sole_missing_recorder(cardinality, &errors);
        }
    }

    /// An admitted-absent dependency still validates fully once it resolves:
    /// the policy only decides what absence means, never what presence must
    /// satisfy.
    #[test]
    fn admitted_absence_policy_still_validates_a_resolved_dependency() {
        let consumer = optional_dep_consumer("zero_or_more");
        let recorder_without_finish = unrelated();
        let errors = optional_dep_errors(
            &consumer,
            MissingDependencyPolicy::AllowAbsentWhenSlotAdmitsEmpty,
            |name, _| (name == "recorder").then(|| recorder_without_finish.clone()),
        );
        assert_eq!(errors.len(), 1, "errors: {errors:?}");
        assert!(
            matches!(&errors[0], ParsingError::MissingInterface(_)),
            "a resolved dependency missing the consumed service is still reported: {:?}",
            errors[0]
        );
    }
}
