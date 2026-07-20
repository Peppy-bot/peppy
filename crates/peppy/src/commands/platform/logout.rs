//! `peppy platform logout`: delegate normal logout, revocation, identity cleanup
//! and router application to the running daemon. `--offline` is an explicit
//! recovery path that is allowed to mutate local state only after proving the
//! daemon is stopped and taking its lifetime ownership lock.

use std::sync::Arc;
use std::time::Duration;

use daemon::control::{self as daemon_control, ControlClientError};
use daemon::state::DaemonState;
use daemon_config::consts::PeppyDirs;

use crate::commands::Command;
use crate::context::AppContext;
use crate::error::{Error, Result};
use auth::{http::HttpClient, identity, logout::CleanupAttempt, profile, storage};

const OFFLINE_PROBE_TIMEOUT: Duration = Duration::from_millis(500);

pub struct LogoutCommand {
    pub api_url: Option<String>,
    pub yes: bool,
    pub offline: bool,
    pub peppy_dirs: Option<PeppyDirs>,
    pub pat: Option<String>,
}

impl Command for LogoutCommand {
    fn execute(self, ctx: &Arc<AppContext>) -> Result<()> {
        let dirs = self.peppy_dirs.clone().unwrap_or_default();
        if self.offline {
            return self.execute_offline(&dirs);
        }

        // Normal logout performs the version handshake before reading
        // credentials, checking PAT mode, completing config, or mutating state.
        super::require_daemon_hello(&dirs, true)?;

        if self.pat.is_some() {
            return Err(Error::ExecutionFailed(
                "PEPPY_API_KEY is still set in this process. Remove it from the shell and the \
                 daemon service environment, restart the daemon, then retry logout; no state \
                 was changed."
                    .into(),
            ));
        }

        let config =
            daemon_config::peppy_config::load_or_create(&dirs).map_err(Error::DaemonConfig)?;
        let socket = daemon_control::federation_control_socket_path(&dirs);
        let daemon_status =
            daemon_control::status(&socket, super::CONTROL_HELLO_TIMEOUT).map_err(|error| {
                super::control_error("inspect the daemon authentication mode", error, true)
            })?;
        if daemon_status.pat_active {
            return Err(Error::ExecutionFailed(
                "PEPPY_API_KEY is active in the daemon service. Remove it, restart the daemon, then retry logout; no state was changed."
                    .into(),
            ));
        }
        if !super::confirm_restart(ctx, self.yes, &super::FederationPokeAction::Logout)? {
            println!("Logout aborted.");
            return Ok(());
        }

        // The confirmation prompt can remain open indefinitely. Refresh the
        // generation and its authoritative budgets immediately before reading
        // the revision and dispatching the mutation.
        super::require_daemon_hello(&dirs, true)?;
        let mutation_status = daemon_control::status(&socket, super::CONTROL_HELLO_TIMEOUT)
            .map_err(|error| {
                super::control_error("refresh the daemon authentication mode", error, true)
            })?;
        if mutation_status.pat_active {
            return Err(Error::ExecutionFailed(
                "PEPPY_API_KEY became active in the daemon service before logout. Remove it, restart the daemon, and retry; no state was changed."
                    .into(),
            ));
        }
        let control_settings = super::identity_control_settings(&dirs, &config);
        let external = mutation_status.operator_managed && !mutation_status.pinned;

        let credentials = storage::load(&storage::credentials_path(&dirs))?;
        let expected_revision = credentials
            .session
            .as_ref()
            .map(|session| session.session_revision);
        let result = daemon_control::logout(
            &socket,
            super::identity_logout_timeout(control_settings.timeout_secs),
            expected_revision,
        )
        .map_err(|error| super::control_error("log out", error, true))?;

        super::report_logout(result, external)?;
        println!(
            "Local platform logout completed ({}).",
            profile::build_env_name()
        );
        Ok(())
    }
}

