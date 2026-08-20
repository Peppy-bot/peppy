use super::deps::{
    DependencyKind, DependencyLookupEntry, DependencyOfferings, build_dependency_lookup,
    build_dependency_offerings,
};
use crate::services::repo::cache as repo_cache;
use config::node::{
    ActionRefinement, InterfaceKind, LinkedMember, RefinementProblem, ServiceRefinement,
    TopicRefinement,
};
use config::{ContractCoverageMismatch, RefinementMismatch};
use daemon_config::consts::PeppyDirs;
use generator::{ConsumedActionMessage, ContractOrigin, DeploymentInterface};
use node_stack::NodeStack;
use std::collections::{HashMap, HashSet};

/// Collects consumed interfaces from a node config and resolves their message
/// formats by looking up the exposed interfaces from dependency nodes via the
/// caller-supplied resolver.
///
/// The `resolve` closure returns a [`config::node::NodeConfig`] for a given
/// `(name, tag)` pair, or `None` if the dependency cannot be found. Callers
/// usually wrap a [`NodeStack`] (see [`stack_resolver`]) but can also chain a
/// peer map first to resolve sibling nodes that haven't been added to the
/// stack yet; used by `node sync -a` for batch operations.
/// The full peppygen interface set for one node, in one call: consumed
/// interfaces from `depends_on`, contract-backed produced entries resolved
/// through `manifest.implements` (coverage-checked, sha-pinned,
/// drift-checked), and both directions of every declared pairing slot
/// (role-validated, sha-pinned, drift-checked). Used by `node add`,
/// `node sync`, and the launch-time auto-sync so the "what feeds peppygen"
/// sequence lives in one place. Error strings name the failing step; callers
/// only add their transport wrapper.
pub fn collect_all_deployment_interfaces(
    manifest: &config::node::Manifest,
    interfaces_cfg: &config::node::Interfaces,
    resolve: impl Fn(&str, &str) -> Option<config::node::NodeConfig>,
    peppy_dirs: &PeppyDirs,
    doc_pins: Option<&crate::services::node::pins::DocPins>,
    on_feedback: &dyn Fn(&str),
) -> std::result::Result<Vec<DeploymentInterface>, String> {
    let mut interfaces = collect_consumed_interfaces(
        manifest,
        interfaces_cfg,
        resolve,
        peppy_dirs,
        doc_pins,
        on_feedback,
    )
    .map_err(|reason| format!("failed to resolve consumed interfaces: {reason}"))?;
    interfaces.extend(
        resolve_implements(manifest, interfaces_cfg, peppy_dirs, doc_pins, on_feedback)
            .map_err(|reason| format!("failed to resolve `manifest.implements`: {reason}"))?,
    );
    interfaces.extend(
        super::pairings::collect_pairing_interfaces(
            manifest,
            interfaces_cfg,
            peppy_dirs,
            doc_pins,
            on_feedback,
        )
        .map_err(|reason| format!("failed to resolve pairing slots: {reason}"))?,
    );
    Ok(interfaces)
}

/// The pairing slot link_ids of a manifest, participants and observers alike.
/// Entries naming one
/// are resolved by `collect_pairing_interfaces` against the pairing document
/// and generated under `paired_topics/<link_id>/<topic>`, so both the consumed
/// collector and the implements resolver must step over them: neither knows
/// the pairing kind, and collecting an entry twice would either drop it
/// silently or land it in the wrong module category.
fn pairing_slot_link_ids(manifest: &config::node::Manifest) -> HashSet<&str> {
    manifest
        .depends_on
        .iter()
        .flat_map(|d| d.pairing_link_ids())
        .collect()
}

/// The dependency tables every consumed-interface lookup resolves against,
/// built once per `collect_consumed_interfaces` call.
struct ResolvedDependencies {
    lookup: HashMap<String, DependencyLookupEntry>,
    /// Native emits/exposes per node dependency, keyed by `(name, tag)`.
    /// Contract-backed entries are deliberately absent: node dependencies
    /// expose native interfaces only, and contract-backed interfaces are
    /// consumed through `depends_on.contracts`.
    node_offerings: HashMap<(String, String), DependencyOfferings>,
    /// Memoized parsed contracts for `depends_on.contracts` entries, keyed by
    /// `link_id` so two entries with the same `(name, tag)` but different
    /// sha256 pins are cached and resolved separately. `resolve_contract_doc`
    /// handles SHA-pin matching and on-disk drift detection per load.
    contract_docs: HashMap<String, daemon_config::contract::PeppyContract>,
}

pub fn collect_consumed_interfaces(
    manifest: &config::node::Manifest,
    interfaces_cfg: &config::node::Interfaces,
    resolve: impl Fn(&str, &str) -> Option<config::node::NodeConfig>,
    peppy_dirs: &PeppyDirs,
    doc_pins: Option<&crate::services::node::pins::DocPins>,
    on_feedback: &dyn Fn(&str),
) -> std::result::Result<Vec<DeploymentInterface>, String> {
    let mut interfaces = Vec::new();
    let mut deps = ResolvedDependencies {
        lookup: build_dependency_lookup(manifest),
        node_offerings: HashMap::new(),
        contract_docs: HashMap::new(),
    };

    // Pre-resolve each unique dependency into its offerings table / contract
    // document, so the per-entry lookups below are pure map reads.
    for (link_id, entry) in deps.lookup.iter() {
        match entry.kind {
            DependencyKind::Node => {
                let node_key = (entry.name.clone(), entry.tag.clone());
                if deps.node_offerings.contains_key(&node_key) {
                    continue;
                }
                let Some(dep_config) = resolve(&entry.name, &entry.tag) else {
                    continue;
                };
                deps.node_offerings
                    .insert(node_key, build_dependency_offerings(&dep_config));
            }
            DependencyKind::Contract => {
                if deps.contract_docs.contains_key(link_id) {
                    continue;
                }
                let parsed = resolve_contract_doc(
                    peppy_dirs,
                    &entry.name,
                    &entry.tag,
                    entry.sha256.as_deref(),
                    doc_pins,
                    on_feedback,
                )?;
                deps.contract_docs.insert(link_id.clone(), parsed);
            }
        }
    }

    let pairing_slots = pairing_slot_link_ids(manifest);

    if let Some(topic_interfaces) = &interfaces_cfg.topics
        && let Some(consumed_topics) = &topic_interfaces.consumes
    {
        for consumed_topic in consumed_topics {
            // Topics are the one consumed section a pairing slot may appear
            // in; services and actions are rejected at parse time.
            if pairing_slots.contains(consumed_topic.link_id.as_str()) {
                continue;
            }
            let Some((message_format, dependency)) = deps.resolve_consumed_offering(
                config::node::InterfaceKind::Topic.consumed_section(),
                &consumed_topic.link_id,
                consumed_topic.name.trim(),
                |offerings, name| offerings.topics.get(name).cloned(),
                // A contract topic declared without a `message_format`
                // defaults like a contract service does, so a `None` here
                // means the name is absent from the document and nothing
                // else.
                |parsed, name| {
                    let emitted = parsed
                        .interfaces
                        .topics
                        .iter()
                        .find(|t| t.name.trim() == name)?;
                    Some(
                        refine_member(
                            consumed_topic.refine.as_ref(),
                            emitted.clone(),
                            TopicRefinement::apply,
                        )
                        .map(|emitted| emitted.message_format.unwrap_or_default()),
                    )
                },
            )?
            else {
                continue;
            };
            interfaces.push(DeploymentInterface::consumed_topic(
                consumed_topic.clone(),
                message_format,
                dependency,
            ));
        }
    }

    if let Some(service_interfaces) = &interfaces_cfg.services
        && let Some(consumed_services) = &service_interfaces.consumes
    {
        for consumed_service in consumed_services {
            let Some(((request_format, response_format), dependency)) = deps
                .resolve_consumed_offering(
                    config::node::InterfaceKind::Service.consumed_section(),
                    &consumed_service.link_id,
                    consumed_service.name.trim(),
                    |offerings, name| offerings.services.get(name).cloned(),
                    |parsed, name| {
                        let exposed = parsed
                            .interfaces
                            .services
                            .iter()
                            .find(|s| s.name.trim() == name)?;
                        Some(
                            refine_member(
                                consumed_service.refine.as_ref(),
                                exposed.clone(),
                                ServiceRefinement::apply,
                            )
                            .map(|exposed| {
                                (
                                    exposed.request_message_format.unwrap_or_default(),
                                    exposed.response_message_format.unwrap_or_default(),
                                )
                            }),
                        )
                    },
                )?
            else {
                continue;
            };
            interfaces.push(DeploymentInterface::consumed_service(
                consumed_service.clone(),
                request_format,
                response_format,
                dependency,
            ));
        }
    }

    if let Some(action_interfaces) = &interfaces_cfg.actions
        && let Some(consumed_actions) = &action_interfaces.consumes
    {
        for consumed_action in consumed_actions {
            let Some((action_message, dependency)) = deps.resolve_consumed_offering(
                config::node::InterfaceKind::Action.consumed_section(),
                &consumed_action.link_id,
                consumed_action.name.trim(),
                |offerings, name| offerings.actions.get(name).cloned(),
                |parsed, name| {
                    let exposed = parsed
                        .interfaces
                        .actions
                        .iter()
                        .find(|a| a.name.trim() == name)?;
                    Some(
                        refine_member(
                            consumed_action.refine.as_deref(),
                            exposed.clone(),
                            ActionRefinement::apply,
                        )
                        .map(|exposed| action_message_from_exposed(&exposed)),
                    )
                },
            )?
            else {
                continue;
            };
            interfaces.push(DeploymentInterface::consumed_action(
                consumed_action.clone(),
                action_message,
                dependency,
            ));
        }
    }

    Ok(interfaces)
}

