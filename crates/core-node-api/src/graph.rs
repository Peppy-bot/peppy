//! Serializable graph types shared across every stack-returning service.
//!
//! These are the JSON payload that every `*_response.graph_json` field
//! contains. Producer: `node_stack::NodeStack::to_serialized_graph`.
//! Consumer: any caller that parses `graph_json` (peppylib wrappers, the
//! peppy CLI, tests).

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SerializedInstance {
    pub instance_id: String,
    pub state: String,
}

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
    pub fn label(&self) -> String {
        format!("{}:{}", self.name, self.tag)
    }

    pub fn instance_count(&self) -> usize {
        self.instance_ids.len()
    }

    /// Returns the lifecycle stage label, or "Unknown" for legacy payloads.
    pub fn stage_label(&self) -> &str {
        self.stage.as_deref().unwrap_or("Unknown")
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
