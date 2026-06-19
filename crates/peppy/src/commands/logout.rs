//! `peppy logout` — kill the access token on the backend (cross-replica) and
//! delete the local credentials for the profile. Does not revoke the refresh
//! token at Zitadel (out of scope for v1); a backend that's unreachable or
//! returns 401/503 still results in the local credentials being cleared.

use std::path::PathBuf;
use std::sync::Arc;

use secrecy::ExposeSecret;

use super::Command;
use crate::auth::{client, http, profile, storage};
use crate::context::AppContext;
use crate::error::Result;

pub struct LogoutCommand {
    pub env: Option<String>,
    pub api_url: Option<String>,
    /// Test seam: override the credentials file.
    pub credentials_file: Option<PathBuf>,
}

impl Command for LogoutCommand {
    fn execute(self, _ctx: &Arc<AppContext>) -> Result<()> {
        let profile = profile::resolve(self.env.as_deref(), self.api_url.as_deref())?;
        let creds_path = self.credentials_file.unwrap_or_else(storage::default_path);
        let agent = http::agent();

        let mut creds = storage::load(&creds_path)?;
        let Some(pc) = creds.profiles.get(&profile.name) else {
            println!("Not logged in ({}).", profile.name);
            return Ok(());
        };

        let access_token = pc.access_token.expose_secret().to_string();
        match client::logout(&agent, &profile.api_url, &access_token) {
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

        creds.profiles.remove(&profile.name);
        storage::save(&creds_path, &creds)?;
        println!("Logged out ({}).", profile.name);
        Ok(())
    }
}
