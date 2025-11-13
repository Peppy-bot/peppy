use std::{
    path::PathBuf,
    sync::{Arc, OnceLock, RwLock},
};

use crate::error::{Error, Result};
use config::node::NodeConfig;
use config::peppy_config::{Deployment, DeploymentNodeSource};

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

    fn matches(&self, name: &str, tag: &str) -> bool {
        self.config.manifest.name.as_str() == name && self.config.manifest.tag == tag
    }
}

static GLOBAL_NODE_STACK: OnceLock<Arc<RwLock<Vec<NodeInstance>>>> = OnceLock::new();

fn shared_stack() -> &'static Arc<RwLock<Vec<NodeInstance>>> {
    GLOBAL_NODE_STACK.get_or_init(|| Arc::new(RwLock::new(Vec::new())))
}

#[derive(Clone)]
pub struct NodeStack {
    shared: Arc<RwLock<Vec<NodeInstance>>>,
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
        *guard = nodes;
    }

    pub fn len(&self) -> usize {
        self.shared.read().expect("node stack poisoned").len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn contains(&self, name: &str, tag: &str) -> bool {
        self.shared
            .read()
            .expect("node stack poisoned")
            .iter()
            .any(|node| node.matches(name, tag))
    }

    pub fn with_nodes<R>(&self, f: impl FnOnce(&[NodeInstance]) -> R) -> R {
        let guard = self.shared.read().expect("node stack poisoned");
        f(&guard)
    }

    pub fn find(&self, name: &str, tag: &str) -> Option<NodeInstance> {
        self.shared
            .read()
            .expect("node stack poisoned")
            .iter()
            .find(|node| node.matches(name, tag))
            .cloned()
    }

    pub fn push_config(&self, node: NodeConfig) {
        self.push_instance(NodeInstance::new(node));
    }

    pub fn push_instance(&self, node: NodeInstance) {
        self.shared.write().expect("node stack poisoned").push(node);
    }

    pub fn snapshot(&self) -> Vec<NodeInstance> {
        self.shared.read().expect("node stack poisoned").clone()
    }
}
