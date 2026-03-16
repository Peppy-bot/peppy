use crate::error::{Error, Result};
use config::node::{InterfaceKind, Interfaces, Name, NodeConfig};
use names_generator2::get_random;
use petgraph::{
    Direction,
    dot::{Config, Dot},
    stable_graph::{NodeIndex, StableDiGraph},
    visit::EdgeRef,
};
use rand::rng;
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, RwLock},
};

/// Serializable representation of a node in the graph.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SerializedNode {
    pub name: String,
    pub tag: String,
    pub instance_ids: Vec<String>,
    pub fs_root_path: String,
}

impl SerializedNode {
    /// Returns a display label in the format "name:tag".
    pub fn label(&self) -> String {
        format!("{}:{}", self.name, self.tag)
    }

    /// Returns the number of instances.
    pub fn instance_count(&self) -> usize {
        self.instance_ids.len()
    }

    /// Returns instance info in the format "N instance(s): ["id1", "id2"]".
    pub fn instance_info(&self) -> String {
        let count = self.instance_count();
        let suffix = if count == 1 { "instance" } else { "instances" };
        let ids: Vec<String> = self
            .instance_ids
            .iter()
            .map(|id| format!("\"{}\"", id))
            .collect();
        format!("{} {}: [{}]", count, suffix, ids.join(", "))
    }
}

/// Serializable representation of a dependency edge.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SerializedEdge {
    pub from: SerializedNode,
    pub to: SerializedNode,
}

/// Serializable representation of the entire node graph.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SerializedNodeGraph {
    pub nodes: Vec<SerializedNode>,
    pub edges: Vec<SerializedEdge>,
}

/// This represent a single entity with n instances inside the node_stack.
/// A NodeEntity always has at least one instance.
#[derive(Clone, Debug)]
pub struct NodeEntity {
    config: NodeConfig,
    instances: Vec<TrackedNodeInstance>,
    // TODO: In the future, for total isolation of the snapshot node we could use a solution like rootless Podman
    // Every node has a root path, it's the directory where the configuration resides
    fs_root_path: PathBuf,
}

impl NodeEntity {
    /// Creates a new NodeEntity with a config only (no instances)
    pub fn new<P: Into<PathBuf>>(config: NodeConfig, root_path: P) -> Self {
        Self {
            config,
            instances: Vec::new(),
            fs_root_path: root_path.into(),
        }
    }

    pub fn config(&self) -> &NodeConfig {
        &self.config
    }

    pub fn root_path(&self) -> &Path {
        &self.fs_root_path
    }

    pub fn into_config(self) -> NodeConfig {
        self.config
    }

    pub fn instances(&self) -> &[TrackedNodeInstance] {
        &self.instances
    }

    /// Adds an instance to this entity
    fn add_instance(&mut self, instance: TrackedNodeInstance) {
        self.instances.push(instance);
    }

    /// Removes an instance by its ID. Returns true if the instance was found and removed.
    fn remove_instance(&mut self, instance_id: &Name) -> bool {
        if let Some(pos) = self
            .instances
            .iter()
            .position(|i| i.instance_id() == instance_id)
        {
            self.instances.remove(pos);
            true
        } else {
            false
        }
    }

    /// Returns the number of instances
    fn instance_count(&self) -> usize {
        self.instances.len()
    }
}

#[derive(Debug, Clone)]
pub struct TrackedNodeInstance {
    instance_id: Name,
    /// Process ID of the running instance. This is `None` for instances running on remote
    /// locations (e.g., embedded systems) where a local PID is not available.
    pid: Option<u32>,
}

impl TrackedNodeInstance {
    pub fn new(instance_id: Name, pid: Option<u32>) -> Self {
        Self { instance_id, pid }
    }

    pub fn instance_id(&self) -> &Name {
        &self.instance_id
    }

    pub fn pid(&self) -> Option<u32> {
        self.pid
    }
}

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
struct NodeKey {
    name: String,
    tag: String,
}

impl NodeKey {
    fn new(name: &str, tag: &str) -> Self {
        Self {
            name: name.trim().to_owned(),
            tag: tag.trim().to_owned(),
        }
    }
}

