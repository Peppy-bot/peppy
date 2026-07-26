//! `peppy platform logout`: deregister this machine's core node, kill the access
//! token on the backend (cross-replica), and delete the local session
//! credentials. Does not revoke the refresh token at Zitadel (out of scope for
//! v1); a backend that's unreachable or returns 401/503 still results in the
//! local credentials being cleared.

use std::sync::Arc;

use secrecy::ExposeSecret;

use daemon::state::DaemonState;
use daemon_config::consts::PeppyDirs;
use daemon_config::peppy_config::PeppyConfig;

use crate::commands::Command;
use crate::context::AppContext;
use crate::error::Result;
use auth::{client, http::HttpClient, profile, storage};

pub struct LogoutCommand {
    pub api_url: Option<String>,
    /// Skip the daemon-restart confirmation prompt.
    pub yes: bool,
    /// Test seam: override the peppy data dirs (the credentials file and
    /// `peppy_config.json5` both derive from it).
    pub peppy_dirs: Option<PeppyDirs>,
}

impl Command for LogoutCommand {
    fn execute(self, ctx: &Arc<AppContext>) -> Result<()> {
        let super::PlatformSession {
            dirs,
            config,
            api_url,
            creds_path,
            http,
            daemon_state,
        } = super::PlatformSession::resolve(self.peppy_dirs, self.api_url.as_deref())?;
        let federation = super::federation_poke_timeout_secs(daemon_state.as_ref(), &config);

        // Load-resilient: a malformed / pre-`workspace_id` file fails to parse
        // with `Error::Auth`; treat it as "already effectively logged out" rather
        // than wedging logout. A default has no session, so the early return below
        // would otherwise leave the bad file on disk; overwrite it with a clean
        // default here so logout actually heals it.
        let mut creds = match storage::load(&creds_path) {
            Ok(creds) => creds,
            Err(auth::AuthError::Auth(_)) => {
                let cleaned = storage::Credentials::default();
                storage::save(&creds_path, &cleaned)?;
                cleaned
            }
            Err(e) => return Err(e.into()),
        };
        let Some(pc) = creds.session.as_ref() else {
            println!("Not logged in ({}).", profile::build_env_name());
            return Ok(());
        };

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

        let access_token = pc.access_token.expose_secret().to_string();

        // Leave the platform's core-node registry before the revocation below
        // kills the token this call needs, and after the `confirm_restart` prompt
        // above, so an aborted logout deregisters nothing. Logging out is what
        // removes a machine from the registry: it stops federating, drops to the
        // `local` namespace, and stops pulling config, so leaving its row behind
        // would be a lie.
        deregister_core_node(
            &http,
            &api_url,
            &access_token,
            registered_core_node_name(daemon_state.as_ref(), &config).as_deref(),
        );

        match client::logout(&http, &api_url, &access_token) {
            Ok(202) | Ok(401) => {}
            Ok(503) => println!(
                "Warning: backend revocation store unavailable; clearing local credentials anyway."
            ),
            Ok(status) => {
                println!("Warning: logout returned {status}; clearing local credentials anyway.")
            }
            Err(e) => println!(
                "Warning: could not reach the backend ({e}); clearing local credentials anyway."
            ),
        }

        creds.session = None;
        // The cached router config is identity-bound; clear it with the session.
        creds.router = None;
        storage::save(&creds_path, &creds)?;
        println!("Logged out ({}).", profile::build_env_name());

        // In managed mode, poke the running daemon so it re-resolves (now logged
        // out) and de-federates the local router immediately, not on its next
        // poll. Best effort: never fails logout (the result is intentionally
        // discarded). External mode leaves federation untouched and tells the
        // operator that sessions change on the next manual restart.
        match federation {
            Some(connect_timeout_secs) => {
                let _ = super::poke_federation_and_report(
                    &dirs,
                    connect_timeout_secs,
                    super::FederationPokeAction::Logout,
                );
            }
            None => println!("{}", super::EXTERNAL_ROUTER_NOTE),
        }
        Ok(())
    }
}

