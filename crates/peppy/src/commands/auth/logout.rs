//! `peppy auth logout`: kill the access token on the backend (cross-replica) and
//! delete the local session credentials. Does not revoke the refresh token at
//! Zitadel (out of scope for v1); a backend that's unreachable or returns
//! 401/503 still results in the local credentials being cleared.

use std::sync::Arc;

use secrecy::ExposeSecret;

use config::consts::PeppyDirs;

use crate::auth::{client, http::HttpClient, profile, storage};
use crate::commands::Command;
use crate::context::AppContext;
use crate::error::{Error, Result};

pub struct LogoutCommand {
    pub api_url: Option<String>,
    /// Test seam: override the peppy data dirs (the credentials file and
    /// `peppy_config.json5` both derive from it).
    pub peppy_dirs: Option<PeppyDirs>,
}

impl Command for LogoutCommand {
    fn execute(self, _ctx: &Arc<AppContext>) -> Result<()> {
        let dirs = self.peppy_dirs.unwrap_or_default();
        let config = config::peppy_config::load_or_create(&dirs).map_err(Error::PeppyConfig)?;
        let api_url = profile::resolve_api_url(self.api_url.as_deref(), &config.resource_servers)?;
        let creds_path = storage::credentials_path(&dirs);
        let http = HttpClient::new();

        let mut creds = storage::load(&creds_path)?;
        let Some(pc) = creds.session.as_ref() else {
            println!("Not logged in ({}).", profile::build_env_name());
            return Ok(());
        };

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
        // effort: never fails logout.
        super::poke_federation_and_report(
            &dirs,
            config.federation.connect_timeout_secs,
            super::FederationPokeAction::Logout,
        );
        Ok(())
    }
}
