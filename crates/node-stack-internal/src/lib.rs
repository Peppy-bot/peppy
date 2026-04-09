#![allow(clippy::result_large_err)]

pub mod build_io;
mod error;
mod node_stack;

pub use error::Error as NodeStackError;

pub use build_io::{FeedbackLine, FeedbackStream, OutputReaderHooks};
pub use node_stack::{
    BuildContext, DependencySpec, EntityHandle, EntitySnapshot, InstanceState, NodeEntity,
    NodeStack, NodeStage, OutputSinks, PendingBuildInput, RestoreTarget, SerializedNodeGraph,
    StartContext, StartedInstanceCtx, TrackedNodeInstance, WorkingDir, collect_dependency_specs,
    extract_tar_zst, validate_dependency_specs,
};
