#![allow(clippy::result_large_err)]

pub mod archive;
pub mod build_io;
mod error;
mod node_stack;
mod virtual_deptree;

pub use error::Error as NodeStackError;

pub use archive::extract_tar_zst;
pub use build_io::{FeedbackLine, FeedbackStream, OutputReaderHooks};
pub use core_node_api::InstanceState;
pub use node_stack::add_steps;
pub use node_stack::{
    BuildContext, DEFAULT_VARIANT, DepRef, DependencySpec, EntityHandle, EntityKey, EntitySnapshot,
    NameTagKey, NodeEntity, NodeStack, NodeStage, OutputSinks, RestoreTarget, StartContext,
    StartedInstanceCtx, TrackedNodeInstance, WorkingDirGuard, collect_dependency_specs,
    validate_dependency_specs,
};
pub use virtual_deptree::{VirtualDeptree, VirtualNodeInfo};
