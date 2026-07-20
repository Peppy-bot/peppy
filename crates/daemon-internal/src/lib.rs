//! The peppy daemon process body.
//!
//! Boundary contract: this crate owns the supervised generation loop
//! ([`serve`]: build a generation, run it, restart in-process on a namespace
//! change), the zenoh router host and its watchdog, the core-node runner, the
//! router-federation lifecycle and its UDS control socket, and the two
//! CLI<->daemon shared surfaces: the on-disk [`state::DaemonState`] handoff
//! file (the daemon writes it once per generation; client commands read it)
//! and the [`control`] socket protocol plus its blocking client
//! (used by `peppy platform login`/`logout`/`federations`).
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
mod core_node;
mod daemon_lock;
mod error;
mod federation_control;
mod identity_applicator;
mod messaging_router;
mod router_federation;
mod router_process;
mod serve;
mod shutdown_signal;

pub use error::{Error as DaemonError, Result};
pub use serve::{ClockSource, ServeOptions, serve};

/// Maximum graceful teardown window a namespace-changing in-process restart
/// may consume before the replacement generation begins startup.
pub fn restart_teardown_budget(shutdown_grace_secs: u64) -> std::time::Duration {
    messaging_router::teardown_budget_for(shutdown_grace_secs)
}

/// Worst-case time between a namespace-changing acknowledgement and the
/// replacement managed router becoming ready to begin identity reconciliation.
pub fn restart_handoff_budget(shutdown_grace_secs: u64) -> std::time::Duration {
    restart_teardown_budget(shutdown_grace_secs)
        + serve::RESTART_REAP_BUDGET
        + serve::RESTART_PORT_RELEASE_BUDGET
        + serve::MANAGED_ROUTER_STARTUP_BUDGET
}

/// Proves that a Peppy-managed router from the last daemon generation is no
/// longer running before an explicit daemon-down identity cleanup. External
/// routers are never signaled. A missing state file means no Peppy generation
/// exists to reclaim; ambiguous/corrupt state fails closed.
pub fn fence_managed_router_for_offline_logout(
    dirs: &daemon_config::consts::PeppyDirs,
) -> Result<()> {
    let state_path = state::DaemonState::state_file_in(dirs.root());
    if !state_path.exists() {
        return Ok(());
    }
    router_process::stop_before_startup(&state_path, true)
}

/// Test-only helpers shared by this crate's unit-test modules.
#[cfg(test)]
pub(crate) mod test_util {
    use daemon_config::peppy_config::ParsedEndpointBuf;

    /// Parses a `tls` dial endpoint, panicking on invalid test input.
    pub(crate) fn dial(endpoint: &str) -> ParsedEndpointBuf {
        ParsedEndpointBuf::parse(endpoint, "tls").unwrap()
    }
}
