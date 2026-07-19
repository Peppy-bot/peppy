//! `peppy platform logout`: kill the access token on the backend
//! (cross-replica) and delete the local session credentials. Does not revoke
//! the refresh token at Zitadel; a backend that's
//! unreachable or returns 401/503 still results in the local credentials being
//! cleared. An environment `PEPPY_API_KEY` cannot be cleared from here: the
//! command reports that authentication and federation remain active until the
//! variable is removed.

use std::sync::Arc;
use std::time::Duration;

use daemon_config::consts::PeppyDirs;

use crate::commands::Command;
use crate::context::AppContext;
use crate::error::{Error, Result};
use auth::{client, http::HttpClient, identity, profile, resolver, storage};
use daemon::control::{self as daemon_control, QueryStatusOutcome};
use daemon::state::DaemonState;

pub struct LogoutCommand {
    pub api_url: Option<String>,
    /// Skip the daemon-restart confirmation prompt.
    pub yes: bool,
    /// Test seam: override the peppy data dirs (the credentials file and
    /// `peppy_config.json5` both derive from it).
    pub peppy_dirs: Option<PeppyDirs>,
    /// The `PEPPY_API_KEY` PAT, injected by the dispatcher (never read from
    /// the environment here). `Some` means auth stays active after this
    /// command; the user is told so.
    pub pat: Option<String>,
}

impl Command for LogoutCommand {
    fn execute(self, ctx: &Arc<AppContext>) -> Result<()> {
        let dirs = self.peppy_dirs.unwrap_or_default();
        // A PAT is ambient authentication that this command cannot remove. Stop
        // before local or remote cleanup instead of claiming logout while the
        // daemon could immediately re-enroll the same core node.
        if self.pat.is_some() {
            return Err(Error::ExecutionFailed(
                "PEPPY_API_KEY is still set. Remove it from this shell and the running daemon's service environment, then run `peppy platform logout` again; no credentials or certificate material were changed."
                    .into(),
            ));
        }
        let _auth_operation =
            identity::acquire_platform_auth_operation(&dirs).map_err(|error| {
                Error::ExecutionFailed(format!(
                    "cannot begin logout: {error}; no remote or local credentials were changed"
                ))
            })?;

        // The invoking shell may not share the service manager's environment.
        // Every live daemon, including an external-router generation, must
        // authoritatively establish `pat_active=false` before cleanup: through
        // live control status for managed mode, or the startup-captured state bit
        // for external mode (which intentionally has no federation socket).
        // Router ownership says nothing about the daemon service environment; an
        // external daemon can still hold PEPPY_API_KEY and re-enrol immediately.
        // Malformed/unreadable state remains ambiguous and fails closed.
        let daemon_state_result = DaemonState::read_from(&DaemonState::state_file_in(dirs.root()));
        let daemon_state = daemon_state_result.as_ref().ok();
        let live_daemon_state = daemon_state.filter(|state| state.is_running());
        if live_daemon_state.is_some_and(|state| state.service_pat_active == Some(true)) {
            return Err(Error::ExecutionFailed(
                "PEPPY_API_KEY is still present in the running daemon's service environment. Remove it there and restart the service before logout; no credentials or certificate material were changed."
                    .into(),
            ));
        }
        // Managed generations expose the live status socket. A current external
        // generation has no federation control task, so its startup-captured
        // state bit is the authoritative service-environment answer. Legacy
        // external state (`None`) remains unknown and fails closed unless a
        // status-capable daemon happens to answer.
        let status_required = !matches!(
            live_daemon_state,
            Some(state)
                if state.federation_connect_timeout_secs.is_none()
                    && state.service_pat_active == Some(false)
        );
        if status_required {
            let socket = daemon_control::federation_control_socket_path(&dirs);
            match daemon_control::query_status(&socket, Duration::from_secs(2)) {
                QueryStatusOutcome::Status(status) if status.pat_active => {
                    return Err(Error::ExecutionFailed(
                        "PEPPY_API_KEY is still present in the running daemon's service environment. Remove it there and restart the service before logout; no credentials or certificate material were changed."
                            .into(),
                    ));
                }
                QueryStatusOutcome::Status(_) => {}
                QueryStatusOutcome::DaemonNotRunning
                    if daemon_state_result
                        .as_ref()
                        .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound)
                        || daemon_state.is_some_and(|state| !state.is_running()) =>
                {
                    // No state file and no control socket, or a parsed state
                    // whose process is definitively stopped, establish that no
                    // live daemon can immediately re-enrol. A malformed state
                    // and a live legacy external state remain ambiguous.
                }
                QueryStatusOutcome::DaemonNotRunning
                | QueryStatusOutcome::Restarting { .. }
                | QueryStatusOutcome::TimedOut
                | QueryStatusOutcome::DaemonError(_) => {
                    return Err(Error::ExecutionFailed(
                        "cannot verify whether the running daemon still has PEPPY_API_KEY because its federation control status is unavailable. Stop/restart the daemon without the PAT (and upgrade an older daemon if needed), then retry logout; no credentials or certificate material were changed."
                            .into(),
                    ));
                }
            }
        }

