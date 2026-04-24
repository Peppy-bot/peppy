//! Serializable graph types shared across every stack-returning service.
//!
//! These are the JSON payload that every `*_response.graph_json` field
//! contains. Producer: `node_stack::NodeStack::to_serialized_graph`.
//! Consumer: any caller that parses `graph_json` (peppylib wrappers, the
//! peppy CLI, tests).

use serde::{Deserialize, Serialize};

/// Serializable representation of a single tracked instance with its state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SerializedInstance {
    pub instance_id: String,
    pub state: String,
}

/// Serializable representation of a node in the graph.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SerializedNode {
    pub name: String,
    pub tag: String,
    pub config_path: String,
    pub artifact_path: Option<String>,
    pub instance_ids: Vec<String>,
    /// Lifecycle stage name (e.g. "Added", "Building", "Ready", "Root").
    #[serde(default)]
    pub stage: Option<String>,
    /// All tracked instances with their per-instance state, including
    /// in-flight `Starting` instances not yet in `instance_ids`.
    #[serde(default)]
    pub instances: Vec<SerializedInstance>,
    /// Variant label selected at `node add` time, if any. `None` for the
    /// synthetic root node and for non-variant add paths.
    #[serde(default)]
    pub variant_name: Option<String>,
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

    /// Returns the lifecycle stage label, or "Unknown" for legacy payloads.
    pub fn stage_label(&self) -> &str {
        self.stage.as_deref().unwrap_or("Unknown")
    }

    /// Returns instance info in the format "N instance(s): [id1[running], id2[starting]]".
    pub fn instance_info(&self) -> String {
        let suffix = if self.instances.len() == 1 {
            "instance"
        } else {
            "instances"
        };
        if self.instances.is_empty() {
            let count = self.instance_count();
            let legacy_suffix = if count == 1 { "instance" } else { "instances" };
            let ids: Vec<String> = self
                .instance_ids
                .iter()
                .map(|id| format!("\"{}\"", id))
                .collect();
            format!("{} {}: [{}]", count, legacy_suffix, ids.join(", "))
        } else {
            let details: Vec<String> = self
                .instances
                .iter()
                .map(|i| format!("{}[{}]", i.instance_id, i.state))
                .collect();
            format!(
                "{} {}: [{}]",
                self.instances.len(),
                suffix,
                details.join(", ")
            )
        }
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
