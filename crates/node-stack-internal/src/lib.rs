#![allow(clippy::result_large_err)]

pub mod archive;
pub mod build_io;
mod error;
mod node_stack;
mod service_action_cycle;
mod virtual_deptree;

pub use error::Error as NodeStackError;
pub use service_action_cycle::{CycleCheckNode, ServiceActionCycle, find_service_action_cycle};

pub use archive::extract_tar_zst;
pub use build_io::{FeedbackLine, FeedbackStream, OutputReaderHooks};
pub use core_node_api::InstanceState;
pub use node_stack::add_steps;
pub use node_stack::{
    BuildContext, EntityHandle, EntitySnapshot, NodeEntity, NodeStack, NodeStage, OutputSinks,
    RestoreTarget, StartContext, StartedInstanceCtx, TrackedNodeInstance, WorkingDirGuard,
};
pub use virtual_deptree::{NodeKey, VirtualDeptree, VirtualNodeInfo};
