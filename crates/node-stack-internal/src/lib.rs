#![allow(clippy::result_large_err)]

mod deployment;
mod error;

pub use error::Error as NodeStackError;

pub use deployment::types::{
    NodeEntity, SerializedEdge, SerializedNode, SerializedNodeGraph, TrackedNodeInstance,
    collect_dependency_specs, collect_peer_specs, exposes_interface, validate_dependency_specs,
    validate_peer_specs,
};
pub use deployment::{LaunchPlan, NodeStack};
