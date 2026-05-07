//! Serializable graph types shared across every stack-returning service.
//!
//! These are the JSON payload that every `*_response.graph_json` field
//! contains. Producer: `node_stack::NodeStack::to_serialized_graph`.
//! Consumer: any caller that parses `graph_json` (peppylib wrappers, the
//! peppy CLI, tests).

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// Per-instance lifecycle state. Wire representation is the lowercase variant
/// name (`"starting"`, `"running"`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum InstanceState {
    Starting,
    Running,
}

impl InstanceState {
    pub fn as_str(self) -> &'static str {
        match self {
            InstanceState::Starting => "starting",
            InstanceState::Running => "running",
        }
    }
}

impl fmt::Display for InstanceState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for InstanceState {
    type Err = UnknownInstanceState;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "starting" => Ok(InstanceState::Starting),
            "running" => Ok(InstanceState::Running),
            other => Err(UnknownInstanceState(other.to_owned())),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnknownInstanceState(pub String);

impl fmt::Display for UnknownInstanceState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "unknown instance state `{}`", self.0)
    }
}

impl std::error::Error for UnknownInstanceState {}

/// Label-only view of a `NodeEntity`'s lifecycle stage. The rich internal
/// variant lives in `node_stack::NodeStage`; this one is the shape written
/// to the wire (JSON / capnp text field) and read by external consumers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum NodeStage {
    Added,
    Building,
    Ready,
    Root,
}

impl NodeStage {
    pub fn as_str(self) -> &'static str {
        match self {
            NodeStage::Added => "Added",
            NodeStage::Building => "Building",
            NodeStage::Ready => "Ready",
            NodeStage::Root => "Root",
        }
    }
}

impl fmt::Display for NodeStage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for NodeStage {
    type Err = UnknownNodeStage;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "Added" => Ok(NodeStage::Added),
            "Building" => Ok(NodeStage::Building),
            "Ready" => Ok(NodeStage::Ready),
            "Root" => Ok(NodeStage::Root),
            other => Err(UnknownNodeStage(other.to_owned())),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnknownNodeStage(pub String);

impl fmt::Display for UnknownNodeStage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "unknown node stage `{}`", self.0)
    }
}

impl std::error::Error for UnknownNodeStage {}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SerializedInstance {
    pub instance_id: String,
    pub state: InstanceState,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SerializedNode {
    pub name: String,
    pub tag: String,
    pub config_path: String,
    pub artifact_path: Option<String>,
    /// Lifecycle stage name. `None` only for payloads produced by versions
    /// that predate the stage field; current producers always populate it.
    #[serde(default)]
    pub stage: Option<NodeStage>,
    /// All tracked instances with their per-instance state, including
    /// in-flight `Starting` instances.
    #[serde(default)]
    pub instances: Vec<SerializedInstance>,
    /// Variant label captured at `node add` time. Always populated — the
    /// synthetic root node and non-variant add paths use the literal
    /// `"default"` string.
    pub variant: String,
}

impl SerializedNode {
    pub fn label(&self) -> String {
        if self.variant == "default" {
            format!("{}:{}", self.name, self.tag)
        } else {
            format!("{}:{}@{}", self.name, self.tag, self.variant)
        }
    }

    /// Externally visible instance ids — the subset of `instances` that have
    /// reached `Running`. In-flight `Starting` instances are intentionally
    /// hidden: the externally-visible meaning is "currently running and
    /// reachable via messenger services".
    pub fn running_instance_ids(&self) -> Vec<&str> {
        self.running_instances()
            .map(|i| i.instance_id.as_str())
            .collect()
    }

    /// Count of `Running` instances. Matches `running_instance_ids().len()`.
    pub fn instance_count(&self) -> usize {
        self.running_instances().count()
    }

    fn running_instances(&self) -> impl Iterator<Item = &SerializedInstance> {
        self.instances
            .iter()
            .filter(|i| i.state == InstanceState::Running)
    }

    /// Returns the lifecycle stage label, or "Unknown" for legacy payloads
    /// that did not carry the stage field.
    pub fn stage_label(&self) -> &'static str {
        self.stage.map_or("Unknown", NodeStage::as_str)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SerializedEdge {
    pub from: SerializedNode,
    pub to: SerializedNode,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SerializedNodeGraph {
    pub nodes: Vec<SerializedNode>,
    pub edges: Vec<SerializedEdge>,
}