impl From<&NodeEntity> for NodeKey {
    fn from(entity: &NodeEntity) -> Self {
        NodeKey::new(
            entity.config.manifest.name.as_str(),
            &entity.config.manifest.tag,
        )
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct InterfaceRequirement {
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

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct DependencySpec {
    pub node_name: String,
    pub node_tag: String,
}

fn interfaces_match(a: &Interfaces, b: &Interfaces) -> bool {
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
    config: &NodeConfig,
    dependant_name: &str,
    dependant_tag: &str,
    resolve: impl Fn(&str, &str) -> Option<NodeConfig>,
) -> Vec<crate::error::Error> {
    let mut errors = Vec::new();

    // Build local_id → (name, tag, resolved_config) lookup from depends_on.nodes
    let mut resolved_deps: HashMap<String, (String, String, NodeConfig)> = HashMap::new();

    // Phase 1: Validate all declared dependency nodes exist
    if let Some(depends_on) = &config.manifest.depends_on {
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

    // Phase 2: Validate consumed interfaces reference valid local_node_ids
    // and that the dependency exposes the required interface
    if let Some(topics) = &config.interfaces.topics
        && let Some(expected) = &topics.consumes
    {
        for topic in expected {
            if let config::node::ConsumedTopic::Linked(linked) = topic {
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

    if let Some(services) = &config.interfaces.services
        && let Some(consumed) = &services.consumes
    {
        for service in consumed {
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

    if let Some(actions) = &config.interfaces.actions
        && let Some(consumed) = &actions.consumes
    {
        for action in consumed {
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
        // The local_node_id doesn't map to any resolved dependency — either
        // the dependency itself was missing (already reported in phase 1)
        // or the local_node_id is invalid. Skip silently since phase 1 covers
        // missing dependencies.
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

struct NodeStackInner {
    graph: StableDiGraph<NodeEntity, ()>,
    key_to_index: HashMap<NodeKey, NodeIndex>,
    pending_requirements: HashMap<NodeKey, Vec<PendingRequirement>>,
    root_key: NodeKey,
}

impl NodeStackInner {
    fn new(root: NodeEntity) -> Self {
        let root_key = NodeKey::from(&root);
        let mut inner = Self {
            graph: StableDiGraph::default(),
            key_to_index: HashMap::new(),
            pending_requirements: HashMap::new(),
            root_key,
        };
        // Root node has no dependencies, so this should never fail
        inner
            .insert_entity(root)
            .expect("root node should have no dependencies");
        inner
    }

    fn insert_entity(&mut self, node: NodeEntity) -> Result<()> {
        // Validate all dependencies exist and expose the required interfaces
        self.validate_dependencies(&node)?;

        let key = NodeKey::from(&node);
        let index = if let Some(&existing_index) = self.key_to_index.get(&key) {
            self.graph[existing_index] = node;
            existing_index
        } else {
            let idx = self.graph.add_node(node);
            self.key_to_index.insert(key.clone(), idx);
            idx
        };

        self.rewire_dependencies(index);
        self.resolve_pending_requirements(&key);
        Ok(())
    }

    fn insert_entity_lenient(&mut self, node: NodeEntity) -> Result<()> {
        let key = NodeKey::from(&node);
        let index = if let Some(&existing_index) = self.key_to_index.get(&key) {
            self.graph[existing_index] = node;
            existing_index
        } else {
            let idx = self.graph.add_node(node);
            self.key_to_index.insert(key.clone(), idx);
            idx
        };

        self.rewire_dependencies(index);
        self.resolve_pending_requirements(&key);
        Ok(())
    }

    fn validate_dependencies(&self, node: &NodeEntity) -> Result<()> {
        let errors = validate_dependency_specs(
            node.config(),
            node.config().manifest.name.as_str(),
            &node.config().manifest.tag,
            |name, tag| {
                let key = NodeKey::new(name, tag);
                self.key_to_index
                    .get(&key)
                    .and_then(|&idx| self.graph.node_weight(idx))
                    .map(|entity| entity.config().clone())
            },
        );

        if let Some(err) = errors.into_iter().next() {
            return Err(err);
        }

        Ok(())
    }

    fn rewire_dependencies(&mut self, index: NodeIndex) {
        let existing_edges: Vec<_> = self
            .graph
            .edges_directed(index, Direction::Outgoing)
            .map(|edge| edge.id())
            .collect();
        for edge in existing_edges {
            self.graph.remove_edge(edge);
        }
        self.attach_dependencies(index);
    }

    fn attach_dependencies(&mut self, index: NodeIndex) {
        let requirements = if let Some(node) = self.graph.node_weight(index) {
            dependency_requirements(node.config())
        } else {
            return;
        };
        self.clear_pending_requirements_for(index);
        for requirement in requirements {
            if !self.try_attach_requirement(index, &requirement) {
                self.register_pending_requirement(requirement, index);
            }
        }
    }

    fn clear_pending_requirements_for(&mut self, dependant: NodeIndex) {
        self.pending_requirements.retain(|_, pending| {
            pending.retain(|req| req.dependant != dependant);
            !pending.is_empty()
        });
    }

    fn register_pending_requirement(
        &mut self,
        requirement: DependencyRequirement,
        dependant: NodeIndex,
    ) {
        let entry = self
            .pending_requirements
            .entry(requirement.key.clone())
            .or_default();
        if entry.iter().any(|pending| pending.dependant == dependant) {
            return;
        }
        entry.push(PendingRequirement { dependant });
    }

    fn try_attach_requirement(
        &mut self,
        dependant_index: NodeIndex,
        requirement: &DependencyRequirement,
    ) -> bool {
        let Some(&dependency_index) = self.key_to_index.get(&requirement.key) else {
            return false;
        };

        if self
            .graph
            .find_edge(dependant_index, dependency_index)
            .is_none()
        {
            self.graph.add_edge(dependant_index, dependency_index, ());
        }
        true
    }

    fn resolve_pending_requirements(&mut self, key: &NodeKey) {
        if !self.key_to_index.contains_key(key) {
            return;
        }

        let Some(mut pending) = self.pending_requirements.remove(key) else {
            return;
        };

        let mut remaining = Vec::new();
        for requirement in pending.drain(..) {
            let dependency_requirement = DependencyRequirement { key: key.clone() };
            if !self.try_attach_requirement(requirement.dependant, &dependency_requirement) {
                remaining.push(requirement);
            }
        }

        if !remaining.is_empty() {
            self.pending_requirements.insert(key.clone(), remaining);
        }
    }

    fn len(&self) -> usize {
        self.graph.node_count()
    }

    fn contains(&self, key: &NodeKey) -> bool {
        self.key_to_index.contains_key(key)
    }

    fn find(&self, key: &NodeKey) -> Option<NodeEntity> {
        self.key_to_index
            .get(key)
            .and_then(|index| self.graph.node_weight(*index))
            .cloned()
    }

    fn find_by_instance_id(&self, instance_id: &Name) -> Option<TrackedNodeInstance> {
        self.graph
            .node_weights()
            .flat_map(|entity| entity.instances())
            .find(|inst| inst.instance_id() == instance_id)
            .cloned()
    }

    fn find_entity_by_instance_id(&self, instance_id: &Name) -> Option<NodeEntity> {
        self.graph
            .node_weights()
            .find(|entity| {
                entity
                    .instances()
                    .iter()
                    .any(|inst| inst.instance_id() == instance_id)
            })
            .cloned()
    }

    fn root(&self) -> NodeEntity {
        self.find(&self.root_key)
            .expect("root node must always exist in NodeStack")
    }

    fn is_root(&self, key: &NodeKey) -> bool {
        &self.root_key == key
    }

    fn entities_snapshot(&self) -> Vec<NodeEntity> {
        self.graph.node_weights().cloned().collect()
    }

    fn dependencies_of(&self, key: &NodeKey) -> Vec<NodeEntity> {
        self.key_to_index
            .get(key)
            .map(|index| {
                self.graph
                    .neighbors_directed(*index, Direction::Outgoing)
                    .filter_map(|dep_index| self.graph.node_weight(dep_index))
                    .cloned()
                    .collect()
            })
            .unwrap_or_default()
    }

    fn dependents_of(&self, key: &NodeKey) -> Vec<NodeEntity> {
        self.key_to_index
            .get(key)
            .map(|index| {
                self.graph
                    .neighbors_directed(*index, Direction::Incoming)
                    .filter_map(|dep_index| self.graph.node_weight(dep_index))
                    .cloned()
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Adds a config to the stack or updates an existing one.
    /// Does not create any instances.
    /// When the entity already exists, config and root_path are always updated.
    /// Dependency checks and rewiring only occur when interfaces change.
    /// Returns Err(CannotModifyRootNode) if trying to modify the root node config.
    /// Returns Err(CannotOverwriteNodeWithDependents) if interfaces change and the node has dependents.
    fn push_config_impl<P: Into<PathBuf>>(
        &mut self,
        config: NodeConfig,
        allow_missing_dependencies: bool,
        root_path: P,
    ) -> Result<()> {
        let key = NodeKey::new(config.manifest.name.as_str(), &config.manifest.tag);
        let root_path = root_path.into();

        // The root node cannot be modified
        if self.is_root(&key) {
            return Err(Error::CannotModifyRootNode);
        }

        if let Some(&index) = self.key_to_index.get(&key) {
            let interfaces_changed = self.graph.node_weight(index).is_some_and(|entity| {
                !interfaces_match(&entity.config().interfaces, &config.interfaces)
            });

            // Dependency checks and rewiring are only needed when interfaces change,
            // because interface changes can break or create dependency relationships.
            if interfaces_changed {
                let has_dependents = self
                    .graph
                    .neighbors_directed(index, Direction::Incoming)
                    .next()
                    .is_some()
                    || self
                        .pending_requirements
                        .get(&key)
                        .is_some_and(|requirements| !requirements.is_empty());

                if has_dependents {
                    return Err(Error::CannotOverwriteNodeWithDependents {
                        node_name: key.name,
                        node_tag: key.tag,
                    });
                }

                if !allow_missing_dependencies {
                    let candidate = NodeEntity::new(config.clone(), root_path.clone());
                    self.validate_dependencies(&candidate)?;
                }
            }

            // Always update config and root_path. Non-breaking changes (start_cmd,
            // add_cmd, labels, parameters) must not be silently dropped.
            if let Some(entity) = self.graph.node_weight_mut(index) {
                entity.config = config;
                entity.fs_root_path = root_path;
            }

            if interfaces_changed {
                self.rewire_dependencies(index);
            }
        } else {
            // Entity doesn't exist, create new one without instances
            let entity = NodeEntity::new(config, root_path);
            if allow_missing_dependencies {
                self.insert_entity_lenient(entity)?;
            } else {
                self.insert_entity(entity)?;
            }
        }

        Ok(())
    }

    /// Add a new instance for an existing config.
    /// If instance_id is None, generates a random one.
    /// Returns the instance_id that was used.
    /// Returns Err(NoMatchingNode) if the config is not found in the stack.
    /// Returns Err(CannotModifyRootNode) if trying to add an instance to the root node.
    /// Returns Err(DuplicateInstanceId) if the instance_id already exists for this entity.
    fn add_instance_impl(
        &mut self,
        name: &str,
        tag: &str,
        instance_id: Option<&Name>,
        pid: Option<u32>,
    ) -> Result<Name> {
        let key = NodeKey::new(name, tag);

        // The root node always has exactly one instance and cannot be modified
        if self.is_root(&key) {
            return Err(Error::CannotModifyRootNode);
        }

        let instance_id = match instance_id {
            Some(id) => id.clone(),
            None => Name::new(get_random(rng())).map_err(|e| Error::Config(e.into()))?,
        };

        let Some(&index) = self.key_to_index.get(&key) else {
            return Err(Error::NoMatchingNode(name.to_owned(), tag.to_owned()));
        };

        if let Some(entity) = self.graph.node_weight_mut(index) {
            // Check if instance_id already exists
            if entity
                .instances()
                .iter()
                .any(|inst| inst.instance_id() == &instance_id)
            {
                return Err(Error::DuplicateInstanceId {
                    instance_id: instance_id.as_str().to_owned(),
                    node_name: name.to_owned(),
                    node_tag: tag.to_owned(),
                });
            }
            let instance = TrackedNodeInstance::new(instance_id.clone(), pid);
            entity.add_instance(instance);
        }

        Ok(instance_id)
    }

    /// Removes an instance from an entity. If the entity has no instances left, removes the entity.
    /// Returns Ok(true) if the instance was found and removed, Ok(false) if not found.
    /// Returns Err(CannotRemoveRootNode) if trying to remove an instance from the root node.
    /// The root node always has exactly one instance and cannot be modified.
    fn remove_instance(&mut self, name: &str, tag: &str, instance_id: &Name) -> Result<bool> {
        let key = NodeKey::new(name, tag);

        // The root node always has exactly one instance and cannot be modified
        if self.is_root(&key) {
            return Err(Error::CannotModifyRootNode);
        }

        let Some(&index) = self.key_to_index.get(&key) else {
            return Ok(false);
        };

        let should_remove_entity = {
            let Some(entity) = self.graph.node_weight_mut(index) else {
                return Ok(false);
            };

            if !entity.remove_instance(instance_id) {
                return Ok(false);
            }

            entity.instance_count() == 0
        };

        if should_remove_entity {
            self.remove_entity(&key);
        }

        Ok(true)
    }

    /// Removes an entity entirely from the graph
    fn remove_entity(&mut self, key: &NodeKey) {
        if let Some(index) = self.key_to_index.remove(key) {
            self.graph.remove_node(index);
            self.clear_pending_requirements_for(index);
        }
    }

    /// Clears all nodes except the root node from the stack.
    /// The root node is preserved as it cannot be removed.
    fn clear(&mut self) {
        // Get the root entity before clearing
        let root_entity = self.root();

        // Clear everything
        self.graph.clear();
        self.key_to_index.clear();
        self.pending_requirements.clear();

        // Re-insert the root node
        let idx = self.graph.add_node(root_entity);
        self.key_to_index.insert(self.root_key.clone(), idx);
    }

    /// Returns the graph in DOT format for visualization.
    fn to_dot(&self) -> String {
        let dot = Dot::with_attr_getters(
            &self.graph,
            &[Config::EdgeNoLabel, Config::NodeNoLabel],
            &|_, _| String::new(),
            &|_, (_, node)| {
                let name = node.config().manifest.name.as_str();
                let tag = &node.config().manifest.tag;
                let instance_count = node.instances().len();
                format!(
                    "label=\"{}:{}\\n({} instance{})\"",
                    name,
                    tag,
                    instance_count,
                    if instance_count == 1 { "" } else { "s" }
                )
            },
        );
        format!("{:?}", dot)
    }

    /// Returns a serializable representation of the graph.
    fn to_serialized_graph(&self) -> SerializedNodeGraph {
        let nodes: Vec<SerializedNode> = self
            .graph
            .node_weights()
            .map(|entity| SerializedNode {
                name: entity.config().manifest.name.as_str().to_string(),
                tag: entity.config().manifest.tag.clone(),
                instance_ids: entity
                    .instances()
                    .iter()
                    .map(|i| i.instance_id().as_str().to_string())
                    .collect(),
                fs_root_path: entity.root_path().display().to_string(),
            })
            .collect();

        let edges: Vec<SerializedEdge> = self
            .graph
            .edge_indices()
            .filter_map(|edge_idx| {
                let (src_idx, dst_idx) = self.graph.edge_endpoints(edge_idx)?;
                let src_entity = self.graph.node_weight(src_idx)?;
                let dst_entity = self.graph.node_weight(dst_idx)?;
                Some(SerializedEdge {
                    from: SerializedNode {
                        name: src_entity.config().manifest.name.as_str().to_string(),
                        tag: src_entity.config().manifest.tag.clone(),
                        instance_ids: src_entity
                            .instances()
                            .iter()
                            .map(|i| i.instance_id().as_str().to_string())
                            .collect(),
                        fs_root_path: src_entity.root_path().display().to_string(),
                    },
                    to: SerializedNode {
                        name: dst_entity.config().manifest.name.as_str().to_string(),
                        tag: dst_entity.config().manifest.tag.clone(),
                        instance_ids: dst_entity
                            .instances()
                            .iter()
                            .map(|i| i.instance_id().as_str().to_string())
                            .collect(),
                        fs_root_path: dst_entity.root_path().display().to_string(),
                    },
                })
            })
            .collect();

        SerializedNodeGraph { nodes, edges }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct DependencyRequirement {
    key: NodeKey,
}

#[derive(Clone, Debug)]
struct PendingRequirement {
    dependant: NodeIndex,
}

fn dependency_requirements(node: &NodeConfig) -> Vec<DependencyRequirement> {
    collect_dependency_specs(node)
        .into_iter()
        .map(|spec| DependencyRequirement {
            key: NodeKey::new(&spec.node_name, &spec.node_tag),
        })
        .collect()
}

#[derive(Clone)]
pub struct NodeStack {
    shared: Arc<RwLock<NodeStackInner>>,
}

impl NodeStack {
    /// Creates a new NodeStack with the given root node configuration.
    /// The root node (core node) is the parent of all other nodes in the graph
    /// and cannot be removed from the stack.
    ///
    /// If `instance_id` is `None`, a random instance ID will be generated for the root node.
    ///
    /// # Arguments
    ///
    /// * `root_config` - The configuration for the root node (core node).
    /// * `instance_id` - Optional instance ID for the root node. If `None`, a random ID is generated.
    /// * `root_path` - The filesystem path where the root node will be stored.
    pub fn new<P: Into<PathBuf>>(
        root_config: NodeConfig,
        instance_id: Option<Name>,
        root_path: P,
    ) -> Self {
        let instance_id = instance_id.unwrap_or_else(|| {
            Name::new(get_random(rng())).expect("random name generation failed")
        });
        let instance = TrackedNodeInstance::new(instance_id, Some(std::process::id()));
        let mut root_entity = NodeEntity::new(root_config, root_path);
        root_entity.add_instance(instance);
        Self {
            shared: Arc::new(RwLock::new(NodeStackInner::new(root_entity))),
        }
    }

    pub fn len(&self) -> usize {
        self.shared.read().expect("node stack poisoned").len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Returns the root node (core node) of this stack.
    /// The root node is guaranteed to always exist.
    pub fn root(&self) -> NodeEntity {
        let guard = self.shared.read().expect("node stack poisoned");
        guard.root()
    }

    pub fn contains(&self, name: &str, tag: &str) -> bool {
        let guard = self.shared.read().expect("node stack poisoned");
        guard.contains(&NodeKey::new(name, tag))
    }

    pub fn find(&self, name: &str, tag: &str) -> Option<NodeEntity> {
        let guard = self.shared.read().expect("node stack poisoned");
        guard.find(&NodeKey::new(name, tag))
    }

    /// Finds a node instance by its instance_id across all entities in the stack.
    /// Returns the NodeInstance if found, None otherwise.
    pub fn find_by_instance_id(&self, instance_id: &Name) -> Option<TrackedNodeInstance> {
        let guard = self.shared.read().expect("node stack poisoned");
        guard.find_by_instance_id(instance_id)
    }

    /// Finds a node entity by an instance_id it contains.
    /// Returns the NodeEntity if found, None otherwise.
    pub fn find_entity_by_instance_id(&self, instance_id: &Name) -> Option<NodeEntity> {
        let guard = self.shared.read().expect("node stack poisoned");
        guard.find_entity_by_instance_id(instance_id)
    }

    /// Adds a config to the stack or updates an existing one.
    /// Does not create any instances.
    /// If allow_missing_dependencies is true, missing dependencies are tracked as pending
    /// requirements and will be wired once the dependency nodes are added to the stack.
    pub fn push_config<P: Into<PathBuf>>(
        &self,
        config: NodeConfig,
        allow_missing_dependencies: bool,
        root_path: P,
    ) -> Result<()> {
        let mut guard = self.shared.write().expect("node stack poisoned");
        guard.push_config_impl(config, allow_missing_dependencies, root_path)
    }

    /// Add a new instance for an existing config.
    /// If instance_id is None, generates a random one.
    /// Returns the instance_id that was used.
    /// Returns Err(NoMatchingNode) if the config is not found in the stack.
    /// Returns Err(CannotModifyRootNode) if trying to add an instance to the root node.
    /// Returns Err(DuplicateInstanceId) if the instance_id already exists for this entity.
    pub fn add_instance(
        &self,
        node_name: &str,
        tag: &str,
        instance_id: Option<&Name>,
        pid: Option<u32>,
    ) -> Result<Name> {
        let mut guard = self.shared.write().expect("node stack poisoned");
        guard.add_instance_impl(node_name, tag, instance_id, pid)
    }

    pub fn snapshot(&self) -> Vec<NodeEntity> {
        let guard = self.shared.read().expect("node stack poisoned");
        guard.entities_snapshot()
    }

    pub fn dependencies_of(&self, name: &str, tag: &str) -> Vec<NodeEntity> {
        let guard = self.shared.read().expect("node stack poisoned");
        guard.dependencies_of(&NodeKey::new(name, tag))
    }

    pub fn dependents_of(&self, name: &str, tag: &str) -> Vec<NodeEntity> {
        let guard = self.shared.read().expect("node stack poisoned");
        guard.dependents_of(&NodeKey::new(name, tag))
    }

    /// Removes an instance from an entity. If the entity has no instances left, removes the entity.
    /// Returns Ok(true) if the instance was found and removed, Ok(false) if not found.
    /// Returns Err(CannotModifyRootNode) if trying to modify the root node.
    /// The root node always has exactly one instance and cannot be modified.
    pub fn remove_instance(&self, name: &str, tag: &str, instance_id: &Name) -> Result<bool> {
        let mut guard = self.shared.write().expect("node stack poisoned");
        guard.remove_instance(name, tag, instance_id)
    }

    /// Removes a node configuration if it has no instances.
    ///
    /// Returns Ok(true) if the config was found and removed, Ok(false) if not found.
    /// Returns Err(CannotModifyRootNode) if trying to remove the root node.
    /// Returns Err(CannotRemoveNodeWithInstances) if the node still has instances.
    pub fn remove_config(&self, name: &str, tag: &str) -> Result<bool> {
        let mut guard = self.shared.write().expect("node stack poisoned");
        let key = NodeKey::new(name, tag);

        if guard.is_root(&key) {
            return Err(Error::CannotModifyRootNode);
        }

        let Some(&index) = guard.key_to_index.get(&key) else {
            return Ok(false);
        };

        let has_instances = guard
            .graph
            .node_weight(index)
            .map(|entity| !entity.instances().is_empty())
            .unwrap_or(false);

        if has_instances {
            return Err(Error::CannotRemoveNodeWithInstances {
                node_name: name.to_string(),
                node_tag: tag.to_string(),
            });
        }

        guard.remove_entity(&key);
        Ok(true)
    }

    /// Clears all nodes except the root node from the stack.
    /// The root node is preserved as it cannot be removed.
    pub fn reset(&self) {
        let mut guard = self.shared.write().expect("node stack poisoned");
        guard.clear();
    }

    /// Applies the state from another NodeStack to this one.
    ///
    /// This resets the current stack (preserving only the root node), then copies
    /// all non-root entities and their instances from the source stack.
    ///
    /// Returns an error if any config or instance fails to be added.
    pub fn apply_from(&self, source: &NodeStack) -> std::result::Result<(), String> {
        let target_root = self.root();
        let target_root_name = target_root.config().manifest.name.as_str().to_owned();
        let target_root_tag = target_root.config().manifest.tag.clone();

        self.reset();

        for entity in source.snapshot() {
            let config = entity.config();

            // Skip the root node from the source stack
            if config.manifest.name.as_str() == target_root_name.as_str()
                && config.manifest.tag == target_root_tag
            {
                continue;
            }

            // First, push the config
            self.push_config(config.clone(), true, entity.root_path())
                .map_err(|e| {
                    format!(
                        "failed to add config {}:{} to node stack: {e}",
                        config.manifest.name.as_str(),
                        config.manifest.tag,
                    )
                })?;

            // Then add each instance
            for instance in entity.instances() {
                self.add_instance(
                    config.manifest.name.as_str(),
                    &config.manifest.tag,
                    Some(instance.instance_id()),
                    instance.pid(),
                )
                .map_err(|e| {
                    format!(
                        "failed to add instance {} for {}:{} to node stack: {e}",
                        instance.instance_id().as_str(),
                        config.manifest.name.as_str(),
                        config.manifest.tag,
                    )
                })?;
            }
        }

        Ok(())
    }

    /// Returns the graph in DOT format for visualization.
    pub fn to_dot(&self) -> String {
        let guard = self.shared.read().expect("node stack poisoned");
        guard.to_dot()
    }

    /// Returns a serializable representation of the graph.
    pub fn to_serialized_graph(&self) -> SerializedNodeGraph {
        let guard = self.shared.read().expect("node stack poisoned");
        guard.to_serialized_graph()
    }
}
