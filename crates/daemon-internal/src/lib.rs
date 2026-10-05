//! The peppy daemon process body.
//!
//! Boundary contract: this crate owns the supervised generation loop
//! ([`serve`]: build a generation, run it, restart in-process on a namespace
//! change), the zenoh router host and its watchdog, the core-node runner, the
//! router-federation lifecycle and its UDS control socket, the renewal of the
//! peer certificate of an enrolled machine, and the two
//! CLI<->daemon shared surfaces: the on-disk [`state::DaemonState`] handoff
//! file (the daemon writes it once per generation; client commands read it)
//! and the [`control`] socket protocol plus its blocking client
//! ([`control::poke_refederate`], used by `peppy platform enroll`/`unenroll`).
//!
//! Consumers (the `peppy` CLI) own everything user-facing and process-level:
//! clap dispatch, service install/uninstall, the device-flow UX, logging
//! initialization, and CLI exit codes. The binary embeds its git hash and
//! passes it in as data ([`ServeOptions::git_hash`]); this crate reads no
//! build-time env of its own.

#![forbid(unsafe_code)]

pub mod control;
pub mod state;

mod builder;
mod certificate_renewal;
mod core_node;
mod daemon_lock;
mod error;
mod federation_control;
mod log_export;
mod messaging_router;
mod router_federation;
mod serve;
mod shutdown_signal;

pub use error::{Error as DaemonError, Result};
pub use serve::{ServeOptions, serve};
