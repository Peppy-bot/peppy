use crate::error::{Error, Result};
use config::node::{Name, NodeConfig};
use config::peppy_config::{Deployment, DeploymentNodeSource};
use names_generator2::get_random;
use petgraph::{
    Direction,
    stable_graph::{NodeIndex, StableDiGraph},
    visit::EdgeRef,
};
use rand::rng;
use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    sync::{Arc, RwLock},
};

/// This represent a single entity with n instances inside the node_stack.
/// A NodeEntity always has at least one instance.
#[derive(Clone)]
pub struct NodeEntity {
    config: NodeConfig,
    root_path: Option<PathBuf>,
    instances: Vec<NodeInstance>,
}

impl NodeEntity {
    /// Creates a new NodeEntity with a single instance
    pub fn new(config: NodeConfig, instance: NodeInstance) -> Self {
        Self {
            config,
            root_path: None,
            instances: vec![instance],
        }
    }

    /// Creates a new NodeEntity with a single instance and a root path
    pub fn with_path<P: Into<PathBuf>>(
        config: NodeConfig,
        instance: NodeInstance,
        root_path: P,
    ) -> Self {
        Self {
            config,
            root_path: Some(root_path.into()),
            instances: vec![instance],
        }
    }

    /// Creates a NodeEntity from a list of instances (must have at least one)
    pub fn from_instances(config: NodeConfig, instances: Vec<NodeInstance>) -> Self {
        assert!(
            !instances.is_empty(),
            "NodeEntity must have at least one instance"
        );
        Self {
            config,
            root_path: None,
            instances,
        }
    }

    pub fn config(&self) -> &NodeConfig {
        &self.config
    }

    pub fn into_config(self) -> NodeConfig {
        self.config
    }

    pub fn root_path(&self) -> Option<&PathBuf> {
        self.root_path.as_ref()
    }

    pub fn instances(&self) -> &[NodeInstance] {
        &self.instances
    }