impl LogoutCommand {
    /// Explicit local recovery. This code is intentionally separate from
    /// normal logout so direct identity/store APIs cannot accidentally creep
    /// back into the live-daemon path.
    fn execute_offline(&self, dirs: &PeppyDirs) -> Result<()> {
        if self.pat.is_some() {
            return Err(Error::ExecutionFailed(
                "PEPPY_API_KEY is still set. Remove it before offline logout; local cleanup cannot \
                 disable ambient authentication."
                    .into(),
            ));
        }

        prove_daemon_stopped(dirs)?;
        let _owner = identity::acquire_identity_owner(dirs).map_err(|error| {
            tracing::warn!(
                event = "identity_offline_owner_lock_conflict",
                "offline identity recovery could not acquire process ownership"
            );
            Error::ExecutionFailed(format!(
                "cannot acquire offline identity ownership: {error}. Ensure the daemon is fully \
                 stopped, then retry `peppy platform logout --offline`."
            ))
        })?;
        // The daemon startup path holds the same lock for its lifetime. Recheck
        // under ownership to close the stop/start race before any auth API is
        // allowed to mutate credentials or key material.
        prove_daemon_stopped(dirs)?;

        // A killed daemon may leave its Peppy-spawned zenohd alive even though
        // the singleton/owner locks were released. Stop only the exact durable
        // child fence before deleting keys; ambiguous process state refuses the
        // offline cleanup. External routers remain operator-managed.
        daemon::fence_managed_router_for_offline_logout(dirs).map_err(|error| {
            Error::ExecutionFailed(format!(
                "cannot prove the last managed router is stopped: {error}. No local identity was deleted."
            ))
        })?;

        let router_ownership = offline_router_ownership(dirs);
        let outcome = auth::logout::logout_offline_recovery(dirs, &HttpClient::new())?;
        report_offline_cleanup(outcome)?;
        println!(
            "Offline logout removed local credentials and certificate material ({}).",
            profile::build_env_name()
        );
        match router_ownership {
            OfflineRouterOwnership::Managed => {}
            OfflineRouterOwnership::External => {
                println!("{}", super::EXTERNAL_ROUTER_OFFLINE_LOGOUT_NOTE);
            }
            OfflineRouterOwnership::Pinned => {
                println!("{}", super::PINNED_ROUTER_OFFLINE_LOGOUT_NOTE);
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OfflineRouterOwnership {
    Managed,
    External,
    Pinned,
}

fn offline_router_ownership(dirs: &PeppyDirs) -> OfflineRouterOwnership {
    let last_generation = DaemonState::read_from(&DaemonState::state_file_in(dirs.root()))
        .ok()
        .and_then(|state| {
            state
                .router_external
                .then_some(OfflineRouterOwnership::External)
                .or_else(|| {
                    (state.router_ownership == daemon::state::RouterOwnership::OperatorManaged)
                        .then_some(OfflineRouterOwnership::Pinned)
                })
        });
    if let Some(ownership) = last_generation {
        return ownership;
    }
    match daemon_config::peppy_config::load_or_create(dirs) {
        Ok(config) if config.zenoh.external_endpoint().is_some() => {
            OfflineRouterOwnership::External
        }
        Ok(_) if std::env::var_os("ZENOH_CONFIG").is_some_and(|value| !value.is_empty()) => {
            OfflineRouterOwnership::Pinned
        }
        Ok(_) => OfflineRouterOwnership::Managed,
        Err(error) => {
            println!(
                "Warning: could not determine current router ownership ({error}); local offline \
                 cleanup will continue."
            );
            OfflineRouterOwnership::Managed
        }
    }
}

fn report_offline_cleanup(outcome: auth::logout::LogoutOutcome) -> Result<()> {
    if let CleanupAttempt::Failed(error) = outcome.certificate_revocation {
        println!(
            "Warning: certificate revocation failed ({error}); offline local cleanup continued."
        );
    }
    if let CleanupAttempt::Failed(error) = outcome.oauth_revocation {
        println!("Warning: OAuth revocation failed ({error}); offline local cleanup continued.");
    }
    if let CleanupAttempt::Failed(error) = outcome.local_cleanup {
        return Err(Error::ExecutionFailed(format!(
            "offline logout could not finish local credential/certificate cleanup: {error}"
        )));
    }
    Ok(())
}

fn prove_daemon_stopped(dirs: &PeppyDirs) -> Result<()> {
    // Do not treat the PID in daemon.json as process identity: after a crash it
    // may be reused by an unrelated process. The authoritative proof is an
    // absent control peer plus the caller holding the daemon's lifetime identity
    // owner lock while performing the second probe.
    let socket = daemon_control::federation_control_socket_path(dirs);
    match daemon_control::hello(&socket, OFFLINE_PROBE_TIMEOUT) {
        Err(ControlClientError::DaemonNotRunning) => Ok(()),
        Ok(()) => Err(Error::ExecutionFailed(
            "offline logout refused because a compatible daemon is still answering control \
             requests; stop it and retry"
                .into(),
        )),
        Err(error) => Err(Error::ExecutionFailed(format!(
            "cannot prove the daemon is stopped: control probe was ambiguous ({error})"
        ))),
    }
}
