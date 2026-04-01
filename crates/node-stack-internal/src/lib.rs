#![allow(clippy::result_large_err)]

mod error;
mod node_stack;

pub use error::Error as NodeStackError;

pub use node_stack::{
    NodeEntity, NodeStack, SerializedEdge, SerializedNode, SerializedNodeGraph,
    TrackedNodeInstance, collect_dependency_specs, validate_dependency_specs,
};
