//! `peppy login` — OAuth 2.0 device-authorization login (RFC 8628).
//!
//! Fetches the public `/cli-config`, runs OIDC discovery against the returned
//! issuer, performs the device flow (opening the browser on a TTY), caches the
//! tokens as the single session, and prints the resolved identity.

use std::sync::Arc;

use config::consts::PeppyDirs;

use super::Command;
use crate::auth::device::DeviceFlowOptions;
use crate::auth::{
    cli_config, client, device, discovery, http::HttpClient, profile::Profile, resolver, storage,
};
use crate::context::AppContext;
use crate::error::{Error, Result};

pub struct LoginCommand {
    /// Override the backend base URL (else the build's `resource_servers.api` /
    /// `PEPPY_API_URL`).
    pub api_url: Option<String>,
    /// Suppress the automatic browser launch.
    pub no_browser: bool,
    /// Test seam: override the peppy data dirs (defaults to the global root).
    /// Both the credentials file and `peppy_config.json5` derive from it, so a
    /// test isolates all auth state under one tempdir without touching
    /// `PEPPY_HOME`.
    pub peppy_dirs: Option<PeppyDirs>,
}

impl Command for LoginCommand {
    fn execute(self, _ctx: &Arc<AppContext>) -> Result<()> {
        let dirs = self.peppy_dirs.unwrap_or_default();
        // Loads (and seeds/completes) peppy_config.json5 with the same strict,
        // fail-loud semantics the daemon uses; resource_servers supplies the
        // per-profile URL fallback.
        let config = config::peppy_config::load_or_create(&dirs).map_err(Error::PeppyConfig)?;
        let profile = Profile::resolve(self.api_url.as_deref(), &config.resource_servers)?;
        let creds_path = storage::credentials_path(&dirs);
        let http = HttpClient::new();

        let cfg = cli_config::fetch(&http, &profile.api_url)?;
        let endpoints = discovery::discover(&http, &cfg.issuer)?;
        let tokens = device::run(
            &http,
            &endpoints,
            &cfg.client_id,
            &cfg.scopes,
            &DeviceFlowOptions {
                no_browser: self.no_browser,
            },
        )?;

        // Persist immediately so a transient `/me` failure can't lose a good login.
        let mut creds = storage::load(&creds_path)?;
        creds.session = Some(client::creds_from_login(
            &cfg,
            &profile.api_url,
            &tokens,
            None,
        ));
        storage::save(&creds_path, &creds)?;

        // Resolve identity for the confirmation line (reuses the cached token).
        let mut cred = resolver::resolve(&creds_path, &http, None)?;
        match client::get_me(&http, &profile.api_url, &mut cred) {
            Ok(principal) => {
                // Cache display identity against the (possibly refreshed) stored creds.
                let mut creds = storage::load(&creds_path)?;
                if let Some(pc) = creds.session.as_mut() {
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
