use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    sync::{Arc, RwLock},
};

use crate::error::{Error, Result};
use config::node::NodeConfig;
use config::peppy_config::{Deployment, DeploymentNodeSource};
use petgraph::{
    Direction,
    stable_graph::{NodeIndex, StableDiGraph},
    visit::EdgeRef,
};

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
    config: NodeConfig,
    filepath: Option<PathBuf>,
}

impl NodeInstance {
    pub fn new(config: NodeConfig) -> Self {
        Self {
            config,
            filepath: None,
        }
    }

    pub fn with_path<P: Into<PathBuf>>(config: NodeConfig, filepath: P) -> Self {
        Self {
            config,
            filepath: Some(filepath.into()),
        }
    }

    pub fn config(&self) -> &NodeConfig {
        &self.config
    }

    pub fn file_path(&self) -> Option<&PathBuf> {
        self.filepath.as_ref()
    }

    pub fn into_config(self) -> NodeConfig {
        self.config
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

impl From<&NodeInstance> for NodeKey {
    fn from(instance: &NodeInstance) -> Self {
        NodeKey::new(
            instance.config.manifest.name.as_str(),
            &instance.config.manifest.tag,
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
    graph: StableDiGraph<NodeInstance, ()>,
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

    fn from_instances(nodes: Vec<NodeInstance>) -> Self {
        let mut inner = Self::new();
        for node in nodes {
            inner.insert_node(node);
        }
        inner
    }

    fn insert_node(&mut self, node: NodeInstance) {
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

    fn find(&self, key: &NodeKey) -> Option<NodeInstance> {
        self.key_to_index
            .get(key)
            .and_then(|index| self.graph.node_weight(*index))
            .cloned()
    }

    fn nodes_snapshot(&self) -> Vec<NodeInstance> {
        self.graph.node_weights().cloned().collect()
    }

    fn dependencies_of(&self, key: &NodeKey) -> Vec<NodeInstance> {
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

    fn dependents_of(&self, key: &NodeKey) -> Vec<NodeInstance> {
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
        let instances = nodes.into_iter().map(NodeInstance::new).collect();
        Self::from_instances(instances)
    }

    pub fn from_instances(nodes: Vec<NodeInstance>) -> Self {
        Self {
            shared: Arc::new(RwLock::new(NodeStackInner::from_instances(nodes))),
        }
    }

    pub fn replace(&self, nodes: Vec<NodeInstance>) {
        let mut guard = self.shared.write().expect("node stack poisoned");
        *guard = NodeStackInner::from_instances(nodes);
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

    pub fn with_nodes<R>(&self, f: impl FnOnce(&[NodeInstance]) -> R) -> R {
        let snapshot = {
            let guard = self.shared.read().expect("node stack poisoned");
            guard.nodes_snapshot()
        };
        f(&snapshot)
    }

    pub fn find(&self, name: &str, tag: &str) -> Option<NodeInstance> {
        let guard = self.shared.read().expect("node stack poisoned");
        guard.find(&NodeKey::new(name, tag))
    }

    pub fn push_config(&self, node: NodeConfig) {
        self.push_instance(NodeInstance::new(node));
    }

    pub fn push_instance(&self, node: NodeInstance) {
        let mut guard = self.shared.write().expect("node stack poisoned");
        guard.insert_node(node);
    }

    pub fn snapshot(&self) -> Vec<NodeInstance> {
        let guard = self.shared.read().expect("node stack poisoned");
        guard.nodes_snapshot()
    }

    pub fn dependencies_of(&self, name: &str, tag: &str) -> Vec<NodeInstance> {
        let guard = self.shared.read().expect("node stack poisoned");
        guard.dependencies_of(&NodeKey::new(name, tag))
    }

    pub fn dependents_of(&self, name: &str, tag: &str) -> Vec<NodeInstance> {
        let guard = self.shared.read().expect("node stack poisoned");
        guard.dependents_of(&NodeKey::new(name, tag))
    }
}