/// The document member as the entry wants it: the entry's `refine` block
/// applied when it carries one, the member untouched otherwise. An `Err`
/// lists every pin the member does not admit.
fn refine_member<M, R>(
    refinement: Option<&R>,
    member: M,
    apply: impl FnOnce(&R, M) -> Result<M, Vec<RefinementProblem>>,
) -> Result<M, Vec<RefinementProblem>> {
    match refinement {
        Some(refinement) => apply(refinement, member),
        None => Ok(member),
    }
}

/// The `document` label a [`RefinementMismatch`] carries for a contract slot.
fn contract_label(name: &str, tag: &str) -> String {
    format!("contract `{name}:{tag}`")
}

impl ResolvedDependencies {
    /// Resolves a single consumed interface to its message-format payload plus
    /// the `DependencyContext` the generator needs to address it. Node
    /// dependencies resolve against the producer's native offerings only
    /// (node-addressed); contract dependencies resolve against the contract
    /// document (contract-addressed).
    ///
    /// `Ok(None)` means the entry is not this collector's to report: an
    /// undeclared link_id, an unresolved node dependency, and a name the
    /// producer does not natively offer are all reported by
    /// `validate_dependency_specs` upstream, which sees node dependencies and
    /// would otherwise report each twice.
    ///
    /// A name absent from a *contract* document has no upstream reporter at
    /// all: `validate_dependency_specs` stops at the declaration check for
    /// `depends_on.contracts` link_ids, and no layer below this one can open a
    /// contract document. So it errors here, or nowhere. The same goes for a
    /// `refine` block the member does not admit, which `extract_from_contract`
    /// reports as its `Err`.
    fn resolve_consumed_offering<T>(
        &self,
        section: &str,
        link_id: &str,
        lookup_name: &str,
        extract_from_node: impl FnOnce(&DependencyOfferings, &str) -> Option<T>,
        extract_from_contract: impl FnOnce(
            &daemon_config::contract::PeppyContract,
            &str,
        ) -> Option<Result<T, Vec<RefinementProblem>>>,
    ) -> std::result::Result<Option<(T, generator::DependencyContext)>, String> {
        let Some(entry) = self.lookup.get(link_id) else {
            return Ok(None);
        };
        match entry.kind {
            DependencyKind::Node => {
                let Some(offerings) = self
                    .node_offerings
                    .get(&(entry.name.clone(), entry.tag.clone()))
                else {
                    return Ok(None);
                };
                let Some(extracted) = extract_from_node(offerings, lookup_name) else {
                    return Ok(None);
                };
                Ok(Some((
                    extracted,
                    generator::DependencyContext::native(
                        &entry.name,
                        &entry.tag,
                        link_id,
                        entry.cardinality,
                    ),
                )))
            }
            DependencyKind::Contract => {
                let parsed = self.contract_docs.get(link_id).ok_or_else(|| {
                    format!(
                        "`{section}` entry `{lookup_name}` references link_id `{link_id}`, \
                         whose contract `{}:{}` failed to resolve",
                        entry.name, entry.tag
                    )
                })?;
                let extracted = extract_from_contract(parsed, lookup_name)
                    .ok_or_else(|| {
                        format!(
                            "`{section}` entry `{lookup_name}` (link_id `{link_id}`) names no \
                             member of contract `{}:{}`",
                            entry.name, entry.tag
                        )
                    })?
                    .map_err(|problems| {
                        RefinementMismatch {
                            document: contract_label(&entry.name, &entry.tag),
                            link_id: link_id.to_string(),
                            name: lookup_name.to_string(),
                            problems,
                        }
                        .to_string()
                    })?;
                Ok(Some((
                    extracted,
                    generator::DependencyContext::contract(
                        &entry.name,
                        &entry.tag,
                        link_id,
                        entry.cardinality,
                    ),
                )))
            }
        }
    }
}

pub(super) fn action_message_from_exposed(
    exposed_action: &config::node::NativeExposedAction,
) -> ConsumedActionMessage {
    ConsumedActionMessage::from(exposed_action)
}

#[cfg(test)]
mod action_message_tests {
    use super::*;

    #[test]
    fn preserves_the_declared_goal_response_format_exactly() {
        let exposed_action: config::node::NativeExposedAction = serde_json5::from_str(
            r#"{
                name: "move_arm",
                goal_service: {
                    response_message_format: {
                        accepted: "bool"
                    }
                }
            }"#,
        )
        .expect("action should parse");
        let declared = exposed_action
            .goal_service
            .as_ref()
            .and_then(|service| service.response_message_format.clone());

        let messages = action_message_from_exposed(&exposed_action);

        assert_eq!(messages.goal_response, declared);
        assert_eq!(
            messages
                .goal_response
                .as_ref()
                .expect("declared response should be preserved")
                .0
                .len(),
            1,
            "the consumed response should contain exactly the declared field"
        );
    }
}

/// Loads a `PeppyContract` document from the local contract cache for
/// `(name, tag)`, verifying both the SHA pin (when set) and on-disk drift
/// against the cached fingerprint. Returns the parsed contract document,
/// or an error string ready to surface to the client. Shared between
/// [`resolve_implements`] (producer side) and the `depends_on.contracts`
/// resolution path (consumer side).
pub(crate) fn resolve_contract_doc(
    peppy_dirs: &PeppyDirs,
    name: &str,
    tag: &str,
    sha256_pin: Option<&str>,
    doc_pins: Option<&crate::services::node::pins::DocPins>,
    on_feedback: &dyn Fn(&str),
) -> std::result::Result<daemon_config::contract::PeppyContract, String> {
    let cache = repo_cache::load_contract_cache(peppy_dirs)
        .map_err(|e| format!("failed to load contract cache: {e}"))?;
    let parse = |content: &str| {
        daemon_config::contract::PeppyContractParser::from_content(content)
            .map_err(|e| e.to_string())
    };

    // A launch pin fixes the bytes: this machine reuses its own copy on a
    // content match and fetches the pin's origin otherwise, but its own
    // priority rules never pick the document. Without one, the local rules
    // apply, with the manifest's own optional sha pin.
    if let Some(pins) = doc_pins {
        let pin = pins.require(
            daemon_config::repository::PinKind::Contract,
            name,
            tag,
            sha256_pin,
        )?;
        return repo_cache::resolve_pinned_doc(peppy_dirs, &cache, pin, parse, on_feedback);
    }
    repo_cache::resolve_cached_doc(
        peppy_dirs,
        &cache,
        name,
        tag,
        sha256_pin,
        parse,
        on_feedback,
    )
}

/// Error from [`resolve_implements`]. Tier B coverage failures keep their
/// per-slot [`ContractCoverageMismatch`] payloads and refinement failures
/// their per-entry [`RefinementMismatch`] payloads, so callers can render or
/// match each diff individually; every other failure is a plain message,
/// matching the sync module's string error contract.
#[derive(Debug)]
pub enum ImplementsError {
    /// One aggregated set-diff per broken slot.
    Coverage(Vec<ContractCoverageMismatch>),
    /// One aggregated problem list per entry whose `refine` block the
    /// contract does not admit. Reported only once coverage is clean.
    Refinement(Vec<RefinementMismatch>),
    Other(String),
}

impl std::fmt::Display for ImplementsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Coverage(mismatches) => join_mismatches(f, mismatches),
            Self::Refinement(mismatches) => join_mismatches(f, mismatches),
            Self::Other(message) => f.write_str(message),
        }
    }
}

/// Renders aggregated mismatches as one `; `-separated line.
fn join_mismatches<M: std::fmt::Display>(
    f: &mut std::fmt::Formatter<'_>,
    mismatches: &[M],
) -> std::fmt::Result {
    for (idx, mismatch) in mismatches.iter().enumerate() {
        if idx > 0 {
            f.write_str("; ")?;
        }
        write!(f, "{mismatch}")?;
    }
    Ok(())
}

impl std::error::Error for ImplementsError {}

impl From<String> for ImplementsError {
    fn from(message: String) -> Self {
        Self::Other(message)
    }
}

/// Per-slot Tier B coverage bookkeeping: which contract members the manifest
/// entries visited (with counts), plus names that matched no member of the
/// right kind.
#[derive(Default)]
struct SlotCoverage {
    visited: HashMap<InterfaceKind, HashMap<String, u32>>,
    unknown: Vec<String>,
    wrong_kind: Vec<String>,
}