    /// Adds an instance to this entity
    fn add_instance(&mut self, instance: NodeInstance) {
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
pub struct ResolvedNodeSource {
    source: Option<DeploymentNodeSource>,
    node: NodeConfig,
}

impl ResolvedNodeSource {
    pub fn new(source: Option<DeploymentNodeSource>, node: NodeConfig) -> Self {
        Self { source, node }
    }

    pub fn source(&self) -> Option<&DeploymentNodeSource> {
        self.source.as_ref()
    }

    pub fn node(&self) -> &NodeConfig {
        &self.node
    }

    pub fn into_node(self) -> NodeConfig {
        self.node
    }

    pub fn into_parts(self) -> (Option<DeploymentNodeSource>, NodeConfig) {
        (self.source, self.node)
    }
}

#[derive(Debug)]
pub struct DeploymentMap {
    deployment: Deployment,
    // Contains an error explaining why the deployment could be be resolved or the actual resolved node
    node_source: Result<ResolvedNodeSource>,
}

impl DeploymentMap {
    pub fn new(deployment: Deployment, node_source: ResolvedNodeSource) -> Self {
        Self {
            deployment,
            node_source: Ok(node_source),
        }
    }

    pub fn unresolved(deployment: Deployment, error: Error) -> Self {
        Self {
            deployment,
            node_source: Err(error),
        }
    }

    pub fn deployment(&self) -> &Deployment {
        &self.deployment
    }

    pub fn node_source(&self) -> &ResolvedNodeSource {
        self.node_source
            .as_ref()
            .expect("deployment is unresolved; call error() to inspect failure")
    }

    pub fn is_resolved(&self) -> bool {
        self.node_source.is_ok()
    }

    pub fn error(&self) -> Option<&Error> {
        self.node_source.as_ref().err()
    }
}

#[derive(Debug, Clone)]
pub struct NodeInstance {
    instance_id: Name,
}

impl NodeInstance {
    pub fn new(instance_id: Name) -> Self {
        Self { instance_id }
    }

    pub fn instance_id(&self) -> &Name {
        &self.instance_id
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

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(super) enum InterfaceKind {
    Topic,
    Service,
    Action,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(super) struct InterfaceRequirement {
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

    pub(super) fn kind(&self) -> InterfaceKind {
        self.kind
    }

    pub(super) fn name(&self) -> &str {
        &self.name
    }
}

#[derive(Clone, Debug)]
pub(super) struct DependencySpec {
    pub(super) node_name: String,
    pub(super) node_tag: String,
    pub(super) interface: InterfaceRequirement,
}

pub(super) fn collect_dependency_specs(node: &NodeConfig) -> Vec<DependencySpec> {
    let Some(subscriptions) = node.interfaces.subscribes_to.as_ref() else {
        return Vec::new();
    };

    let mut specs: HashMap<(String, String), HashSet<InterfaceRequirement>> = HashMap::new();

    if let Some(topics) = subscriptions.topics.as_ref() {
        for topic in topics {
            let node_name = topic.node.trim();
            if node_name.is_empty() {
                continue;
            }

            let interface_name = topic.name.trim();
            if interface_name.is_empty() {
                continue;
            }

            let requirement = InterfaceRequirement::new(InterfaceKind::Topic, interface_name);
            let tag = topic.tag.trim().to_owned();
            specs
                .entry((node_name.to_owned(), tag))
                .or_default()
                .insert(requirement);
        }
    }

    if let Some(services) = subscriptions.services.as_ref() {
        for service in services {
            let node_name = service.node.trim();
            if node_name.is_empty() {
                continue;
            }

            let interface_name = service.name.trim();
            if interface_name.is_empty() {
                continue;
            }

            let requirement = InterfaceRequirement::new(InterfaceKind::Service, interface_name);
            let tag = service.tag.trim().to_owned();
            specs
                .entry((node_name.to_owned(), tag))
                .or_default()
                .insert(requirement);
        }
    }

    if let Some(actions) = subscriptions.actions.as_ref() {
        for action in actions {
            let node_name = action.node.trim();
            if node_name.is_empty() {
                continue;
            }

            let interface_name = action.name.trim();
            if interface_name.is_empty() {
                continue;
            }

            let requirement = InterfaceRequirement::new(InterfaceKind::Action, interface_name);
            let tag = action.tag.trim().to_owned();
            specs
                .entry((node_name.to_owned(), tag))
                .or_default()
                .insert(requirement);
        }
    }

    specs
        .into_iter()
        .flat_map(|((name, tag), requirements)| {
            requirements
                .into_iter()
                .map(move |interface| DependencySpec {
                    node_name: name.clone(),
                    node_tag: tag.clone(),
                    interface,
                })
        })
        .collect()
}

pub(super) fn exposes_interface(node: &NodeConfig, requirement: &InterfaceRequirement) -> bool {
    let Some(exposes) = node.interfaces.exposes.as_ref() else {
        return false;
    };

    match requirement.kind() {
        InterfaceKind::Topic => exposes.topics.as_ref().map_or(false, |topics| {
            topics
                .iter()
                .any(|topic| topic.name.trim() == requirement.name())
        }),
        InterfaceKind::Service => exposes.services.as_ref().map_or(false, |services| {
            services
                .iter()
                .any(|service| service.name.trim() == requirement.name())
        }),
        InterfaceKind::Action => exposes.actions.as_ref().map_or(false, |actions| {
            actions
                .iter()
                .any(|action| action.name.trim() == requirement.name())
        }),
    }
}

pub(super) fn interface_kind_label(kind: InterfaceKind) -> &'static str {
    match kind {
        InterfaceKind::Topic => "topic",
        InterfaceKind::Service => "service",
        InterfaceKind::Action => "action",
    }
}

struct NodeStackInner {
    graph: StableDiGraph<NodeEntity, ()>,
    key_to_index: HashMap<NodeKey, NodeIndex>,
    pending_requirements: HashMap<NodeKey, Vec<PendingRequirement>>,
}

impl NodeStackInner {
    fn new() -> Self {
        Self {
            graph: StableDiGraph::default(),
            key_to_index: HashMap::new(),
            pending_requirements: HashMap::new(),
        }
    }

    fn insert_entity(&mut self, node: NodeEntity) {
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
        if entry.iter().any(|pending| {
            pending.dependant == dependant && pending.interface == requirement.interface
        }) {
            return;
        }
        entry.push(PendingRequirement {
            dependant,
            interface: requirement.interface,
        });
    }

    fn try_attach_requirement(
        &mut self,
        dependant_index: NodeIndex,
        requirement: &DependencyRequirement,
    ) -> bool {
        let Some(&dependency_index) = self.key_to_index.get(&requirement.key) else {
            return false;
        };
        let Some(dependency_node) = self.graph.node_weight(dependency_index) else {
            return false;
        };

        if exposes_interface(dependency_node.config(), &requirement.interface) {
            if self
                .graph
                .find_edge(dependant_index, dependency_index)
                .is_none()
            {
                self.graph.add_edge(dependant_index, dependency_index, ());
            }
            true
        } else {
            false
        }
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
            let dependency_requirement = DependencyRequirement {
                key: key.clone(),
                interface: requirement.interface.clone(),
            };
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

    /// Adds an instance to an existing entity or creates a new entity if not present.
    /// If instance_id is None, generates a random one.
    /// Returns the instance_id that was used.
    fn add_instance(&mut self, config: &NodeConfig, instance_id: Option<&Name>) -> Result<Name> {
        let instance_id = match instance_id {
            Some(id) => id.clone(),
            None => Name::new(get_random(rng())).map_err(|e| Error::Config(e.into()))?,
        };

        let key = NodeKey::new(config.manifest.name.as_str(), &config.manifest.tag);

        if let Some(&index) = self.key_to_index.get(&key) {
            // Entity exists, add instance to it
            if let Some(entity) = self.graph.node_weight_mut(index) {
                let instance = NodeInstance::new(instance_id.clone());
                entity.add_instance(instance);
            }
        } else {
            // Entity doesn't exist, create new one
            let instance = NodeInstance::new(instance_id.clone());
            let entity = NodeEntity::new(config.clone(), instance);
            self.insert_entity(entity);
        }

        Ok(instance_id)
    }

    /// Removes an instance from an entity. If the entity has no instances left, removes the entity.
    /// Returns true if the instance was found and removed.
    fn remove_instance(&mut self, name: &str, tag: &str, instance_id: &Name) -> bool {
        let key = NodeKey::new(name, tag);

        let Some(&index) = self.key_to_index.get(&key) else {
            return false;
        };

        let should_remove_entity = {
            let Some(entity) = self.graph.node_weight_mut(index) else {
                return false;
            };

            if !entity.remove_instance(instance_id) {
                return false;
            }

            entity.instance_count() == 0
        };

        if should_remove_entity {
            self.remove_entity(&key);
        }

        true
    }

    /// Removes an entity entirely from the graph
    fn remove_entity(&mut self, key: &NodeKey) {
        if let Some(index) = self.key_to_index.remove(key) {
            self.graph.remove_node(index);
            self.clear_pending_requirements_for(index);
        }
    }

    /// Clears the entire stack
    fn clear(&mut self) {
        self.graph.clear();
        self.key_to_index.clear();
        self.pending_requirements.clear();
    }
}

#[derive(Clone, Debug)]
struct DependencyRequirement {
    key: NodeKey,
    interface: InterfaceRequirement,
}

#[derive(Clone, Debug)]
struct PendingRequirement {
    dependant: NodeIndex,
    interface: InterfaceRequirement,
}

fn dependency_requirements(node: &NodeConfig) -> Vec<DependencyRequirement> {
    collect_dependency_specs(node)
        .into_iter()
        .map(|spec| DependencyRequirement {
            key: NodeKey::new(&spec.node_name, &spec.node_tag),
            interface: spec.interface,
        })
        .collect()
}

#[derive(Clone)]
pub struct NodeStack {
    shared: Arc<RwLock<NodeStackInner>>,
}

impl Default for NodeStack {
    fn default() -> Self {
        Self::new()
    }
}

impl NodeStack {
    pub fn new() -> Self {
        Self {
            shared: Arc::new(RwLock::new(NodeStackInner::new())),
        }
    }

    pub fn from_configs(nodes: Vec<NodeConfig>) -> Self {
        let stack = Self::new();
        for config in nodes {
            stack.push_config(config);
        }
        stack
    }

    pub fn len(&self) -> usize {
        self.shared.read().expect("node stack poisoned").len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn contains(&self, name: &str, tag: &str) -> bool {
        let guard = self.shared.read().expect("node stack poisoned");
        guard.contains(&NodeKey::new(name, tag))
    }

    pub fn find(&self, name: &str, tag: &str) -> Option<NodeEntity> {
        let guard = self.shared.read().expect("node stack poisoned");
        guard.find(&NodeKey::new(name, tag))
    }

    pub fn push_config(&self, config: NodeConfig) {
        let instance_id = Name::new(get_random(rng())).expect("random name generation failed");
        self.push_config_with_instance_id(config, instance_id);
    }

    pub fn push_config_with_instance_id(&self, config: NodeConfig, instance_id: Name) {
        let instance = NodeInstance::new(instance_id);
        let entity = NodeEntity::new(config, instance);
        self.push_entity(entity);
    }

    fn push_entity(&self, entity: NodeEntity) {
        let mut guard = self.shared.write().expect("node stack poisoned");
        guard.insert_entity(entity);
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

    /// Adds an instance to an existing entity or creates a new entity if not present.
    /// If instance_id is None, generates a random one.
    /// Returns the instance_id that was used.
    pub fn add_instance(&self, config: &NodeConfig, instance_id: Option<&Name>) -> Result<Name> {
        let mut guard = self.shared.write().expect("node stack poisoned");
        guard.add_instance(config, instance_id)
    }

    /// Removes an instance from an entity. If the entity has no instances left, removes the entity.
    /// Returns true if the instance was found and removed.
    pub fn remove_instance(&self, name: &str, tag: &str, instance_id: &Name) -> bool {
        let mut guard = self.shared.write().expect("node stack poisoned");
        guard.remove_instance(name, tag, instance_id)
    }

    /// Clears the entire stack, removing all entities and instances.
    pub fn reset(&self) {
        let mut guard = self.shared.write().expect("node stack poisoned");
        guard.clear();
    }
}
