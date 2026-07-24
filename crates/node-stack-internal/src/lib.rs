//! `node-stack`: in-memory, thread-safe model of the daemon's node DAG.
//!
//! # Boundary contract
//!
//! This crate owns: the node dependency graph ([`NodeStack`]), each node's
//! lifecycle state machine ([`NodeEntity`] / [`NodeStage`]), the tracked
//! instances of a node, the concrete add/build/run I/O steps that drive an
//! entity `Added` to `Building` to `Ready` and spawn/stop its OS child
//! process, `.tar.zst` archive extraction, child-process output streaming,
//! and caller-driven service/action cycle detection.
//!
//! Consumers (the daemon in `core-node-internal`, the CLI in `peppy`) own:
//! the `peppy.json5` config objects, every filesystem path (working dirs,
//! log files, artifact storage, peppy directory layout), environment
//! variables, and the messenger/feedback plumbing. They pass these across
//! the boundary through the explicit context structs ([`BuildContext`],
//! [`StartContext`], [`OutputSinks`]) and receive [`EntityHandle`]s to read
//! or drive entities.
//!
//! # Initialization
//!
//! Construction is explicit: [`NodeStack::new`] takes the root config, an
//! optional root instance id, and the root path; [`NodeStack::with_shutdown_grace`]
//! is the builder knob for the cooperative-shutdown grace period. The crate
//! reads no environment variables and performs no lazy global init. It does
//! keep two process-wide monotonic counters (documented at their definitions
//! in `node_stack::entity` and `node_stack::run_steps`); both are intentional
//! per-process token sources, not per-`NodeStack` state.
#![allow(clippy::result_large_err)]
#![forbid(unsafe_code)]

pub mod archive;
pub mod build_io;
mod error;
mod node_stack;
mod service_action_cycle;
mod virtual_deptree;

pub use error::Error as NodeStackError;

pub use archive::extract_tar_zst;
pub use build_io::{FeedbackLine, FeedbackStream, OutputReaderHooks};
pub use core_node_api::InstanceState;
pub use node_stack::add_steps;
pub use node_stack::{
    BuildContext, EntityHandle, NodeEntity, NodeStack, NodeStage, OutputSinks, PairEndpoint,
    Pairing, PairingNodeSnapshot, SlotAddr, StartContext, StartedInstanceCtx, TrackedNodeInstance,
    WorkingDirGuard, is_host_provided_mount_source, pairing_slot_view,
};
pub use virtual_deptree::{NodeKey, VirtualDeptree, VirtualNodeInfo};