        // Configuration is needed only to decide whether a non-running future
        // daemon uses a managed router. A malformed config must never block
        // local credential/key cleanup, so retain it as a warning and continue.
        let config = daemon_config::peppy_config::load_or_create(&dirs);
        let federation = match daemon_state.as_ref() {
            Some(state) if state.is_running() => state.federation_connect_timeout_secs,
            _ => config
                .as_ref()
                .ok()
                .and_then(|config| config.zenoh.federation().map(|f| f.connect_timeout_secs)),
        };
        if let Err(error) = &config {
            println!(
                "Warning: could not read daemon configuration ({error}); continuing with local logout cleanup."
            );
        }
        let _requested_api_url = self.api_url;
        let creds_path = storage::credentials_path(&dirs);
        let http = HttpClient::new();

        // Load-resilient: a malformed credentials file fails to parse with
        // `Error::Auth`; treat it as "already effectively logged out" rather
        // than wedging logout. A default has no session, so the early return below
        // would otherwise leave the bad file on disk; overwrite it with a clean
        // default here so logout actually heals it.
        let (mut creds, credentials_invalid) = match storage::load(&creds_path) {
            Ok(creds) => (creds, false),
            Err(auth::AuthError::Auth(_)) => (storage::Credentials::default(), true),
            Err(e) => return Err(e.into()),
        };
        let pc = creds.session.clone();
        let identity_metadata = identity::load_identity_metadata(&dirs)
            .unwrap_or_else(|_| creds.core_node_identity.clone());
        if !credentials_invalid
            && pc.is_none()
            && identity_metadata.is_none()
            && !identity::identity_root(&dirs).exists()
        {
            // A crashed/failed login may have left only the durable binding
            // transition marker. Logged-out state is already fail closed, so
            // remove that marker before reporting successful cleanup.
            let marker_cleanup = identity::clear_binding_incomplete(&dirs);
            // Heal a crash after durable local deletion but before the control
            // poke. Even an already-locally-logged-out managed daemon may still
            // have the old upstream applied in memory.
            if federation.is_some() {
                let _ = super::finish_federation(
                    &dirs,
                    federation,
                    super::FederationPokeAction::Logout,
                );
            }
            marker_cleanup?;
            println!("Not logged in ({}).", profile::build_env_name());
            return Ok(());
        }

        // With a managed router, warn (before revoking) that logging out clears
        // the namespace, which restarts the daemon and wipes the running node
        // stack. Bypassed by `--yes`, skipped when no daemon is running or its
        // node stack holds no user nodes (so the restart wipes nothing). External
        // mode never pokes or restarts the daemon.
        if federation.is_some()
            && !super::confirm_restart(ctx, self.yes, &super::FederationPokeAction::Logout)?
        {
            println!("Logout aborted.");
            return Ok(());
        }

