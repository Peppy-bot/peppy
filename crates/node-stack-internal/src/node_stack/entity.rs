use config::node::{Name, NodeConfig};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

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

/// A single entity with N instances inside the node stack.
#[derive(Clone, Debug)]
pub struct NodeEntity {
    pub(super) config: NodeConfig,
    pub(super) instances: Vec<TrackedNodeInstance>,
    pub(super) fs_root_path: PathBuf,
}

impl NodeEntity {
    /// Creates a new NodeEntity with a config only (no instances).
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

    /// Adds an instance to this entity.
    pub(super) fn add_instance(&mut self, instance: TrackedNodeInstance) {
        self.instances.push(instance);
    }

    /// Removes an instance by its ID. Returns true if the instance was found and removed.
    pub(super) fn remove_instance(&mut self, instance_id: &Name) -> bool {
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

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct DependencySpec {
    pub node_name: String,
    pub node_tag: String,
}
