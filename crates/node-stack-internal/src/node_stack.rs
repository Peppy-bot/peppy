mod entity;
mod validation;

pub use entity::{DependencySpec, NodeEntity, SerializedNodeGraph};
pub use validation::{collect_dependency_specs, validate_dependency_specs};

use entity::{SerializedEdge, SerializedNode, TrackedNodeInstance};

use crate::error::{Error, Result};
use config::node::{Name, NodeConfig};
use names_generator2::get_random;
use petgraph::{
    Direction,
    dot::{Config, Dot},
    stable_graph::{NodeIndex, StableDiGraph},
    visit::EdgeRef,
};
use rand::rng;
use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{Arc, RwLock},
};

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

fn dependency_keys(node: &NodeConfig) -> Vec<NodeKey> {
    collect_dependency_specs(node)
        .into_iter()
        .map(|spec| NodeKey::new(&spec.node_name, &spec.node_tag))
        .collect()
}

struct NodeStackInner {
    graph: StableDiGraph<NodeEntity, ()>,
    key_to_index: HashMap<NodeKey, NodeIndex>,
    pending_requirements: HashMap<NodeKey, Vec<NodeIndex>>,
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
            .insert_entity(root, true)
            .expect("root node should have no dependencies");
        inner
    }

    fn insert_entity(&mut self, node: NodeEntity, validate: bool) -> Result<()> {
        if validate {
            self.validate_dependencies(&node)?;
        }

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
            &node.config().manifest,
            &node.config().interfaces,
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
        let keys = if let Some(node) = self.graph.node_weight(index) {
            dependency_keys(node.config())
        } else {
            return;
        };
        self.clear_pending_requirements_for(index);
        for dep_key in keys {
            if !self.try_attach_edge(index, &dep_key) {
                self.register_pending_requirement(dep_key, index);
            }
        }
    }

    fn clear_pending_requirements_for(&mut self, dependant: NodeIndex) {
        self.pending_requirements.retain(|_, pending| {
            pending.retain(|&idx| idx != dependant);
            !pending.is_empty()
        });
    }

    fn register_pending_requirement(&mut self, dep_key: NodeKey, dependant: NodeIndex) {
        let entry = self.pending_requirements.entry(dep_key).or_default();
        if !entry.contains(&dependant) {
            entry.push(dependant);
        }
    }

    fn try_attach_edge(&mut self, dependant_index: NodeIndex, dep_key: &NodeKey) -> bool {
        let Some(&dependency_index) = self.key_to_index.get(dep_key) else {
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

        let Some(pending) = self.pending_requirements.remove(key) else {
            return;
        };

        let mut remaining = Vec::new();
        for dependant_index in pending {
            if !self.try_attach_edge(dependant_index, key) {
                remaining.push(dependant_index);
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
            let interfaces_changed = self
                .graph
                .node_weight(index)
                .is_some_and(|entity| entity.config().interfaces != config.interfaces);

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
            self.insert_entity(entity, !allow_missing_dependencies)?;
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

    /// Removes an instance from an entity.
    /// Returns Ok(true) if the instance was found and removed, Ok(false) if not found.
    /// Returns Err(CannotModifyRootNode) if trying to modify the root node.
    fn remove_instance(&mut self, name: &str, tag: &str, instance_id: &Name) -> Result<bool> {
        let key = NodeKey::new(name, tag);

        if self.is_root(&key) {
            return Err(Error::CannotModifyRootNode);
        }

        let Some(&index) = self.key_to_index.get(&key) else {
            return Ok(false);
        };

        let Some(entity) = self.graph.node_weight_mut(index) else {
            return Ok(false);
        };

        if !entity.remove_instance(instance_id) {
            return Ok(false);
        }

        Ok(true)
    }

    /// Removes an entity entirely from the graph.
    fn remove_entity(&mut self, key: &NodeKey) {
        if let Some(index) = self.key_to_index.remove(key) {
            self.graph.remove_node(index);
            self.clear_pending_requirements_for(index);
        }
    }

    /// Clears all nodes except the root node from the stack.
    fn clear(&mut self) {
        let root_entity = self.root();

        self.graph.clear();
        self.key_to_index.clear();
        self.pending_requirements.clear();

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
        let nodes = self
            .graph
            .node_weights()
            .map(SerializedNode::from)
            .collect();

        let edges = self
            .graph
            .edge_indices()
            .filter_map(|edge_idx| {
                let (src_idx, dst_idx) = self.graph.edge_endpoints(edge_idx)?;
                let src_entity = self.graph.node_weight(src_idx)?;
                let dst_entity = self.graph.node_weight(dst_idx)?;
                Some(SerializedEdge {
                    from: SerializedNode::from(src_entity),
                    to: SerializedNode::from(dst_entity),
                })
            })
            .collect();

        SerializedNodeGraph { nodes, edges }
    }
}

// ---------------------------------------------------------------------------
// Public thread-safe wrapper
// ---------------------------------------------------------------------------

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
    pub fn find_by_instance_id(&self, instance_id: &Name) -> Option<TrackedNodeInstance> {
        let guard = self.shared.read().expect("node stack poisoned");
        guard.find_by_instance_id(instance_id)
    }

    /// Finds a node entity by an instance_id it contains.
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

    /// Removes an instance from an entity.
    /// Returns Ok(true) if the instance was found and removed, Ok(false) if not found.
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
    pub fn reset(&self) {
        let mut guard = self.shared.write().expect("node stack poisoned");
        guard.clear();
    }

    /// Applies the state from another NodeStack to this one.
    ///
    /// This resets the current stack (preserving only the root node), then copies
    /// all non-root entities and their instances from the source stack.
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