/// Resolves the node's contract-backed produced entries (Decision 1:
/// entries drive codegen). For every `{link_id, name}` entry in
/// `topics.emits` / `services.exposes` / `actions.exposes`:
///
/// entry -> implements slot -> contract document -> member (by name and
/// kind) -> shape/qos with the entry's `refine` block applied, stamped with
/// a [`ContractOrigin`] so the generator nests the artifact under
/// `{link_id}/{leaf}` and embeds the matching wire segments.
///
/// After resolution, the Tier B coverage check runs per (slot x kind): the
/// contract-backed entries referencing a slot must cover every member of
/// its contract exactly once, with no extras. Any discrepancy is reported
/// as one aggregated set-diff per broken slot
/// ([`ContractCoverageMismatch`]). An entry whose `refine` block the member
/// does not admit still counts toward coverage (the member exists); once
/// coverage is clean, those entries are reported together, one
/// [`RefinementMismatch`] each listing every inadmissible pin.
///
/// Contract documents resolve through the local cache with sha256 pinning
/// and on-disk drift detection (see [`resolve_contract_doc`]); a cache miss
/// surfaces "run `peppy repo refresh`".
pub fn resolve_implements(
    manifest: &config::node::Manifest,
    interfaces_cfg: &config::node::Interfaces,
    peppy_dirs: &PeppyDirs,
    doc_pins: Option<&crate::services::node::pins::DocPins>,
    on_feedback: &dyn Fn(&str),
) -> std::result::Result<Vec<DeploymentInterface>, ImplementsError> {
    if manifest.implements.is_empty() {
        return Ok(Vec::new());
    }

    // Resolve each slot's contract document once, keeping the slot alongside
    // so entry resolution recovers both with one lookup. Parse-time validation
    // already rejected duplicate link_ids and duplicate (name, tag) pairs.
    let mut docs: HashMap<
        &str,
        (
            &config::node::ImplementsEntry,
            daemon_config::contract::PeppyContract,
        ),
    > = HashMap::new();
    for slot in &manifest.implements {
        let parsed = resolve_contract_doc(
            peppy_dirs,
            slot.name.as_str(),
            &slot.tag,
            slot.sha256.as_deref(),
            doc_pins,
            on_feedback,
        )?;
        docs.insert(slot.link_id.as_str(), (slot, parsed));
    }

    let mut out: Vec<DeploymentInterface> = Vec::new();
    let mut coverage: HashMap<&str, SlotCoverage> = HashMap::new();
    let mut inadmissible: Vec<RefinementMismatch> = Vec::new();
    let pairing_slots = pairing_slot_link_ids(manifest);

    for member in interfaces_cfg.linked_entries() {
        // A pairing-backed emit is resolved against the pairing document by
        // `collect_pairing_interfaces`. It must not count toward any
        // implements slot's coverage, nor trip the unknown-link_id arm below.
        if pairing_slots.contains(member.link_id()) {
            continue;
        }
        let kind = member.kind();
        let name = member.name();
        let Some((slot, doc)) = docs.get(member.link_id()) else {
            // Parse-time validation guarantees every produced entry's
            // link_id names an implements slot; reaching this means the
            // config bypassed the parser.
            return Err(ImplementsError::Other(format!(
                "produced entry `{name}` references link_id `{}`, which matches no \
                 `manifest.implements` slot",
                member.link_id()
            )));
        };
        let origin = ContractOrigin {
            link_id: slot.link_id.as_str().to_string(),
            contract_name: slot.name.as_str().to_string(),
            contract_tag: slot.tag.clone(),
        };
        let slot_coverage = coverage.entry(slot.link_id.as_str()).or_default();

        // `None`: no member of this kind by this name. `Some(Err)`: the
        // member exists but does not admit the entry's `refine` block.
        let resolved = match member {
            LinkedMember::Topic(entry) => doc
                .interfaces
                .topics
                .iter()
                .find(|t| t.name == name)
                .map(|topic| {
                    refine_member(entry.refine.as_ref(), topic.clone(), TopicRefinement::apply)
                        .map(|topic| DeploymentInterface::emitted_topic(topic, Some(origin)))
                }),
            LinkedMember::Service(entry) => doc
                .interfaces
                .services
                .iter()
                .find(|s| s.name == name)
                .map(|service| {
                    refine_member(
                        entry.refine.as_ref(),
                        service.clone(),
                        ServiceRefinement::apply,
                    )
                    .map(|service| DeploymentInterface::exposed_service(service, Some(origin)))
                }),
            LinkedMember::Action(entry) => doc
                .interfaces
                .actions
                .iter()
                .find(|a| a.name == name)
                .map(|action| {
                    refine_member(
                        entry.refine.as_deref(),
                        action.clone(),
                        ActionRefinement::apply,
                    )
                    .map(|action| DeploymentInterface::exposed_action(action, Some(origin)))
                }),
        };

        match resolved {
            Some(refined) => {
                *slot_coverage
                    .visited
                    .entry(kind)
                    .or_default()
                    .entry(name.to_string())
                    .or_insert(0) += 1;
                match refined {
                    Ok(interface) => out.push(interface),
                    Err(problems) => inadmissible.push(RefinementMismatch {
                        document: contract_label(slot.name.as_str(), &slot.tag),
                        link_id: slot.link_id.clone(),
                        name: name.to_string(),
                        problems,
                    }),
                }
            }
            None => {
                // The entry's name was not found under `kind`, so any kind
                // this returns is necessarily a different one.
                match member_kind(doc, name) {
                    Some(member_kind) => slot_coverage.wrong_kind.push(format!(
                        "{name} (declared as {kind}, contract declares {member_kind})"
                    )),
                    None => slot_coverage.unknown.push(name.to_string()),
                }
            }
        }
    }

    // Tier B coverage: per (slot x kind) set equality between the contract's
    // members and the visited entries, aggregated into one diff per slot.
    let mut broken: Vec<ContractCoverageMismatch> = Vec::new();
    for slot in &manifest.implements {
        let (_, doc) = &docs[slot.link_id.as_str()];
        let empty = SlotCoverage::default();
        let slot_coverage = coverage.get(slot.link_id.as_str()).unwrap_or(&empty);

        let mut missing: Vec<String> = Vec::new();
        let mut duplicated: Vec<String> = Vec::new();
        let members = doc
            .interfaces
            .topics
            .iter()
            .map(|t| (InterfaceKind::Topic, t.name.as_str()))
            .chain(
                doc.interfaces
                    .services
                    .iter()
                    .map(|s| (InterfaceKind::Service, s.name.as_str())),
            )
            .chain(
                doc.interfaces
                    .actions
                    .iter()
                    .map(|a| (InterfaceKind::Action, a.name.as_str())),
            );
        for (kind, member_name) in members {
            let visits = slot_coverage
                .visited
                .get(&kind)
                .and_then(|members| members.get(member_name))
                .copied()
                .unwrap_or(0);
            match visits {
                0 => missing.push(format!("{member_name} ({kind})")),
                1 => {}
                _ => duplicated.push(format!("{member_name} ({kind})")),
            }
        }

        if missing.is_empty()
            && duplicated.is_empty()
            && slot_coverage.unknown.is_empty()
            && slot_coverage.wrong_kind.is_empty()
        {
            continue;
        }
        broken.push(ContractCoverageMismatch {
            contract_name: slot.name.as_str().to_string(),
            contract_tag: slot.tag.clone(),
            link_id: slot.link_id.clone(),
            missing,
            unknown: slot_coverage.unknown.clone(),
            duplicated,
            wrong_kind: slot_coverage.wrong_kind.clone(),
        });
    }
    if !broken.is_empty() {
        return Err(ImplementsError::Coverage(broken));
    }
    if !inadmissible.is_empty() {
        return Err(ImplementsError::Refinement(inadmissible));
    }

    Ok(out)
}

/// The first kind under which a contract declares a member named `name`.
fn member_kind(doc: &daemon_config::contract::PeppyContract, name: &str) -> Option<InterfaceKind> {
    if doc.interfaces.topics.iter().any(|t| t.name == name) {
        Some(InterfaceKind::Topic)
    } else if doc.interfaces.services.iter().any(|s| s.name == name) {
        Some(InterfaceKind::Service)
    } else if doc.interfaces.actions.iter().any(|a| a.name == name) {
        Some(InterfaceKind::Action)
    } else {
        None
    }
}

/// Convenience helper that builds a resolver closure backed by a [`NodeStack`].
///
/// Use this for callers that don't have any local peers to layer on top of
/// the daemon's persistent stack, i.e. `node add` and `auto_sync_if_missing`.
pub fn stack_resolver(
    node_stack: &NodeStack,
) -> impl Fn(&str, &str) -> Option<config::node::NodeConfig> + '_ {
    move |name, tag| {
        node_stack
            .find(name, tag)
            .map(|e| e.read().config().clone())
    }
}

/// Two-tier resolver: the node stack first, then the repository-materialized
/// configs a [`super::deps::materialize_repo_deps`] call produced. Stack
/// lookups always win; the one stack-then-cache precedence rule, shared by
/// `node sync` and `node add`.
pub(crate) fn stack_then_repo_resolver<'a>(
    node_stack: &'a NodeStack,
    repo_resolved: &'a std::collections::HashMap<(String, String), config::node::NodeConfig>,
) -> impl Fn(&str, &str) -> Option<config::node::NodeConfig> + 'a {
    let stack = stack_resolver(node_stack);
    move |name, tag| {
        stack(name, tag).or_else(|| {
            repo_resolved
                .get(&(name.to_owned(), tag.to_owned()))
                .cloned()
        })
    }
}

#[cfg(test)]
mod implements_tests {
    //! Exercises [`resolve_implements`]: the cache-loading, entry-driven
    //! resolution and Tier B coverage side of `manifest.implements`. The
    //! generator side (module nesting / wire-segment embedding) is verified
    //! by the integration tests in
    //! `crates/generator-internal/tests/{rust,python}/`.

    use super::*;
    use config::node::{Interfaces, Manifest};
    use generator::InterfaceVariant;
    use std::fs;
    use tempfile::TempDir;

    /// Writes a contract manifest to `dir/{name}_{tag}.json5` and seeds the
    /// returned `ContractCacheEntry` with the matching sha256. Returns
    /// `(entry, abs_path)` so callers can either keep or mutate the entry
    /// (e.g. for the drift test).
    fn seed_contract(
        dir: &std::path::Path,
        name: &str,
        tag: &str,
        body: &str,
    ) -> repo_cache::ContractCacheEntry {
        let file_name = format!("{name}_{tag}.json5");
        let path = dir.join(&file_name);
        fs::write(&path, body).expect("write contract file");
        repo_cache::ContractCacheEntry {
            contract_name: daemon_config::repository::ItemName::parse(name)
                .expect("test name is valid"),
            tag: daemon_config::repository::ItemTag::parse(tag).expect("test tag is valid"),
            sha256: daemon_config::repository::ManifestFingerprint::of_bytes(body.as_bytes()),
            origin: repo_cache::EntryOrigin::Fs { path: path.clone() },
            repo_id: 0,
        }
    }

    /// Builds a contracts.json5 cache + dir-rooted `PeppyDirs` from a set of
    /// seeded entries.
    fn make_peppy_dirs_with_cache(
        entries: &[repo_cache::ContractCacheEntry],
    ) -> (TempDir, PeppyDirs) {
        let tmp = TempDir::new().expect("temp dir");
        let dirs = PeppyDirs::new(tmp.path().to_path_buf());
        fs::create_dir_all(dirs.cache_dir()).expect("create cache dir");
        let cache_path = repo_cache::contracts_repo_cache_path(&dirs);
        let json = serde_json5::to_string(&entries.to_vec()).expect("serialize cache");
        fs::write(&cache_path, json).expect("write cache file");
        (tmp, dirs)
    }

