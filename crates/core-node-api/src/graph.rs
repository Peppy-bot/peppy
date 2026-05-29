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
}

impl SerializedNode {
    pub fn label(&self) -> String {
        format!("{}:{}", self.name, self.tag)
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeNotFound {
    label: String,
}

impl NodeNotFound {
    pub fn new(name: &str, tag: &str) -> Self {
        Self {
            label: format!("{name}:{tag}"),
        }
    }

    /// `name:tag` of the missing node.
    pub fn label(&self) -> &str {
        &self.label
    }
}

impl fmt::Display for NodeNotFound {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "no node matches `{}`", self.label)
    }
}

impl std::error::Error for NodeNotFound {}

impl SerializedNodeGraph {
    /// Look up a node by its `(name, tag)` identity. The pair is unique
    /// across `nodes`, so the first match is the only match.
    pub fn find_node(&self, name: &str, tag: &str) -> Option<&SerializedNode> {
        self.nodes.iter().find(|n| n.name == name && n.tag == tag)
    }

    /// Externally visible instance ids for the node identified by
    /// `(node_name, node_tag)`. Returns `NodeNotFound` when no node
    /// matches; returns `Ok(vec![])` when the node exists but every
    /// instance is still `Starting`.
    pub fn running_instance_ids_by_node(
        &self,
        node_name: &str,
        node_tag: &str,
    ) -> Result<Vec<&str>, NodeNotFound> {
        self.find_node(node_name, node_tag)
            .map(SerializedNode::running_instance_ids)
            .ok_or_else(|| NodeNotFound::new(node_name, node_tag))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_node(name: &str, tag: &str, instances: &[(&str, InstanceState)]) -> SerializedNode {
        SerializedNode {
            name: name.into(),
            tag: tag.into(),
            config_path: String::new(),
            artifact_path: None,
            stage: Some(NodeStage::Ready),
            instances: instances
                .iter()
                .map(|(id, st)| SerializedInstance {
                    instance_id: (*id).into(),
                    state: *st,
                })
                .collect(),
        }
    }

    #[test]
    fn find_node_returns_matching_node() {
        let graph = SerializedNodeGraph {
            nodes: vec![
                make_node("foo", "v1", &[]),
                make_node("foo", "v2", &[("r1", InstanceState::Running)]),
                make_node("bar", "v1", &[]),
            ],
            edges: vec![],
        };
        let node = graph.find_node("foo", "v2").expect("node should be found");
        assert_eq!(node.name, "foo");
        assert_eq!(node.tag, "v2");
        assert_eq!(node.instances.len(), 1);
    }

    #[test]
    fn find_node_returns_none_when_missing() {
        let graph = SerializedNodeGraph {
            nodes: vec![make_node("foo", "v1", &[])],
            edges: vec![],
        };
        assert!(graph.find_node("foo", "v2").is_none());
        assert!(graph.find_node("bar", "v1").is_none());
    }

    #[test]
    fn by_node_returns_running_only() {
        let graph = SerializedNodeGraph {
            nodes: vec![make_node(
                "foo",
                "v1",
                &[
                    ("r1", InstanceState::Running),
                    ("s1", InstanceState::Starting),
                    ("r2", InstanceState::Running),
                ],
            )],
            edges: vec![],
        };
        assert_eq!(
            graph.running_instance_ids_by_node("foo", "v1"),
            Ok(vec!["r1", "r2"])
        );
    }

    #[test]
    fn by_node_ok_empty_when_all_starting() {
        let graph = SerializedNodeGraph {
            nodes: vec![make_node(
                "foo",
                "v1",
                &[
                    ("s1", InstanceState::Starting),
                    ("s2", InstanceState::Starting),
                ],
            )],
            edges: vec![],
        };
        assert_eq!(graph.running_instance_ids_by_node("foo", "v1"), Ok(vec![]));
    }

    #[test]
    fn by_node_err_when_name_mismatch() {
        let graph = SerializedNodeGraph {
            nodes: vec![make_node("foo", "v1", &[("r1", InstanceState::Running)])],
            edges: vec![],
        };
        assert_eq!(
            graph.running_instance_ids_by_node("bar", "v1"),
            Err(NodeNotFound::new("bar", "v1"))
        );
    }

    #[test]
    fn by_node_err_when_tag_mismatch() {
        let graph = SerializedNodeGraph {
            nodes: vec![make_node("foo", "v1", &[("r1", InstanceState::Running)])],
            edges: vec![],
        };
        assert_eq!(
            graph.running_instance_ids_by_node("foo", "v2"),
            Err(NodeNotFound::new("foo", "v2"))
        );
    }

    #[test]
    fn by_node_err_on_empty_graph() {
        let graph = SerializedNodeGraph {
            nodes: vec![],
            edges: vec![],
        };
        assert_eq!(
            graph.running_instance_ids_by_node("foo", "v1"),
            Err(NodeNotFound::new("foo", "v1"))
        );
    }

    #[test]
    fn by_node_picks_correct_among_many() {
        let graph = SerializedNodeGraph {
            nodes: vec![
                make_node("foo", "v1", &[("foo_v1_r1", InstanceState::Running)]),
                make_node(
                    "foo",
                    "v2",
                    &[
                        ("foo_v2_r1", InstanceState::Running),
                        ("foo_v2_r2", InstanceState::Running),
                    ],
                ),
                make_node("bar", "v1", &[("bar_v1_r1", InstanceState::Running)]),
            ],
            edges: vec![],
        };
        assert_eq!(
            graph.running_instance_ids_by_node("foo", "v2"),
            Ok(vec!["foo_v2_r1", "foo_v2_r2"])
        );
    }

    #[test]
    fn node_not_found_display() {
        let err = NodeNotFound::new("router", "v1");
        let msg = err.to_string();
        assert!(msg.contains("router"), "got: {msg}");
        assert!(msg.contains("v1"), "got: {msg}");
        assert_eq!(err.label(), "router:v1");
    }
}