/// The core-node name this machine registered with the platform, for
/// deregistration.
///
/// The daemon state file wins, and is used REGARDLESS of whether the pid it
/// records is still alive: the file outlives the daemon process, so a machine
/// whose daemon is already stopped still knows the name it registered under,
/// which is the ordinary case for a logout rather than the exception. An
/// explicitly configured `peppy_config.core_node_name` is the fallback for a
/// wiped state file.
///
/// `None` when neither resolves, which leaves one row behind. The residual leak
/// is narrow: the state file would have to be gone while the credentials
/// survive, and a wiped `PEPPY_HOME` takes both, so logout exits early with "not
/// logged in". The daemon's private `derive_core_node_name` is deliberately not
/// exported to close that last sliver, because recomputing a machine-UID hash to
/// guess which row to delete is a worse failure mode than leaving one behind.
fn registered_core_node_name(
    daemon_state: Option<&DaemonState>,
    config: &PeppyConfig,
) -> Option<String> {
    [
        daemon_state.map(|state| state.core_node_name.as_str()),
        config.core_node_name.as_deref(),
    ]
    .into_iter()
    .flatten()
    .map(str::trim)
    .find(|name| !name.is_empty())
    .map(str::to_owned)
}

/// Remove this machine's row from the platform's core-node registry, best
/// effort. Every failure warns and lets logout complete, matching what logout
/// already does for the token revocation itself.
///
/// A `404` is silent success: an external-mode daemon that never registered, and
/// a repeated logout, both find nothing to remove.
fn deregister_core_node(
    http: &HttpClient,
    api_url: &str,
    access_token: &str,
    core_node_name: Option<&str>,
) {
    let Some(core_node_name) = core_node_name else {
        println!(
            "Warning: could not determine this machine's core-node name; it stays listed in \
             the platform's core-node registry."
        );
        return;
    };
    let leak =
        format!("core node {core_node_name} may stay listed in the platform's core-node registry");
    match client::deregister_core_node(http, api_url, access_token, core_node_name) {
        Ok(204) | Ok(404) => {}
        Ok(status) => println!("Warning: deregistering returned {status}; {leak}."),
        Err(e) => println!("Warning: could not reach the backend ({e}); {leak}."),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state_with(core_node_name: &str, pid: Option<u32>) -> DaemonState {
        let mut state = DaemonState::new(
            core_node_name,
            "127.0.0.1",
            7447,
            "test",
            5,
            config::namespace::Namespace::local(),
            None,
        );
        state.daemon_pid = pid;
        state
    }

    fn config_with(core_node_name: Option<&str>) -> PeppyConfig {
        PeppyConfig {
            core_node_name: core_node_name.map(str::to_owned),
            ..PeppyConfig::default()
        }
    }

    #[test]
    fn the_daemon_state_name_wins_over_the_configured_one() {
        let state = state_with("cn-from-state", Some(std::process::id()));
        assert_eq!(
            registered_core_node_name(Some(&state), &config_with(Some("cn-from-config"))),
            Some("cn-from-state".to_string()),
            "the running daemon registered under the name it recorded, not the disk config's"
        );
    }

    #[test]
    fn a_dead_daemons_state_file_still_resolves_the_name() {
        // The ordinary case: logging out on a machine whose daemon is already
        // stopped. The state file outlives the process, and it still names the
        // core node this machine registered under, so `is_running` must not gate
        // this the way it gates the federation poke.
        let state = state_with("cn-from-dead-daemon", Some(u32::MAX));
        assert!(!state.is_running(), "u32::MAX names no live process");
        assert_eq!(
            registered_core_node_name(Some(&state), &config_with(None)),
            Some("cn-from-dead-daemon".to_string())
        );
    }

    #[test]
    fn the_configured_name_is_the_fallback_for_a_wiped_state_file() {
        assert_eq!(
            registered_core_node_name(None, &config_with(Some("cn-from-config"))),
            Some("cn-from-config".to_string())
        );
        // A state file that somehow carries no name falls through to the config
        // rather than deregistering an empty name.
        let blank = state_with("   ", Some(std::process::id()));
        assert_eq!(
            registered_core_node_name(Some(&blank), &config_with(Some("cn-from-config"))),
            Some("cn-from-config".to_string())
        );
    }

    #[test]
    fn neither_source_yields_no_name() {
        // Nothing is guessed here: recomputing the machine-UID hash to pick a row
        // to delete would be a worse failure mode than leaving one behind.
        assert_eq!(registered_core_node_name(None, &config_with(None)), None);
    }
}
