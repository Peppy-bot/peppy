//! `peppy login` — OAuth 2.0 device-authorization login (RFC 8628).
//!
//! Fetches the public `/cli-config`, runs OIDC discovery against the returned
//! issuer, performs the device flow (opening the browser on a TTY), caches the
//! tokens per profile, and prints the resolved identity.

use std::path::PathBuf;
use std::sync::Arc;

use super::Command;
use crate::auth::device::DeviceFlowOptions;
use crate::auth::{cli_config, client, device, discovery, http, profile, resolver, storage};
use crate::context::AppContext;
use crate::error::Result;

pub struct LoginCommand {
    /// Profile to log into (`dev`/`prod`/…); defaults per build.
    pub env: Option<String>,
    /// Override the backend base URL (else profile default / `PEPPY_API_URL`).
    pub api_url: Option<String>,
    /// Suppress the automatic browser launch.
    pub no_browser: bool,
    /// Test seam: override the credentials file (defaults to the global
    /// `~/.peppy/conf/credentials.json5`).
    pub credentials_file: Option<PathBuf>,
}

impl Command for LoginCommand {
    fn execute(self, _ctx: &Arc<AppContext>) -> Result<()> {
        let profile = profile::resolve(self.env.as_deref(), self.api_url.as_deref())?;
        let creds_path = self.credentials_file.unwrap_or_else(storage::default_path);
        let agent = http::agent();

        let cfg = cli_config::fetch(&agent, &profile.api_url)?;
        let endpoints = discovery::discover(&agent, &cfg.issuer)?;
        let tokens = device::run(
            &agent,
            &endpoints,
            &cfg.client_id,
            &cfg.scopes,
            &DeviceFlowOptions {
                no_browser: self.no_browser,
            },
        )?;

        // Persist immediately so a transient `/me` failure can't lose a good login.
        let mut creds = storage::load(&creds_path)?;
        creds.profiles.insert(
            profile.name.clone(),
            client::creds_from_login(&cfg, &profile.api_url, &tokens, None),
        );
        storage::save(&creds_path, &creds)?;

        // Resolve identity for the confirmation line (reuses the cached token).
        let mut cred = resolver::resolve(&profile, &creds_path, &agent, None)?;
        match client::get_me(&agent, &profile.api_url, &mut cred) {
            Ok(principal) => {
                // Cache display identity against the (possibly refreshed) stored creds.
                let mut creds = storage::load(&creds_path)?;
                if let Some(pc) = creds.profiles.get_mut(&profile.name) {
                    pc.subject = principal.sub.clone();
                    pc.username = principal.display_name().to_string();
                    storage::save(&creds_path, &creds)?;
                }
                println!(
                    "Logged in as {} ({})",
                    principal.display_name(),
                    profile.name
                );
            }
            Err(e) => {
                // The tokens are valid and stored; only the identity lookup failed.
                println!(
                    "Logged in ({}). Could not fetch identity: {e}",
                    profile.name
                );
            }
        }
        Ok(())
    }
}
