use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    sync::{Arc, OnceLock, RwLock},
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

struct NodeStackInner {
    graph: StableDiGraph<NodeInstance, ()>,
    key_to_index: HashMap<NodeKey, NodeIndex>,
    pending_dependents: HashMap<NodeKey, Vec<NodeIndex>>,
}

impl NodeStackInner {
    fn new() -> Self {
        Self {
            graph: StableDiGraph::default(),
            key_to_index: HashMap::new(),
            pending_dependents: HashMap::new(),
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
        self.attach_pending_dependents(&key);
    }

    fn attach_pending_dependents(&mut self, key: &NodeKey) {
        let Some(index) = self.key_to_index.get(key).copied() else {
            return;
        };

        if let Some(mut dependants) = self.pending_dependents.remove(key) {
            dependants.retain(|dep_index| dep_index != &index);
            for dependant in dependants {
                if self.graph.find_edge(dependant, index).is_none() {
                    self.graph.add_edge(dependant, index, ());
                }
            }
        }
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
        let Some(node) = self.graph.node_weight(index) else {
            return;
        };
        for dep_key in dependency_keys(node.config()) {
            if let Some(&dependency_index) = self.key_to_index.get(&dep_key) {
                if self.graph.find_edge(index, dependency_index).is_none() {
                    self.graph.add_edge(index, dependency_index, ());
                }
            } else {
                let waiting = self.pending_dependents.entry(dep_key).or_default();
                if !waiting.iter().any(|existing| *existing == index) {
                    waiting.push(index);
                }
            }
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

fn dependency_keys(node: &NodeConfig) -> Vec<NodeKey> {
    let Some(subscriptions) = node.interfaces.subscribes_to.as_ref() else {
        return Vec::new();
    };

    let mut deps: HashSet<NodeKey> = HashSet::new();

    if let Some(topics) = subscriptions.topics.as_ref() {
        for topic in topics {
            if let Some(node_name) = topic.node.as_deref() {
                deps.insert(NodeKey::new(node_name, &topic.tag));
            }
        }
    }

    if let Some(services) = subscriptions.services.as_ref() {
        for service in services {
            deps.insert(NodeKey::new(&service.node, &service.tag));
        }
    }

    if let Some(actions) = subscriptions.actions.as_ref() {
        for action in actions {
            deps.insert(NodeKey::new(&action.node, &action.tag));
        }
    }

    deps.into_iter().collect()
}

static GLOBAL_NODE_STACK: OnceLock<Arc<RwLock<NodeStackInner>>> = OnceLock::new();

fn shared_stack() -> &'static Arc<RwLock<NodeStackInner>> {
    GLOBAL_NODE_STACK.get_or_init(|| Arc::new(RwLock::new(NodeStackInner::new())))
}

#[derive(Clone)]
pub struct NodeStack {
    shared: Arc<RwLock<NodeStackInner>>,
}

impl Default for NodeStack {
    fn default() -> Self {
        Self::global()
    }
}

impl NodeStack {
    pub fn global() -> Self {
        Self {
            shared: shared_stack().clone(),
        }
    }

    pub fn from_configs(nodes: Vec<NodeConfig>) -> Self {
        let instances = nodes.into_iter().map(NodeInstance::new).collect();
        Self::from_instances(instances)
    }

    pub fn from_instances(nodes: Vec<NodeInstance>) -> Self {
        let stack = Self::global();
        stack.replace(nodes);
        stack
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
