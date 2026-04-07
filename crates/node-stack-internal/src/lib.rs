#![allow(clippy::result_large_err)]

pub mod build_io;
mod error;
mod node_stack;

pub use error::Error as NodeStackError;

pub use node_stack::{
    BuildContext, DependencySpec, EntityHandle, NodeEntity, NodeStack, NodeStage,
    SerializedNodeGraph, TrackedNodeInstance, collect_dependency_specs, validate_dependency_specs,
};
