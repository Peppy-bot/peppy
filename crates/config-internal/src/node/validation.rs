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

use crate::error::{MissingInterface, ParsingError};
use crate::node::{InterfaceKind, Interfaces, Manifest, NodeConfig};

/// Minimal `(name, tag)` view of a single `depends_on.nodes` entry,
/// stripped of the link_id / from_any noise that callers don't need
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

/// Validates that all dependencies of a node config exist and expose the required interfaces.
///
/// Uses the provided `resolve` closure to look up a dependency's `NodeConfig` by name and tag.
/// Returns a list of all validation errors found (empty if all dependencies are satisfied).
///
/// Validation is two-phase:
/// 1. **Node existence**: Each entry in `manifest.depends_on.nodes` must resolve to an existing node.
/// 2. **Interface exposure**: Each consumed/expected interface must reference a valid `link_id`
///    declared in either `depends_on.nodes` or `depends_on.interfaces`. For node-backed
///    link_ids the producer must expose the required interface; interface-backed link_ids
///    are validated against their parsed interface contract at parse time and only need
///    the link_id declaration check here.
pub fn validate_dependency_specs(
    manifest: &Manifest,
    interfaces: &Interfaces,
    dependant_name: &str,
    dependant_tag: &str,
    resolve: impl Fn(&str, &str) -> Option<NodeConfig>,
) -> Vec<ParsingError> {
    let mut errors = Vec::new();

    // Build link_id → (name, tag, resolved_config) lookup from depends_on.nodes
    let mut resolved_deps: HashMap<String, (String, String, NodeConfig)> = HashMap::new();

    // Phase 1: Validate all declared dependency nodes exist
    if let Some(depends_on) = &manifest.depends_on {
        for dep in &depends_on.nodes {
            let dep_name = dep.name.as_str().to_owned();
            let dep_tag = dep.tag.clone();
            let Some(dependency_config) = resolve(&dep_name, &dep_tag) else {
                errors.push(ParsingError::MissingDependency {
                    dependant: dependant_name.to_owned(),
                    dependant_tag: dependant_tag.to_owned(),
                    dependency: dep_name,
                    dependency_tag: dep_tag,
                });
                continue;
            };
            resolved_deps.insert(dep.link_id.clone(), (dep_name, dep_tag, dependency_config));
        }
    }

    // Collect all declared link_ids so we can distinguish "declared but unresolved"
    // (already has a MissingDependency error or is an interface-backed dep validated
    // at parse time) from "never declared" (typo).
    let declared_link_ids: HashSet<&str> = manifest
        .depends_on
        .as_ref()
        .map(|d| {
            d.nodes
                .iter()
                .map(|n| n.link_id.as_str())
                .chain(d.interfaces.iter().map(|i| i.link_id.as_str()))
                .collect()
        })
        .unwrap_or_default();

    // Phase 2: Validate consumed interfaces reference valid link_ids
    // and that the dependency exposes the required interface
    if let Some(topics) = &interfaces.topics
        && let Some(consumes) = &topics.consumes
    {
        let items = consumes
            .iter()
            .map(|t| (t.link_id.as_str(), t.name.as_str()));
        validate_consumed_items(
            items,
            InterfaceKind::Topic,
            &resolved_deps,
            &declared_link_ids,
            dependant_name,
            dependant_tag,
            &mut errors,
        );
    }

    if let Some(services) = &interfaces.services
        && let Some(consumes) = &services.consumes
    {
        let items = consumes
            .iter()
            .map(|s| (s.link_id.as_str(), s.name.as_str()));
        validate_consumed_items(
            items,
            InterfaceKind::Service,
            &resolved_deps,
            &declared_link_ids,
            dependant_name,
            dependant_tag,
            &mut errors,
        );
    }

    if let Some(actions) = &interfaces.actions
        && let Some(consumes) = &actions.consumes
    {
        let items = consumes
            .iter()
            .map(|a| (a.link_id.as_str(), a.name.as_str()));
        validate_consumed_items(
            items,
            InterfaceKind::Action,
            &resolved_deps,
            &declared_link_ids,
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
fn validate_consumed_items<'a>(
    items: impl Iterator<Item = (&'a str, &'a str)>,
    kind: InterfaceKind,
    resolved_deps: &HashMap<String, (String, String, NodeConfig)>,
    declared_link_ids: &HashSet<&str>,
    dependant_name: &str,
    dependant_tag: &str,
    errors: &mut Vec<ParsingError>,
) {
    for (link_id, name) in items {
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
/// that exposes the required interface.
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
        // This path is only reached when the dependency was declared but failed
        // to resolve (already reported as MissingDependency in Phase 1).
        // Undeclared link_ids are caught before this function is called.
        return;
    };

    let requirement = InterfaceRequirement::new(kind, interface_name);
    if !exposes_interface(dep_config, &requirement) {
        errors.push(ParsingError::MissingInterface(Box::new(MissingInterface {
            dependant: dependant_name.to_owned(),
            dependant_tag: dependant_tag.to_owned(),
            dependency: dep_name.clone(),
            dependency_tag: dep_tag.clone(),
            interface_kind: format!("{:?}", kind),
            interface_name: interface_name.to_owned(),
        })));
    }
}

fn exposes_interface(node: &NodeConfig, requirement: &InterfaceRequirement) -> bool {
    match requirement.kind() {
        InterfaceKind::Topic => node
            .interfaces
            .topics
            .as_ref()
            .and_then(|t| t.emits.as_ref())
            .is_some_and(|topics| {
                topics
                    .iter()
                    .any(|topic| topic.name.trim() == requirement.name())
            }),
        InterfaceKind::Service => node
            .interfaces
            .services
            .as_ref()
            .and_then(|s| s.exposes.as_ref())
            .is_some_and(|services| {
                services
                    .iter()
                    .any(|service| service.name.trim() == requirement.name())
            }),
        InterfaceKind::Action => node
            .interfaces
            .actions
            .as_ref()
            .and_then(|a| a.exposes.as_ref())
            .is_some_and(|actions| {
                actions
                    .iter()
                    .any(|action| action.name.trim() == requirement.name())
            }),
    }
}
