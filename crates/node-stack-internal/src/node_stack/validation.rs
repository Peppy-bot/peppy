use std::collections::{HashMap, HashSet};

use config::node::{InterfaceKind, Interfaces, Manifest, NodeConfig};

use super::entity::DependencySpec;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct InterfaceRequirement {
    kind: InterfaceKind,
    name: String,
}

impl InterfaceRequirement {
    pub(super) fn new(kind: InterfaceKind, name: &str) -> Self {
        Self {
            kind,
            name: name.trim().to_owned(),
        }
    }

    pub(super) fn kind(&self) -> InterfaceKind {
        self.kind
    }

    pub(super) fn name(&self) -> &str {
        &self.name
    }
}

pub(super) fn interfaces_match(a: &Interfaces, b: &Interfaces) -> bool {
    a == b
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
/// 2. **Interface exposure**: Each consumed/expected interface must reference a valid `local_node_id`
///    that maps to a dependency which exposes the required interface.
pub fn validate_dependency_specs(
    manifest: &Manifest,
    interfaces: &Interfaces,
    dependant_name: &str,
    dependant_tag: &str,
    resolve: impl Fn(&str, &str) -> Option<NodeConfig>,
) -> Vec<crate::error::Error> {
    let mut errors = Vec::new();

    // Build local_id → (name, tag, resolved_config) lookup from depends_on.nodes
    let mut resolved_deps: HashMap<String, (String, String, NodeConfig)> = HashMap::new();

    // Phase 1: Validate all declared dependency nodes exist
    if let Some(depends_on) = &manifest.depends_on {
        for dep in &depends_on.nodes {
            let dep_name = dep.name.as_str().to_owned();
            let dep_tag = dep.tag.clone();
            let Some(dependency_config) = resolve(&dep_name, &dep_tag) else {
                errors.push(crate::error::Error::MissingDependency {
                    dependant: dependant_name.to_owned(),
                    dependant_tag: dependant_tag.to_owned(),
                    dependency: dep_name,
                    dependency_tag: dep_tag,
                });
                continue;
            };
            resolved_deps.insert(dep.local_id.clone(), (dep_name, dep_tag, dependency_config));
        }
    }

    // Collect all declared local_ids so we can distinguish "declared but unresolved"
    // (already has a MissingDependency error) from "never declared" (typo).
    let declared_local_ids: HashSet<&str> = manifest
        .depends_on
        .as_ref()
        .map(|d| d.nodes.iter().map(|n| n.local_id.as_str()).collect())
        .unwrap_or_default();

    // Phase 2: Validate consumed interfaces reference valid local_node_ids
    // and that the dependency exposes the required interface
    if let Some(topics) = &interfaces.topics
        && let Some(expected) = &topics.consumes
    {
        for topic in expected {
            if let config::node::ConsumedTopic::Linked(linked) = topic {
                if !resolved_deps.contains_key(linked.local_node_id.as_str()) {
                    if !declared_local_ids.contains(linked.local_node_id.as_str()) {
                        errors.push(crate::error::Error::UndeclaredLocalNodeId {
                            dependant: dependant_name.to_owned(),
                            dependant_tag: dependant_tag.to_owned(),
                            local_node_id: linked.local_node_id.clone(),
                        });
                    }
                    continue;
                }
                validate_consumed_interface(
                    &linked.local_node_id,
                    &linked.name,
                    InterfaceKind::Topic,
                    &resolved_deps,
                    dependant_name,
                    dependant_tag,
                    &mut errors,
                );
            }
        }
    }

    if let Some(services) = &interfaces.services
        && let Some(consumed) = &services.consumes
    {
        for service in consumed {
            if !resolved_deps.contains_key(service.local_node_id.as_str()) {
                if !declared_local_ids.contains(service.local_node_id.as_str()) {
                    errors.push(crate::error::Error::UndeclaredLocalNodeId {
                        dependant: dependant_name.to_owned(),
                        dependant_tag: dependant_tag.to_owned(),
                        local_node_id: service.local_node_id.clone(),
                    });
                }
                continue;
            }
            validate_consumed_interface(
                &service.local_node_id,
                &service.name,
                InterfaceKind::Service,
                &resolved_deps,
                dependant_name,
                dependant_tag,
                &mut errors,
            );
        }
    }

    if let Some(actions) = &interfaces.actions
        && let Some(consumed) = &actions.consumes
    {
        for action in consumed {
            if !resolved_deps.contains_key(action.local_node_id.as_str()) {
                if !declared_local_ids.contains(action.local_node_id.as_str()) {
                    errors.push(crate::error::Error::UndeclaredLocalNodeId {
                        dependant: dependant_name.to_owned(),
                        dependant_tag: dependant_tag.to_owned(),
                        local_node_id: action.local_node_id.clone(),
                    });
                }
                continue;
            }
            validate_consumed_interface(
                &action.local_node_id,
                &action.name,
                InterfaceKind::Action,
                &resolved_deps,
                dependant_name,
                dependant_tag,
                &mut errors,
            );
        }
    }

    errors
}

/// Validates that a consumed interface's `local_node_id` resolves to a dependency
/// that exposes the required interface.
fn validate_consumed_interface(
    local_node_id: &str,
    interface_name: &str,
    kind: InterfaceKind,
    resolved_deps: &HashMap<String, (String, String, NodeConfig)>,
    dependant_name: &str,
    dependant_tag: &str,
    errors: &mut Vec<crate::error::Error>,
) {
    let Some((dep_name, dep_tag, dep_config)) = resolved_deps.get(local_node_id) else {
        // The local_node_id doesn't map to any resolved dependency.
        // This path is only reached when the dependency was declared but failed
        // to resolve (already reported as MissingDependency in Phase 1).
        // Undeclared local_node_ids are caught before this function is called.
        return;
    };

    let requirement = InterfaceRequirement::new(kind, interface_name);
    if !exposes_interface(dep_config, &requirement) {
        errors.push(crate::error::Error::MissingInterface {
            dependant: dependant_name.to_owned(),
            dependant_tag: dependant_tag.to_owned(),
            dependency: dep_name.clone(),
            dependency_tag: dep_tag.clone(),
            interface_kind: format!("{:?}", kind),
            interface_name: interface_name.to_owned(),
        });
    }
}

pub(crate) fn exposes_interface(node: &NodeConfig, requirement: &InterfaceRequirement) -> bool {
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
