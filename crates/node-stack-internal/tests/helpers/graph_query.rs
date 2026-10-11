//! Read-only graph-wiring queries for tests, built on the public
//! [`NodeStack::to_serialized_graph`] API.
//!
//! Tests assert dependency wiring by inspecting the serialized graph's direct
//! edges. Only direct `depends_on.nodes` edges are considered
//! (contract-implementation edges, tagged with `via_contract`, are excluded).

use node_stack::NodeStack;

/// Names of the nodes that `(name, tag)` directly depends on.
pub fn dependency_names(stack: &NodeStack, name: &str, tag: &str) -> Vec<String> {
    stack
        .to_serialized_graph()
        .edges
        .iter()
        .filter(|edge| edge.via_contract.is_none())
        .filter(|edge| edge.from.name == name && edge.from.tag == tag)
        .map(|edge| edge.to.name.clone())
        .collect()
}

/// `(name, tag)` pairs of the nodes that `(name, tag)` directly depends on.
pub fn dependency_name_tags(stack: &NodeStack, name: &str, tag: &str) -> Vec<(String, String)> {
    stack
        .to_serialized_graph()
        .edges
        .iter()
        .filter(|edge| edge.via_contract.is_none())
        .filter(|edge| edge.from.name == name && edge.from.tag == tag)
        .map(|edge| (edge.to.name.clone(), edge.to.tag.clone()))
        .collect()
}

/// Names of the nodes that directly depend on `(name, tag)`.
pub fn dependent_names(stack: &NodeStack, name: &str, tag: &str) -> Vec<String> {
    stack
        .to_serialized_graph()
        .edges
        .iter()
        .filter(|edge| edge.via_contract.is_none())
        .filter(|edge| edge.to.name == name && edge.to.tag == tag)
        .map(|edge| edge.from.name.clone())
        .collect()
}
