#![allow(clippy::result_large_err)]

mod deployment;
mod error;

pub use error::Error as NodeStackError;

pub use deployment::types::{
    collect_dependency_specs, exposes_interface, NodeEntity, NodeInstance, SerializedEdge,
    SerializedNode, SerializedNodeGraph,
};
pub use deployment::{LaunchPlan, NodeStack};
