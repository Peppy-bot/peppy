#![allow(clippy::result_large_err)]

pub mod archive;
pub mod build_io;
mod error;
mod node_stack;
mod virtual_deptree;

pub use error::Error as NodeStackError;

pub use archive::extract_tar_zst;
pub use build_io::{FeedbackLine, FeedbackStream, OutputReaderHooks};
pub use node_stack::add_steps;
pub use node_stack::{
    BuildContext, DependencySpec, EntityHandle, EntitySnapshot, InstanceState, NodeEntity,
    NodeStack, NodeStage, OutputSinks, RestoreTarget, SerializedInstance, SerializedNodeGraph,
    StartContext, StartedInstanceCtx, TrackedNodeInstance, WorkingDirGuard,
    collect_dependency_specs, validate_dependency_specs,
};
pub use virtual_deptree::{NodeKey, VirtualDeptree, VirtualNodeInfo};