    const DEPTH_V1_BODY: &str = r#"{
        peppy_schema: "contract/v1",
        manifest: { name: "depth_camera", tag: "v1" },
        interfaces: {
            topics: [
                { name: "video_stream", qos_profile: "sensor_data" }
            ]
        }
    }"#;

    fn manifest_with_implements(implements: &str) -> Manifest {
        serde_json5::from_str(&format!(
            r#"{{ name: "camera_node", tag: "v1", implements: [{implements}] }}"#
        ))
        .expect("manifest parses")
    }

    fn interfaces_from(json5: &str) -> Interfaces {
        serde_json5::from_str(json5).expect("interfaces parses")
    }

    #[test]
    fn returns_empty_when_no_implements() {
        let dirs = PeppyDirs::new(TempDir::new().unwrap().path().to_path_buf());
        let manifest: Manifest = serde_json5::from_str(r#"{ name: "plain", tag: "v1" }"#).unwrap();
        let out = resolve_implements(&manifest, &Interfaces::default(), &dirs, None, &|_| {})
            .expect("ok");
        assert!(out.is_empty());
    }

    /// Happy path: a full-coverage manifest yields one DeploymentInterface
    /// per contract-backed entry, each carrying the member's shape from the
    /// contract document and a `ContractOrigin` pointing back at the source
    /// contract.
    #[test]
    fn resolves_full_coverage_with_origin() {
        let tmp = TempDir::new().unwrap();
        let entry = seed_contract(tmp.path(), "depth_camera", "v1", DEPTH_V1_BODY);
        let (_tmp_dirs, dirs) = make_peppy_dirs_with_cache(&[entry]);

        let manifest =
            manifest_with_implements(r#"{ name: "depth_camera", tag: "v1", link_id: "cam" }"#);
        let interfaces =
            interfaces_from(r#"{ topics: { emits: [{ link_id: "cam", name: "video_stream" }] } }"#);

        let out =
            resolve_implements(&manifest, &interfaces, &dirs, None, &|_| {}).expect("happy path");
        assert_eq!(out.len(), 1, "should pull the one video_stream topic");
        match out[0].interface() {
            InterfaceVariant::EmittedTopic {
                topic,
                origin: Some(o),
            } => {
                assert_eq!(topic.name, "video_stream");
                assert_eq!(
                    topic.qos_profile,
                    config::node::QoSProfile::SensorData,
                    "shape and qos come from the contract document"
                );
                assert_eq!(o.contract_name, "depth_camera");
                assert_eq!(o.contract_tag, "v1");
            }
            other => panic!("expected EmittedTopic with origin, got {other:?}"),
        }
    }

    #[test]
    fn partial_coverage_rejected_with_aggregated_diff() {
        const UVC_BODY: &str = r#"{
            peppy_schema: "contract/v1",
            manifest: { name: "uvc_camera", tag: "v1" },
            interfaces: {
                topics: [ { name: "video_stream" } ],
                services: [ { name: "video_stream_info" }, { name: "set_contrast" } ]
            }
        }"#;
        let tmp = TempDir::new().unwrap();
        let entry = seed_contract(tmp.path(), "uvc_camera", "v1", UVC_BODY);
        let (_tmp_dirs, dirs) = make_peppy_dirs_with_cache(&[entry]);

        let manifest =
            manifest_with_implements(r#"{ name: "uvc_camera", tag: "v1", link_id: "cam" }"#);
        // Missing both services; carries one unknown extra.
        let interfaces = interfaces_from(
            r#"{
                topics: { emits: [
                    { link_id: "cam", name: "video_stream" },
                    { link_id: "cam", name: "not_in_contract" },
                ] },
            }"#,
        );

        let err = resolve_implements(&manifest, &interfaces, &dirs, None, &|_| {})
            .expect_err("partial coverage must error");
        let ImplementsError::Coverage(mismatches) = &err else {
            panic!("coverage failure must carry structured mismatches, got {err:?}");
        };
        assert_eq!(mismatches.len(), 1, "one broken slot, one mismatch");
        let err = err.to_string();
        assert!(
            err.contains("uvc_camera:v1") && err.contains("cam"),
            "error should name the contract and slot, got: {err}"
        );
        assert!(
            err.contains("video_stream_info") && err.contains("set_contrast"),
            "error should list every missing member at once, got: {err}"
        );
        assert!(
            err.contains("not_in_contract"),
            "error should list unknown names, got: {err}"
        );
    }

    #[test]
    fn wrong_kind_entry_reported_in_diff() {
        let tmp = TempDir::new().unwrap();
        let entry = seed_contract(tmp.path(), "depth_camera", "v1", DEPTH_V1_BODY);
        let (_tmp_dirs, dirs) = make_peppy_dirs_with_cache(&[entry]);

        let manifest =
            manifest_with_implements(r#"{ name: "depth_camera", tag: "v1", link_id: "cam" }"#);
        // The contract's one topic listed under services.exposes.
        let interfaces = interfaces_from(
            r#"{ services: { exposes: [{ link_id: "cam", name: "video_stream" }] } }"#,
        );

        let err = resolve_implements(&manifest, &interfaces, &dirs, None, &|_| {})
            .expect_err("wrong-kind entry must error")
            .to_string();
        assert!(
            err.contains("video_stream") && err.contains("wrong kind"),
            "error should flag the wrong-kind entry, got: {err}"
        );
        assert!(
            err.contains("declared as service, contract declares topic"),
            "error should say which kinds are involved, got: {err}"
        );
    }

    /// One complete slot + one broken slot: the error names only the broken
    /// one.
    #[test]
    fn multi_slot_error_names_only_the_broken_slot() {
        const IMU_BODY: &str = r#"{
            peppy_schema: "contract/v1",
            manifest: { name: "imu", tag: "v1" },
            interfaces: { topics: [ { name: "orientation" } ] }
        }"#;
        let tmp = TempDir::new().unwrap();
        let depth = seed_contract(tmp.path(), "depth_camera", "v1", DEPTH_V1_BODY);
        let imu = seed_contract(tmp.path(), "imu", "v1", IMU_BODY);
        let (_tmp_dirs, dirs) = make_peppy_dirs_with_cache(&[depth, imu]);

        let manifest = manifest_with_implements(
            r#"{ name: "depth_camera", tag: "v1", link_id: "cam" },
               { name: "imu", tag: "v1", link_id: "motion" }"#,
        );
        // cam fully covered; motion missing its one topic.
        let interfaces =
            interfaces_from(r#"{ topics: { emits: [{ link_id: "cam", name: "video_stream" }] } }"#);

        let err = resolve_implements(&manifest, &interfaces, &dirs, None, &|_| {})
            .expect_err("broken slot must error")
            .to_string();
        assert!(
            err.contains("imu:v1") && err.contains("motion") && err.contains("orientation"),
            "error should name the broken slot and its missing member, got: {err}"
        );
        assert!(
            !err.contains("depth_camera:v1"),
            "the complete slot must not be flagged, got: {err}"
        );
    }

    /// A zero-member contract with zero entries is degenerately
    /// coverage-complete.
    #[test]
    fn zero_member_contract_with_zero_entries_passes() {
        const EMPTY_BODY: &str = r#"{
            peppy_schema: "contract/v1",
            manifest: { name: "empty_contract", tag: "v1" },
            interfaces: {}
        }"#;
        let tmp = TempDir::new().unwrap();
        let entry = seed_contract(tmp.path(), "empty_contract", "v1", EMPTY_BODY);
        let (_tmp_dirs, dirs) = make_peppy_dirs_with_cache(&[entry]);

        let manifest =
            manifest_with_implements(r#"{ name: "empty_contract", tag: "v1", link_id: "noop" }"#);
        let out = resolve_implements(&manifest, &Interfaces::default(), &dirs, None, &|_| {})
            .expect("degenerate coverage passes");
        assert!(out.is_empty());
    }

    #[test]
    fn cache_miss_suggests_repo_refresh() {
        // Empty cache; any lookup misses.
        let (_tmp_dirs, dirs) = make_peppy_dirs_with_cache(&[]);
        let manifest =
            manifest_with_implements(r#"{ name: "depth_camera", tag: "v1", link_id: "cam" }"#);

        let err = resolve_implements(&manifest, &Interfaces::default(), &dirs, None, &|_| {})
            .expect_err("miss must error")
            .to_string();
        assert!(
            err.contains("`depth_camera:v1`") && err.contains("peppy repo refresh"),
            "missing-from-cache error should name the entry and suggest refresh, got: {err}"
        );
    }

    /// Services and actions resolve through the same entry-driven path with
    /// origins stamped, including the (not-yet-used-in-production)
    /// contract-declared action shape.
    #[test]
    fn resolves_service_and_action_members_with_origin() {
        const ARM_BODY: &str = r#"{
            peppy_schema: "contract/v1",
            manifest: { name: "arm", tag: "v1" },
            interfaces: {
                services: [ { name: "control" } ],
                actions: [ { name: "move_arm" } ]
            }
        }"#;
        let tmp = TempDir::new().unwrap();
        let entry = seed_contract(tmp.path(), "arm", "v1", ARM_BODY);
        let (_tmp_dirs, dirs) = make_peppy_dirs_with_cache(&[entry]);

        let manifest = manifest_with_implements(r#"{ name: "arm", tag: "v1", link_id: "arm" }"#);
        let interfaces = interfaces_from(
            r#"{
                services: { exposes: [{ link_id: "arm", name: "control" }] },
                actions: { exposes: [{ link_id: "arm", name: "move_arm" }] },
            }"#,
        );

        let out =
            resolve_implements(&manifest, &interfaces, &dirs, None, &|_| {}).expect("happy path");

        let mut saw_service = false;
        let mut saw_action = false;
        for entry in &out {
            match entry.interface() {
                InterfaceVariant::ExposedService {
                    service,
                    origin: Some(o),
                } => {
                    assert_eq!(service.name, "control");
                    assert_eq!(o.contract_name, "arm");
                    assert_eq!(o.contract_tag, "v1");
                    saw_service = true;
                }
                InterfaceVariant::ExposedAction {
                    action,
                    origin: Some(o),
                } => {
                    assert_eq!(action.name, "move_arm");
                    assert_eq!(o.contract_name, "arm");
                    assert_eq!(o.contract_tag, "v1");
                    saw_action = true;
                }
                other => panic!("unexpected resolved variant: {other:?}"),
            }
        }
        assert!(saw_service, "service should be resolved with origin");
        assert!(saw_action, "action should be resolved with origin");
    }

    /// A contract keeping its joint vectors generic, with one array it pins
    /// itself (`position`), so tests can tell a pin that lands from one the
    /// document refuses.
    const LIMB_MOTION_V1_BODY: &str = r#"{
        peppy_schema: "contract/v1",
        manifest: { name: "limb_motion", tag: "v1" },
        interfaces: {
            topics: [
                {
                    name: "joint_states",
                    message_format: { positions: { $type: "array", $items: "f64" } }
                }
            ],
            actions: [
                {
                    name: "move_arm_joints",
                    goal_service: {
                        request_message_format: {
                            arm_id: "u8",
                            joint_positions: { $type: "array", $items: "f64" },
                        }
                    },
                    result_service: {
                        response_message_format: {
                            final_joint_positions: { $type: "array", $items: "f64" },
                            position: { $type: "array", $items: "f64", $length: 3 },
                        }
                    }
                }
            ]
        }
    }"#;

    fn array_length(format: Option<&config::node::MessageFormat>, field: &str) -> Option<usize> {
        let format = format.expect("the format is declared");
        let config::node::SchemaType::Array(array) = &format.0[field] else {
            panic!("`{field}` should be an array");
        };
        array.length
    }

    fn refinement_mismatches(err: ImplementsError) -> Vec<RefinementMismatch> {
        match err {
            ImplementsError::Refinement(mismatches) => mismatches,
            other => panic!("expected refinement mismatches, got: {other}"),
        }
    }

    /// The entry's `refine` block lands on the resolved member: the pinned
    /// arrays gain their length, every other field stays as the contract
    /// declares it, and the origin is stamped as for an unrefined entry.
    #[test]
    fn refine_pins_generic_arrays_of_the_resolved_member() {
        let tmp = TempDir::new().unwrap();
        let entry = seed_contract(tmp.path(), "limb_motion", "v1", LIMB_MOTION_V1_BODY);
        let (_tmp_dirs, dirs) = make_peppy_dirs_with_cache(&[entry]);

        let manifest =
            manifest_with_implements(r#"{ name: "limb_motion", tag: "v1", link_id: "moves" }"#);
        let interfaces = interfaces_from(
            r#"{
                topics: { emits: [
                    { link_id: "moves", name: "joint_states", refine: { message_format: { positions: { $length: 7 } } } },
                ] },
                actions: { exposes: [
                    {
                        link_id: "moves",
                        name: "move_arm_joints",
                        refine: {
                            goal_service: { request_message_format: { joint_positions: { $length: 7 } } },
                            result_service: { response_message_format: { final_joint_positions: { $length: 7 } } },
                        },
                    },
                ] },
            }"#,
        );

        let out = resolve_implements(&manifest, &interfaces, &dirs, None, &|_| {})
            .expect("admissible refinements resolve");
        assert_eq!(out.len(), 2);
        for resolved in &out {
            match resolved.interface() {
                InterfaceVariant::EmittedTopic {
                    topic,
                    origin: Some(origin),
                } => {
                    assert_eq!(
                        array_length(topic.message_format.as_ref(), "positions"),
                        Some(7)
                    );
                    assert_eq!(origin.link_id, "moves");
                }
                InterfaceVariant::ExposedAction {
                    action,
                    origin: Some(origin),
                } => {
                    let goal = action.goal_service.as_ref().unwrap();
                    let request = goal.request_message_format.as_ref();
                    assert_eq!(array_length(request, "joint_positions"), Some(7));
                    assert!(
                        matches!(
                            request.unwrap().0["arm_id"],
                            config::node::SchemaType::Type(config::node::TypeToken::U8)
                        ),
                        "unpinned fields stay as the contract declares them"
                    );
                    let response = action
                        .result_service
                        .as_ref()
                        .unwrap()
                        .response_message_format
                        .as_ref();
                    assert_eq!(array_length(response, "final_joint_positions"), Some(7));
                    assert_eq!(array_length(response, "position"), Some(3));
                    assert_eq!(origin.contract_name, "limb_motion");
                }
                other => panic!("unexpected resolved variant: {other:?}"),
            }
        }
    }

    /// Every pin the member refuses is reported for the entry at once, with
    /// the path inside the member, and the report names the contract and
    /// slot the entry resolved through.
    #[test]
    fn inadmissible_refine_is_reported_per_entry_with_every_problem() {
        use config::node::RefinementProblemKind;

        let tmp = TempDir::new().unwrap();
        let entry = seed_contract(tmp.path(), "limb_motion", "v1", LIMB_MOTION_V1_BODY);
        let (_tmp_dirs, dirs) = make_peppy_dirs_with_cache(&[entry]);

        let manifest =
            manifest_with_implements(r#"{ name: "limb_motion", tag: "v1", link_id: "moves" }"#);
        let interfaces = interfaces_from(
            r#"{
                topics: { emits: [{ link_id: "moves", name: "joint_states" }] },
                actions: { exposes: [
                    {
                        link_id: "moves",
                        name: "move_arm_joints",
                        refine: {
                            goal_service: { request_message_format: {
                                arm_id: { $length: 1 },
                                joint_positions: { $length: 7 },
                                missing: { $length: 2 },
                            } },
                            result_service: { response_message_format: { position: { $length: 3 } } },
                        },
                    },
                ] },
            }"#,
        );

        let mismatches = refinement_mismatches(
            resolve_implements(&manifest, &interfaces, &dirs, None, &|_| {})
                .expect_err("inadmissible pins must be rejected"),
        );
        assert_eq!(mismatches.len(), 1);
        let mismatch = &mismatches[0];
        assert_eq!(mismatch.name, "move_arm_joints");
        assert_eq!(mismatch.link_id, "moves");
        assert_eq!(mismatch.document, "contract `limb_motion:v1`");
        let reported: Vec<(&str, &RefinementProblemKind)> = mismatch
            .problems
            .iter()
            .map(|problem| (problem.path.as_str(), &problem.kind))
            .collect();
        assert_eq!(
            reported,
            vec![
                (
                    "goal_service.request_message_format.arm_id",
                    &RefinementProblemKind::NotAnArray {
                        declared: "a `u8`".to_string()
                    }
                ),
                (
                    "goal_service.request_message_format.missing",
                    &RefinementProblemKind::UnknownField
                ),
                (
                    "result_service.response_message_format.position",
                    &RefinementProblemKind::AlreadyFixed { length: 3 }
                ),
            ]
        );
        let rendered = mismatch.to_string();
        for needle in [
            "entry `move_arm_joints` (link_id `moves`) refines contract `limb_motion:v1`",
            "`goal_service.request_message_format.arm_id`: `$length` and `$items` apply to arrays, but the document declares a `u8`",
            "`result_service.response_message_format.position`: the document already fixes the length at 3",
        ] {
            assert!(
                rendered.contains(needle),
                "expected `{needle}` in: {rendered}"
            );
        }
    }

    /// A refined entry still counts toward coverage, and a coverage failure
    /// is what gets reported while one exists: the refinement report waits
    /// until the slot lists the right members.
    #[test]
    fn coverage_is_reported_before_refinement_problems() {
        let tmp = TempDir::new().unwrap();
        let entry = seed_contract(tmp.path(), "limb_motion", "v1", LIMB_MOTION_V1_BODY);
        let (_tmp_dirs, dirs) = make_peppy_dirs_with_cache(&[entry]);

        let manifest =
            manifest_with_implements(r#"{ name: "limb_motion", tag: "v1", link_id: "moves" }"#);
        // `joint_states` is missing, and the one entry present pins a field
        // the contract does not declare.
        let interfaces = interfaces_from(
            r#"{ actions: { exposes: [
                {
                    link_id: "moves",
                    name: "move_arm_joints",
                    refine: { goal_service: { request_message_format: { missing: { $length: 2 } } } },
                },
            ] } }"#,
        );

        let err = resolve_implements(&manifest, &interfaces, &dirs, None, &|_| {})
            .expect_err("the slot is not fully implemented");
        let ImplementsError::Coverage(mismatches) = err else {
            panic!("coverage must be reported first, got: {err}");
        };
        assert_eq!(mismatches.len(), 1);
        assert_eq!(mismatches[0].missing, vec!["joint_states (topic)"]);
        assert!(
            mismatches[0].duplicated.is_empty() && mismatches[0].unknown.is_empty(),
            "the refined entry counts as visiting its member: {:?}",
            mismatches[0]
        );
    }

    #[test]
    fn sha256_drift_is_rejected() {
        let tmp = TempDir::new().unwrap();
        let entry = seed_contract(tmp.path(), "depth_camera", "v1", DEPTH_V1_BODY);
        // Rewrite the underlying file so its fingerprint no longer matches
        // the cache's `sha256`, i.e. the cache thinks the file is X but it
        // is now Y. resolve_implements must catch this.
        fs::write(
            entry.origin.path_str(),
            DEPTH_V1_BODY.replace("video_stream", "video_stream_v2"),
        )
        .unwrap();
        // Keep the stale (pre-rewrite) sha256 in the cache entry. We need to
        // ensure load_contract_cache trusts it.
        let (_tmp_dirs, dirs) = make_peppy_dirs_with_cache(&[entry]);

        let manifest =
            manifest_with_implements(r#"{ name: "depth_camera", tag: "v1", link_id: "cam" }"#);
        let interfaces =
            interfaces_from(r#"{ topics: { emits: [{ link_id: "cam", name: "video_stream" }] } }"#);
        let err = resolve_implements(&manifest, &interfaces, &dirs, None, &|_| {})
            .expect_err("drift must error")
            .to_string();
        assert!(
            err.contains("drifted") && err.contains("peppy repo refresh"),
            "drift error should mention drift + refresh, got: {err}"
        );
    }

    /// Seeds a local git repository at `repo_dir` with a single file at the
    /// given repo-relative path, then returns the branch name (e.g. "main"
    /// or "master") that `ensure_checkout` can target.
    fn init_git_repo_with_contract(
        repo_dir: &std::path::Path,
        repo_relative_path: &str,
        body: &str,
    ) -> (String, daemon_config::repository::GitCommit) {
        let repo = git2::Repository::init(repo_dir).expect("git init");
        let abs = repo_dir.join(repo_relative_path);
        if let Some(parent) = abs.parent() {
            fs::create_dir_all(parent).expect("create parent dirs");
        }
        fs::write(&abs, body).expect("write contract file");

        let mut index = repo.index().expect("open index");
        index
            .add_path(std::path::Path::new(repo_relative_path))
            .expect("add path");
        index.write().expect("write index");
        let tree_id = index.write_tree().expect("write tree");
        let tree = repo.find_tree(tree_id).expect("find tree");
        let sig = git2::Signature::now("Peppy", "peppy@example.com").expect("signature");
        let oid = repo
            .commit(Some("HEAD"), &sig, &sig, "seed contract", &tree, &[])
            .expect("commit");
        let branch = repo
            .head()
            .expect("head")
            .shorthand()
            .expect("shorthand")
            .to_owned();
        (
            branch,
            daemon_config::repository::GitCommit::parse(&oid.to_string())
                .expect("a real commit is a full hash"),
        )
    }

    /// A `manifest.implements` slot whose cache record is git-sourced (so
    /// `entry.path` is repo-relative) must materialize the checkout via
    /// `ensure_checkout` and read from the joined absolute path, not from
    /// CWD.
    #[test]
    fn resolve_implements_git_sourced_contract_reads_from_checkout() {
        let peppy_tmp = TempDir::new().unwrap();
        let dirs = PeppyDirs::new(peppy_tmp.path().to_path_buf());
        fs::create_dir_all(dirs.cache_dir()).expect("create cache dir");

        let source_parent = TempDir::new().unwrap();
        let source_repo_dir = source_parent.path().join("contracts_hub");
        fs::create_dir_all(&source_repo_dir).expect("create source repo dir");
        let (branch, commit) = init_git_repo_with_contract(
            &source_repo_dir,
            "cameras/depth_camera.json5",
            DEPTH_V1_BODY,
        );
        let repo_url = source_repo_dir.display().to_string();

        let entry = repo_cache::ContractCacheEntry {
            contract_name: daemon_config::repository::ItemName::parse("depth_camera").unwrap(),
            tag: daemon_config::repository::ItemTag::parse("v1").unwrap(),
            sha256: daemon_config::repository::ManifestFingerprint::of_bytes(
                DEPTH_V1_BODY.as_bytes(),
            ),
            origin: repo_cache::EntryOrigin::Git {
                repo_url,
                repo_ref: Some(branch),
                commit,
                path: daemon_config::repository::RepoRelativePath::parse(
                    "cameras/depth_camera.json5",
                )
                .unwrap(),
            },
            repo_id: 0,
        };
        let cache_path = repo_cache::contracts_repo_cache_path(&dirs);
        fs::write(
            &cache_path,
            serde_json5::to_string(&vec![entry]).expect("serialize cache"),
        )
        .expect("write cache file");

        let manifest =
            manifest_with_implements(r#"{ name: "depth_camera", tag: "v1", link_id: "cam" }"#);
        let interfaces =
            interfaces_from(r#"{ topics: { emits: [{ link_id: "cam", name: "video_stream" }] } }"#);

        let out = resolve_implements(&manifest, &interfaces, &dirs, None, &|_| {})
            .expect("git-sourced implements should resolve");
        assert_eq!(out.len(), 1, "should pull the one video_stream topic");
        match out[0].interface() {
            InterfaceVariant::EmittedTopic {
                topic,
                origin: Some(o),
            } => {
                assert_eq!(topic.name, "video_stream");
                assert_eq!(o.contract_name, "depth_camera");
                assert_eq!(o.contract_tag, "v1");
            }
            other => panic!("expected EmittedTopic with origin, got {other:?}"),
        }
    }

    /// Companion to the git happy-path test: proves the drift fingerprint
    /// check runs against the *resolved* (checkout-joined) path, not just
    /// the cache record. Pinning a stale sha256 in the cache must still
    /// trigger the drift error even when the on-disk content lives in a
    /// git checkout the resolver has to materialize first.
    #[test]
    fn resolve_implements_git_sourced_drift_detected() {
        let peppy_tmp = TempDir::new().unwrap();
        let dirs = PeppyDirs::new(peppy_tmp.path().to_path_buf());
        fs::create_dir_all(dirs.cache_dir()).expect("create cache dir");

        let source_parent = TempDir::new().unwrap();
        let source_repo_dir = source_parent.path().join("contracts_hub");
        fs::create_dir_all(&source_repo_dir).expect("create source repo dir");
        let (branch, commit) = init_git_repo_with_contract(
            &source_repo_dir,
            "cameras/depth_camera.json5",
            DEPTH_V1_BODY,
        );
        let repo_url = source_repo_dir.display().to_string();

        let entry = repo_cache::ContractCacheEntry {
            contract_name: daemon_config::repository::ItemName::parse("depth_camera").unwrap(),
            tag: daemon_config::repository::ItemTag::parse("v1").unwrap(),
            // Deliberately wrong fingerprint; must trigger drift detection.
            sha256: daemon_config::repository::ManifestFingerprint::parse(&"0".repeat(64)).unwrap(),
            origin: repo_cache::EntryOrigin::Git {
                repo_url,
                repo_ref: Some(branch),
                commit,
                path: daemon_config::repository::RepoRelativePath::parse(
                    "cameras/depth_camera.json5",
                )
                .unwrap(),
            },
            repo_id: 0,
        };
        let cache_path = repo_cache::contracts_repo_cache_path(&dirs);
        fs::write(
            &cache_path,
            serde_json5::to_string(&vec![entry]).expect("serialize cache"),
        )
        .expect("write cache file");

        let manifest =
            manifest_with_implements(r#"{ name: "depth_camera", tag: "v1", link_id: "cam" }"#);
        let interfaces =
            interfaces_from(r#"{ topics: { emits: [{ link_id: "cam", name: "video_stream" }] } }"#);

        let err = resolve_implements(&manifest, &interfaces, &dirs, None, &|_| {})
            .expect_err("git-sourced drift must error")
            .to_string();
        assert!(
            err.contains("drifted") && err.contains("peppy repo refresh"),
            "drift error should mention drift + refresh, got: {err}"
        );
    }

    #[test]
    fn consumed_service_with_response_only_format_resolves() {
        const UVC_V1_BODY: &str = r#"{
            peppy_schema: "contract/v1",
            manifest: { name: "uvc_camera", tag: "v1" },
            interfaces: {
                services: [
                    {
                        name: "video_stream_info",
                        response_message_format: {
                            width: "u32",
                            height: "u32",
                            frames_per_second: "u8",
                        }
                    }
                ]
            }
        }"#;

        let tmp = TempDir::new().unwrap();
        let entry = seed_contract(tmp.path(), "uvc_camera", "v1", UVC_V1_BODY);
        let (_tmp_dirs, dirs) = make_peppy_dirs_with_cache(&[entry]);

        let manifest: Manifest = serde_json5::from_str(
            r#"{
                name: "uvc_consumer",
                tag: "v1",
                depends_on: {
                    contracts: [
                        { name: "uvc_camera", tag: "v1", link_id: "camera" }
                    ]
                }
            }"#,
        )
        .expect("manifest parses");
        let cfg: Interfaces = serde_json5::from_str(
            r#"{
                services: {
                    consumes: [
                        { link_id: "camera", name: "video_stream_info" }
                    ]
                }
            }"#,
        )
        .expect("interfaces parses");

        let out = collect_consumed_interfaces(&manifest, &cfg, |_, _| None, &dirs, None, &|_| {})
            .expect("response-only service must resolve");

        assert_eq!(
            out.len(),
            1,
            "expected exactly one ConsumedService, got {} entries (pre-fix this was 0: \
             the service was silently dropped)",
            out.len()
        );
        match out[0].interface() {
            InterfaceVariant::ConsumedService {
                service,
                request_format,
                response_format,
                ..
            } => {
                assert_eq!(service.name, "video_stream_info");
                assert!(
                    request_format.0.is_empty(),
                    "response-only service should have empty request format"
                );
                assert_eq!(
                    response_format.0.len(),
                    3,
                    "response format should preserve all three declared fields"
                );
            }
            other => panic!("expected ConsumedService variant, got {other:?}"),
        }
    }

    /// The category partition, end to end: a node holding a pairing slot AND
    /// a contract dependency must route each entry to exactly one collector.
    ///
    /// This is the regression this whole design is most exposed to. Admitting
    /// pairing link_ids into the consumed collector would either drop them
    /// (the consumed dependency lookup knows only node and contract kinds) or
    /// land them in the flat `consumed_topics` namespace, splitting a slot's
    /// two directions across unrelated module trees.
    #[test]
    fn pairing_and_contract_entries_route_to_separate_collectors() {
        const ARM_LINK_BODY: &str = r#"{
            peppy_schema: "pairing/v1",
            manifest: { name: "arm_link", tag: "v1" },
            roles: ["controller", "arm"],
            topics: [
                { emitted_by: "controller", name: "joint_commands" },
                { emitted_by: "arm", name: "joint_states" }
            ]
        }"#;

        // One PeppyDirs holding both caches, so `collect_all_deployment_interfaces`
        // can resolve the contract and the pairing document in one pass.
        let tmp = TempDir::new().unwrap();
        let contract = seed_contract(tmp.path(), "depth_camera", "v1", DEPTH_V1_BODY);
        let (_tmp_dirs, dirs) = make_peppy_dirs_with_cache(&[contract]);
        let pairing_path = tmp.path().join("arm_link_v1.json5");
        fs::write(&pairing_path, ARM_LINK_BODY).expect("write pairing doc");
        let pairing_entry = repo_cache::PairingCacheEntry {
            pairing_name: daemon_config::repository::ItemName::parse("arm_link").unwrap(),
            tag: daemon_config::repository::ItemTag::parse("v1").unwrap(),
            sha256: daemon_config::repository::ManifestFingerprint::of_bytes(
                ARM_LINK_BODY.as_bytes(),
            ),
            origin: repo_cache::EntryOrigin::Fs {
                path: pairing_path.clone(),
            },
            repo_id: 0,
        };
        fs::write(
            repo_cache::pairings_repo_cache_path(&dirs),
            serde_json5::to_string(&vec![pairing_entry]).expect("serialize pairing cache"),
        )
        .expect("write pairing cache");

        // The relay shape plus a pairing: one implements slot, one contract
        // dependency on the same contract, and one pairing slot. All three
        // collectors run, and each must claim exactly its own entries.
        let manifest: Manifest = serde_json5::from_str(
            r#"{
                name: "robot_arm", tag: "v1",
                implements: [{ name: "depth_camera", tag: "v1", link_id: "cam_out" }],
                depends_on: {
                    contracts: [{ name: "depth_camera", tag: "v1", link_id: "cam_in" }],
                    pairings: [{ name: "arm_link", tag: "v1", role: "arm", link_id: "controller" }]
                }
            }"#,
        )
        .expect("manifest parses");
        let cfg = interfaces_from(
            r#"{ topics: {
                emits: [
                    { link_id: "cam_out", name: "video_stream" },
                    { link_id: "controller", name: "joint_states" },
                ],
                consumes: [
                    { link_id: "controller", name: "joint_commands" },
                    { link_id: "cam_in", name: "video_stream" },
                ],
            } }"#,
        );

        let out =
            collect_all_deployment_interfaces(&manifest, &cfg, |_, _| None, &dirs, None, &|_| {})
                .expect("all three kinds resolve together");

        let mut emitted_topics = Vec::new();
        let mut peer_emitted = Vec::new();
        let mut peer_consumed = Vec::new();
        let mut consumed_topics = Vec::new();
        for interface in &out {
            match interface.interface() {
                InterfaceVariant::EmittedTopic { topic, .. } => {
                    emitted_topics.push(topic.name.as_str())
                }
                InterfaceVariant::PeerEmittedTopic { topic, .. } => {
                    peer_emitted.push(topic.name.as_str())
                }
                InterfaceVariant::PeerConsumedTopic { topic, .. } => {
                    peer_consumed.push(topic.name.as_str())
                }
                InterfaceVariant::ConsumedTopic { topic, .. } => {
                    consumed_topics.push(topic.name.as_str())
                }
                other => panic!("unexpected variant {other:?}"),
            }
        }
        assert_eq!(peer_emitted, vec!["joint_states"]);
        assert_eq!(peer_consumed, vec!["joint_commands"]);
        assert_eq!(
            emitted_topics,
            vec!["video_stream"],
            "the pairing emit must not be counted against the implements slot"
        );
        assert_eq!(
            consumed_topics,
            vec!["video_stream"],
            "the contract topic is the ONLY entry that may reach the consumed collector"
        );
        assert_eq!(out.len(), 4, "no entry may be collected twice: {out:?}");
    }

    /// A consumed entry naming no member of its contract used to resolve to
    /// `None` and get dropped, so a typo produced a missing module and a
    /// successful sync. Nothing upstream sees contract link_ids, so this is
    /// the only place it can be reported.
    #[test]
    fn consumed_entry_absent_from_contract_is_rejected() {
        let tmp = TempDir::new().unwrap();
        let entry = seed_contract(tmp.path(), "depth_camera", "v1", DEPTH_V1_BODY);
        let (_tmp_dirs, dirs) = make_peppy_dirs_with_cache(&[entry]);

        let manifest: Manifest = serde_json5::from_str(
            r#"{
                name: "camera_consumer", tag: "v1",
                depends_on: {
                    contracts: [{ name: "depth_camera", tag: "v1", link_id: "cam" }]
                }
            }"#,
        )
        .expect("manifest parses");
        let cfg = interfaces_from(
            r#"{ topics: { consumes: [{ link_id: "cam", name: "video_strem" }] } }"#,
        );

        let err = collect_consumed_interfaces(&manifest, &cfg, |_, _| None, &dirs, None, &|_| {})
            .expect_err("a name absent from the contract must not be dropped silently");
        for needle in [
            "video_strem",
            "cam",
            "depth_camera",
            "v1",
            "topics.consumes",
        ] {
            assert!(
                err.contains(needle),
                "error must name the entry, the slot and the document, missing {needle}: {err}"
            );
        }
    }

    /// Contract fixture for the consumed-action tests: one full member
    /// (goal request/response, feedback, result) and one minimal member
    /// (goal request and result only).
    const ARM_ACTIONS_V1_BODY: &str = r#"{
        peppy_schema: "contract/v1",
        manifest: { name: "arm_actions", tag: "v1" },
        interfaces: {
            actions: [
                {
                    name: "move_arm",
                    goal_service: {
                        request_message_format: { task: "string" },
                        response_message_format: { accepted: "bool" }
                    },
                    feedback_topic: {
                        qos_profile: "standard",
                        message_format: { frames_done: "u64" }
                    },
                    result_service: {
                        response_message_format: { success: "bool" }
                    }
                },
                {
                    name: "wave",
                    goal_service: {
                        request_message_format: { times: "u8" }
                    },
                    result_service: {
                        response_message_format: { success: "bool" }
                    }
                }
            ]
        }
    }"#;

    /// The consumer-side contract action path: a `depends_on.contracts` slot
    /// consuming a contract-declared action must resolve every action message
    /// format (goal request/response, feedback, result) from the contract
    /// document, and carry a contract-origin dependency so codegen addresses
    /// the producer as a contract. Consumed topics and services already had
    /// contract-slot coverage here; actions had none.
    #[test]
    fn consumed_action_via_contract_resolves() {
        let tmp = TempDir::new().unwrap();
        let entry = seed_contract(tmp.path(), "arm_actions", "v1", ARM_ACTIONS_V1_BODY);
        let (_tmp_dirs, dirs) = make_peppy_dirs_with_cache(&[entry]);

        let manifest: Manifest = serde_json5::from_str(
            r#"{
                name: "arm_consumer", tag: "v1",
                depends_on: {
                    contracts: [{ name: "arm_actions", tag: "v1", link_id: "arm" }]
                }
            }"#,
        )
        .expect("manifest parses");
        let cfg =
            interfaces_from(r#"{ actions: { consumes: [{ link_id: "arm", name: "move_arm" }] } }"#);

        let out = collect_consumed_interfaces(&manifest, &cfg, |_, _| None, &dirs, None, &|_| {})
            .expect("contract-declared action must resolve");

        assert_eq!(out.len(), 1, "expected exactly one ConsumedAction: {out:?}");
        match out[0].interface() {
            InterfaceVariant::ConsumedAction {
                action,
                messages,
                dependency,
            } => {
                assert_eq!(action.name, "move_arm");
                let goal_request = messages
                    .goal_request
                    .as_ref()
                    .expect("goal request format must come from the contract");
                assert_eq!(goal_request.0.len(), 1, "goal request should carry `task`");
                assert!(
                    messages.goal_response.is_some(),
                    "goal response format must come from the contract"
                );
                assert!(
                    messages.feedback.is_some(),
                    "feedback format must come from the contract"
                );
                assert!(
                    messages.result_response.is_some(),
                    "result format must come from the contract"
                );
                let origin = dependency
                    .origin
                    .as_ref()
                    .expect("a contract slot must resolve to a contract-origin dependency");
                assert_eq!(origin.contract_name, "arm_actions");
                assert_eq!(origin.contract_tag, "v1");
            }
            other => panic!("expected ConsumedAction variant, got {other:?}"),
        }
    }

    fn limb_motion_consumer_manifest() -> Manifest {
        serde_json5::from_str(
            r#"{
                name: "arm_consumer", tag: "v1",
                depends_on: {
                    contracts: [{ name: "limb_motion", tag: "v1", link_id: "arm" }]
                }
            }"#,
        )
        .expect("manifest parses")
    }

    /// A consumer that knows the arm it is bound to pins the contract's
    /// generic arrays the same way an implementer does, on each consumed
    /// kind.
    #[test]
    fn consumed_contract_entries_apply_their_refine_blocks() {
        let tmp = TempDir::new().unwrap();
        let entry = seed_contract(tmp.path(), "limb_motion", "v1", LIMB_MOTION_V1_BODY);
        let (_tmp_dirs, dirs) = make_peppy_dirs_with_cache(&[entry]);
        let cfg = interfaces_from(
            r#"{
                topics: { consumes: [
                    { link_id: "arm", name: "joint_states", refine: { message_format: { positions: { $length: 7 } } } },
                ] },
                actions: { consumes: [
                    {
                        link_id: "arm",
                        name: "move_arm_joints",
                        refine: { goal_service: { request_message_format: { joint_positions: { $length: 7 } } } },
                    },
                ] },
            }"#,
        );

        let out = collect_consumed_interfaces(
            &limb_motion_consumer_manifest(),
            &cfg,
            |_, _| None,
            &dirs,
            None,
            &|_| {},
        )
        .expect("admissible refinements resolve");
        assert_eq!(out.len(), 2, "{out:?}");
        for resolved in &out {
            match resolved.interface() {
                InterfaceVariant::ConsumedTopic { message_format, .. } => {
                    assert_eq!(array_length(Some(message_format), "positions"), Some(7));
                }
                InterfaceVariant::ConsumedAction { messages, .. } => {
                    assert_eq!(
                        array_length(messages.goal_request.as_ref(), "joint_positions"),
                        Some(7)
                    );
                    assert_eq!(
                        array_length(messages.result_response.as_ref(), "final_joint_positions"),
                        None,
                        "an endpoint the block does not name stays generic"
                    );
                }
                other => panic!("unexpected resolved variant: {other:?}"),
            }
        }
    }

    /// Like a name absent from the contract, an inadmissible pin has no
    /// reporter upstream of this collector, so it is reported here with the
    /// entry, slot, contract, and every problem named.
    #[test]
    fn consumed_contract_entry_with_inadmissible_refine_is_rejected() {
        let tmp = TempDir::new().unwrap();
        let entry = seed_contract(tmp.path(), "limb_motion", "v1", LIMB_MOTION_V1_BODY);
        let (_tmp_dirs, dirs) = make_peppy_dirs_with_cache(&[entry]);
        let cfg = interfaces_from(
            r#"{ topics: { consumes: [
                { link_id: "arm", name: "joint_states", refine: { message_format: { positions: { $length: 7 }, nope: { $length: 1 } } } },
            ] } }"#,
        );

        let err = collect_consumed_interfaces(
            &limb_motion_consumer_manifest(),
            &cfg,
            |_, _| None,
            &dirs,
            None,
            &|_| {},
        )
        .expect_err("an inadmissible pin must not be dropped silently");
        for needle in [
            "entry `joint_states` (link_id `arm`) refines contract `limb_motion:v1`",
            "`message_format.nope`: the document declares no such field",
        ] {
            assert!(err.contains(needle), "expected `{needle}` in: {err}");
        }
    }

    /// A contract action without a feedback topic (and without a goal
    /// response) resolves with those formats absent instead of inventing
    /// them, so the generated consumer exposes no feedback API for a
    /// progress-free member.
    #[test]
    fn consumed_feedbackless_action_via_contract_resolves_without_feedback() {
        let tmp = TempDir::new().unwrap();
        let entry = seed_contract(tmp.path(), "arm_actions", "v1", ARM_ACTIONS_V1_BODY);
        let (_tmp_dirs, dirs) = make_peppy_dirs_with_cache(&[entry]);

        let manifest: Manifest = serde_json5::from_str(
            r#"{
                name: "arm_consumer", tag: "v1",
                depends_on: {
                    contracts: [{ name: "arm_actions", tag: "v1", link_id: "arm" }]
                }
            }"#,
        )
        .expect("manifest parses");
        let cfg =
            interfaces_from(r#"{ actions: { consumes: [{ link_id: "arm", name: "wave" }] } }"#);

        let out = collect_consumed_interfaces(&manifest, &cfg, |_, _| None, &dirs, None, &|_| {})
            .expect("feedback-less contract action must resolve");

        assert_eq!(out.len(), 1, "expected exactly one ConsumedAction: {out:?}");
        match out[0].interface() {
            InterfaceVariant::ConsumedAction {
                action, messages, ..
            } => {
                assert_eq!(action.name, "wave");
                assert!(messages.goal_request.is_some());
                assert!(
                    messages.goal_response.is_none(),
                    "an absent goal response must stay absent"
                );
                assert!(
                    messages.feedback.is_none(),
                    "an absent feedback topic must stay absent"
                );
                assert!(messages.result_response.is_some());
            }
            other => panic!("expected ConsumedAction variant, got {other:?}"),
        }
    }

    /// A consumed action naming no member of its contract is rejected with
    /// the same error shape as topics and services; nothing upstream sees
    /// contract link_ids, so this is the only place it can be reported.
    #[test]
    fn consumed_action_absent_from_contract_is_rejected() {
        let tmp = TempDir::new().unwrap();
        let entry = seed_contract(tmp.path(), "arm_actions", "v1", ARM_ACTIONS_V1_BODY);
        let (_tmp_dirs, dirs) = make_peppy_dirs_with_cache(&[entry]);

        let manifest: Manifest = serde_json5::from_str(
            r#"{
                name: "arm_consumer", tag: "v1",
                depends_on: {
                    contracts: [{ name: "arm_actions", tag: "v1", link_id: "arm" }]
                }
            }"#,
        )
        .expect("manifest parses");
        let cfg =
            interfaces_from(r#"{ actions: { consumes: [{ link_id: "arm", name: "move_leg" }] } }"#);

        let err = collect_consumed_interfaces(&manifest, &cfg, |_, _| None, &dirs, None, &|_| {})
            .expect_err("a name absent from the contract must not be dropped silently");
        for needle in ["move_leg", "arm", "arm_actions", "v1", "actions.consumes"] {
            assert!(
                err.contains(needle),
                "error must name the entry, the slot and the document, missing {needle}: {err}"
            );
        }
    }

    /// The same typo against a NODE dependency is reported once, by
    /// `validate_dependency_specs` upstream. This collector stays silent so
    /// the user does not see it twice.
    #[test]
    fn unknown_node_dep_name_is_left_to_the_upstream_reporter() {
        let (_tmp_dirs, dirs) = make_peppy_dirs_with_cache(&[]);

        let producer: config::node::NodeConfig = config::node::NodeConfigParser::from_content(
            r#"{
                peppy_schema: "node/v1",
                manifest: { name: "producer_node", tag: "v1" },
                interfaces: {
                    topics: { emits: [{ name: "debug_stream", message_format: { x: "f64" } }] },
                },
                execution: { language: "rust", run_cmd: ["./bin"] },
            }"#,
        )
        .expect("producer parses");
        let manifest: Manifest = serde_json5::from_str(
            r#"{
                name: "consumer", tag: "v1",
                depends_on: { nodes: [{ name: "producer_node", tag: "v1", link_id: "producer" }] }
            }"#,
        )
        .expect("manifest parses");
        let cfg = interfaces_from(
            r#"{ topics: { consumes: [{ link_id: "producer", name: "debug_strem" }] } }"#,
        );

        let out = collect_consumed_interfaces(
            &manifest,
            &cfg,
            |name, _| (name == "producer_node").then(|| producer.clone()),
            &dirs,
            None,
            &|_| {},
        )
        .expect("collection itself must not fail for a node dep");
        assert!(out.is_empty(), "nothing resolves, and nothing is reported");

        let upstream = config::node::validate_dependency_specs(
            &manifest,
            &cfg,
            "consumer",
            "v1",
            config::node::MissingDependencyPolicy::RequireResolvable,
            |name, _| (name == "producer_node").then(|| producer.clone()),
        );
        assert_eq!(
            upstream.len(),
            1,
            "exactly one reporter owns this error: {upstream:?}"
        );
    }

    /// Node-dependency consumption resolves the producer's NATIVE entries
    /// only; the same interface consumed via a contract dep is
    /// contract-addressed.
    #[test]
    fn node_dep_consumption_is_native_only() {
        let (_tmp_dirs, dirs) = make_peppy_dirs_with_cache(&[]);

        let producer: config::node::NodeConfig = config::node::NodeConfigParser::from_content(
            r#"{
                peppy_schema: "node/v1",
                manifest: {
                    name: "hybrid", tag: "v1",
                    implements: [{ name: "depth_camera", tag: "v1", link_id: "cam" }],
                },
                interfaces: {
                    topics: { emits: [
                        { link_id: "cam", name: "video_stream" },
                        { name: "debug_stream", message_format: { x: "f64" } },
                    ] },
                },
                execution: { language: "rust", run_cmd: ["./bin"] },
            }"#,
        )
        .expect("producer parses");

        let manifest: Manifest = serde_json5::from_str(
            r#"{
                name: "consumer",
                tag: "v1",
                depends_on: {
                    nodes: [ { name: "hybrid", tag: "v1", link_id: "producer" } ]
                }
            }"#,
        )
        .expect("manifest parses");

        // Native name resolves, node-addressed.
        let native_cfg: Interfaces = serde_json5::from_str(
            r#"{ topics: { consumes: [{ link_id: "producer", name: "debug_stream" }] } }"#,
        )
        .unwrap();
        let out = collect_consumed_interfaces(
            &manifest,
            &native_cfg,
            |name, _| (name == "hybrid").then(|| producer.clone()),
            &dirs,
            None,
            &|_| {},
        )
        .expect("native consumption resolves");
        assert_eq!(out.len(), 1);
        match out[0].interface() {
            InterfaceVariant::ConsumedTopic { dependency, .. } => {
                assert!(
                    dependency.origin.is_none(),
                    "node-dep consumption must be node-addressed"
                );
            }
            other => panic!("expected ConsumedTopic, got {other:?}"),
        }

        // Contract-backed-only name does not resolve through the node dep
        // (validate_dependency_specs reports the dedicated error upstream;
        // resolution simply finds no native offering here).
        let contract_backed_cfg: Interfaces = serde_json5::from_str(
            r#"{ topics: { consumes: [{ link_id: "producer", name: "video_stream" }] } }"#,
        )
        .unwrap();
        let out = collect_consumed_interfaces(
            &manifest,
            &contract_backed_cfg,
            |name, _| (name == "hybrid").then(|| producer.clone()),
            &dirs,
            None,
            &|_| {},
        )
        .expect("collection itself does not fail");
        assert!(
            out.is_empty(),
            "a contract-backed-only name must not resolve through a node dep"
        );
    }
}
