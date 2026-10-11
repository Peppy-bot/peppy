//! `node-stack`: in-memory, thread-safe model of the daemon's node DAG.
//!
//! # Boundary contract
//!
//! This crate owns: the node dependency graph ([`NodeStack`]), each node's
//! lifecycle state machine ([`NodeEntity`] / [`NodeStage`]), the tracked
//! instances of a node, the concrete add/build/run I/O steps that drive an
//! entity `Added` to `Building` to `Ready` and spawn/stop its OS child
//! process, `.tar.zst` archive extraction, child-process output streaming,
//! the log file of an action and the lines written to it ([`ActionLog`],
//! [`Announcer`]), and caller-driven service/action cycle detection. The stack is the only
//! record of the children the daemon spawned, so dropping its last handle
//! SIGKILLs every process group it still tracks (see `NodeStackInner`'s
//! `Drop`); the cooperative stop paths belong to the daemon.
//!
//! Consumers (the daemon in `core-node-internal`, the CLI in `peppy`) own:
//! the `peppy.json5` config objects, every filesystem path (working dirs,
//! log files, artifact storage, peppy directory layout), the environment
//! variables that builds and runs get (the variables the crate reads itself
//! are listed under Initialization), and the messenger/feedback plumbing.
//! They pass these across the boundary through the explicit context structs
//! ([`BuildContext`], [`StartContext`], [`OutputSinks`]) and receive
//! [`EntityHandle`]s to read or drive entities.
//!
//! # Initialization
//!
//! Construction is explicit: [`NodeStack::new`] takes the root config, an
//! optional root instance id, and the root path; [`NodeStack::with_shutdown_grace`]
//! is the builder knob for the cooperative-shutdown grace period,
//! [`NodeStack::with_pypi_mirror`] the one for the PyPI mirror of Python
//! builds, and [`NodeStack::with_log_exporter`] the one for the exporter of the
//! log files. The crate reads one environment variable of its own,
//! `PEPPY_NO_CONTAINER_BUILD_CACHE`, which turns the container build cache
//! off (`node_stack::container_build_cache`), and the HTTP client of the PyPI
//! mirror check follows the proxy variables of the daemon (`ALL_PROXY`,
//! `HTTPS_PROXY`, `HTTP_PROXY`, `NO_PROXY`). Its process-wide state is three
//! monotonic counters (the entity generation in `node_stack::entity`, the
//! runtime config file names in `node_stack::run_steps`, the sccache server
//! ports in `node_stack::container_build_cache`) and the map of uv cache gates
//! in `node_stack::container_build_cache`, created on first use. Each is
//! per-process by intent, not per-`NodeStack` state.
#![allow(clippy::result_large_err)]
#![forbid(unsafe_code)]

pub mod action_log;
mod archive;
pub mod build_io;
mod build_progress;
mod error;
mod node_stack;
mod process_group;
mod service_action_cycle;
mod virtual_deptree;

pub use error::Error as NodeStackError;

pub use action_log::{ActionLog, Announcer};
pub use archive::extract_tar_zst;
pub use build_io::{FeedbackLine, FeedbackOwner, FeedbackStream, OutputReaderHooks};
pub use core_node_api::InstanceState;
pub use node_stack::add_steps;
pub use node_stack::{
    Artifact, BuildContext, BuiltInLaunch, CACHED_BUILD_REUSE_PREFIX, EntityHandle, InstanceClock,
    NodeEntity, NodeStack, NodeStage, OUTPUT_READER_JOIN_BUDGET, OutputSinks, PairEndpoint,
    Pairing, PairingNodeSnapshot, RemoteSlotMeta, SlotAddr, StartContext, StartedInstanceCtx,
    TrackedNodeInstance, WorkingDirGuard, pairing_slot_view,
};
pub use process_group::{kill_process_group, terminate_process_group};
pub use virtual_deptree::{NodeKey, VirtualDeptree, VirtualNodeInfo};
