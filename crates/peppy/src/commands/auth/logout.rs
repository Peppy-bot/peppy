//! `peppy auth logout`: kill the access token on the backend (cross-replica) and
//! delete the local session credentials. Does not revoke the refresh token at
//! Zitadel (out of scope for v1); a backend that's unreachable or returns
//! 401/503 still results in the local credentials being cleared.

use std::sync::Arc;

use secrecy::ExposeSecret;

use daemon_config::consts::PeppyDirs;

use crate::commands::Command;
use crate::context::AppContext;
use crate::error::{Error, Result};
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
        let dirs = self.peppy_dirs.unwrap_or_default();
        let config =
            daemon_config::peppy_config::load_or_create(&dirs).map_err(Error::DaemonConfig)?;
        let api_url = profile::resolve_api_url(self.api_url.as_deref(), &config.resource_servers)?;
        let creds_path = storage::credentials_path(&dirs);
        let http = HttpClient::new();

        // Load-resilient: a malformed / pre-`organization_id` file fails to parse
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

        // Warn (before revoking) that logging out clears the namespace, which
        // restarts the daemon and wipes the running node stack. Bypassed by
        // `--yes`, skipped when no daemon is running or its node stack holds no
        // user nodes (so the restart wipes nothing).
        if !super::confirm_restart(ctx, self.yes, &super::FederationPokeAction::Logout)? {
            println!("Logout aborted.");
            return Ok(());
        }

        let access_token = pc.access_token.expose_secret().to_string();
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

        // Poke the running daemon so it re-resolves (now logged out) and
        // de-federates the local router immediately, not on its next poll. Best
        // effort: never fails logout (the result is intentionally discarded).
        let _ = super::poke_federation_and_report(
            &dirs,
            config.federation.connect_timeout_secs,
            super::FederationPokeAction::Logout,
        );
        Ok(())
    }
}
