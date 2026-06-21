//! The peppyOS command-line tool.
//!
//! `peppy` is a leaf binary: `main.rs` is the entrypoint and this library half
//! exists so the integration test crate can construct and execute command
//! structs in-process. No other crate depends on it.
//!
//! The supported surface is intentionally small:
//! - [`commands`]: the [`commands::Command`] trait plus the per-group command
//!   and subcommand types that `main.rs` dispatches to.
//! - [`auth`]: the `peppy auth login`/`logout`/`whoami` engine (OAuth device
//!   flow, token storage, credential resolution, authenticated backend client).
//!   It is the engine behind the [`commands::auth`] command group, kept separate
//!   from the CLI plumbing.
//! - [`context::AppContext`]: the explicit construction seam (`new`,
//!   `from_current_dir`, `with_daemon_state_file`, `with_messenger`).
//! - [`error`]: the crate error type, surfaced through `Command::execute`.
//! - [`colors_enabled`]: the single stdout color decision, shared with the
//!   binary's log formatter.
//!
//! `test_support` is test-only scaffolding behind the `test-support` feature.
//! `daemon_state` and `terminal` are internal and not part of the contract.

#![deny(unsafe_code)]

pub mod auth;
pub mod commands;
pub mod context;
mod daemon_state;
pub mod error;
pub(crate) mod terminal;

// `terminal` itself stays crate-private; only the color gate is re-exported so
// the binary crate's `logging` module shares the one stdout color decision.
pub use terminal::colors_enabled;

#[cfg(feature = "test-support")]
pub mod test_support;