        // Own identity maintenance before either remote revocation call. If a
        // daemon rotation is currently applying/probing, fail now with every
        // bearer and local generation untouched; never revoke first and then
        // discover that local cleanup cannot acquire the rotation lease.
        let maintenance = identity::acquire_identity_maintenance(&dirs).map_err(|error| {
            Error::ExecutionFailed(format!(
                "cannot begin logout while core-node certificate maintenance is active: {error}. Wait for the daemon operation to settle, then retry; no remote or local credentials were changed."
            ))
        })?;

        // Re-read after acquiring the guard so the remote deletion and local
        // cleanup use the identity/session that won before this logout.
        creds = match storage::load(&creds_path) {
            Ok(creds) => creds,
            Err(auth::AuthError::Auth(_)) => storage::Credentials::default(),
            Err(e) => return Err(e.into()),
        };
        let pc = creds.session.clone();
        let identity_metadata = identity::load_identity_metadata(&dirs)
            .unwrap_or_else(|_| creds.core_node_identity.clone());

        if let Some(pc) = pc.as_ref() {
            // Keep one origin-bound credential for both revocation calls.
            // Reactive refresh during certificate deletion updates it in place;
            // never reload and adopt an unrelated concurrently-logged-in
            // session before sending the `/logout` bearer.
            let mut credential = resolver::session_credential(&creds_path, pc);
            // Revoke the core-node enrollment first while OAuth is still usable.
            if let Some(identity) = identity_metadata.as_ref() {
                match identity::normalize_api_origin(&pc.api_url) {
                    Ok(session_origin) if session_origin == identity.api_origin => {
                        match client::delete_core_node_certificate(
                            &http,
                            &pc.api_url,
                            &mut credential,
                            &identity.core_node_name,
                        ) {
                            Ok(204) => {}
                            Ok(status) => println!(
                                "Warning: core-node certificate revocation returned {status}; clearing local certificate material anyway."
                            ),
                            Err(error) => println!(
                                "Warning: could not revoke the core-node certificate ({error}); clearing local certificate material anyway. The issued leaf remains usable only until its short expiry."
                            ),
                        }
                    }
                    Ok(session_origin) => println!(
                        "Warning: the OAuth session is bound to {session_origin}, but the core-node certificate belongs to {}; refusing to forward that bearer cross-origin. Clearing local material; server-side revocation remains bounded by certificate expiry.",
                        identity.api_origin
                    ),
                    Err(error) => println!(
                        "Warning: the stored OAuth API origin is invalid ({error}); skipping remote certificate revocation and clearing local material."
                    ),
                }
            }

            // Certificate deletion may reactively refresh; the same credential
            // now carries that rotated access token and remains bound to this
            // session's API origin.
            match client::logout(&http, &pc.api_url, &credential) {
                Ok(202) | Ok(401) => {}
                Ok(503) => println!(
                    "Warning: backend revocation store unavailable; clearing local credentials anyway."
                ),
                Ok(status) => println!(
                    "Warning: logout returned {status}; clearing local credentials anyway."
                ),
                Err(e) => println!(
                    "Warning: could not reach the backend ({e}); clearing local credentials anyway."
                ),
            }
        } else if identity_metadata.is_some() {
            println!(
                "Warning: no OAuth bearer is available to revoke the orphaned core-node certificate; it remains bounded by its expiry."
            );
        }

        // Local cleanup is unconditional even when either backend call failed.
        // Once revocation/cleanup has begun, always release the maintenance
        // lease and ask a managed daemon to render standalone—even if durable
        // cleanup reports an error after partially clearing state. Otherwise a
        // revoked session or half-removed identity could leave the old link
        // applied until the next maintenance wake.
        let cleanup = maintenance.clear_local_logout();
        drop(maintenance);
        let action = if cleanup.is_ok() {
            super::FederationPokeAction::Logout
        } else {
            // Credentials may still be visible after a failed transaction;
            // never let a cleanup-error poke re-resolve them into federation.
            super::FederationPokeAction::FailClosed
        };
        let _ = super::finish_federation(&dirs, federation, action);
        cleanup?;
        println!(
            "Logged out and removed the local core-node identity ({}).",
            profile::build_env_name()
        );
        Ok(())
    }
}
