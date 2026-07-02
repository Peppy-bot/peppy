//! `peppy auth login`: OAuth 2.0 device-authorization login (RFC 8628).
//!
//! Fetches the public `/cli-config`, runs OIDC discovery against the returned
//! issuer, performs the device flow (opening the browser on a TTY), caches the
//! tokens as the single session, and prints the resolved identity.

use std::sync::Arc;

use daemon_config::consts::PeppyDirs;

use crate::auth::device::DeviceFlowOptions;
use crate::auth::{
    cli_config, client, device, discovery, http::HttpClient, profile, resolver, storage,
};
use crate::commands::Command;
use crate::context::AppContext;
use crate::error::{Error, Result};

pub struct LoginCommand {
    /// Override the backend base URL (else the build's `resource_servers.api` /
    /// `PEPPY_API_URL`).
    pub api_url: Option<String>,
    /// Suppress the automatic browser launch.
    pub no_browser: bool,
    /// Skip the daemon-restart confirmation prompt.
    pub yes: bool,
    /// Test seam: override the peppy data dirs (defaults to the global root).
    /// Both the credentials file and `peppy_config.json5` derive from it, so a
    /// test isolates all auth state under one tempdir without touching
    /// `PEPPY_HOME`.
    pub peppy_dirs: Option<PeppyDirs>,
}

impl Command for LoginCommand {
    fn execute(self, ctx: &Arc<AppContext>) -> Result<()> {
        let dirs = self.peppy_dirs.unwrap_or_default();
        // Loads (and seeds/completes) peppy_config.json5 with the same strict,
        // fail-loud semantics the daemon uses; resource_servers supplies the
        // per-profile URL fallback.
        let config =
            daemon_config::peppy_config::load_or_create(&dirs).map_err(Error::DaemonConfig)?;
        let api_url = profile::resolve_api_url(self.api_url.as_deref(), &config.resource_servers)?;
        let creds_path = storage::credentials_path(&dirs);
        let http = HttpClient::new();

        // Warn (before authentication begins) that a login changing the
        // organization namespace restarts the daemon and wipes the running node
        // stack. Bypassed by `--yes`, and skipped when no daemon is running or
        // its node stack holds no user nodes (so the restart wipes nothing).
        if !super::confirm_restart(ctx, self.yes, &super::FederationPokeAction::Login)? {
            println!("Login aborted.");
            return Ok(());
        }

        let cfg = cli_config::fetch(&http, &api_url)?;
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
        // Load-resilient: a malformed / pre-`organization_id` / version-mismatched
        // file fails to parse with `Error::Auth`; start fresh rather than wedge
        // login on it (the stale file self-heals on this save).
        let mut creds = match storage::load(&creds_path) {
            Ok(creds) => creds,
            Err(Error::Auth(_)) => storage::Credentials::default(),
            Err(e) => return Err(e),
        };
        let pc = client::creds_from_login(&cfg, &api_url, &tokens);
        creds.session = Some(pc.clone());
        // Drop any cached router config: it is identity-bound, and this login may
        // be a different user/backend. The next remote connect re-pulls.
        creds.router = None;
        storage::save(&creds_path, &creds)?;

        // Fetch identity using the in-memory credential (the token was minted
        // seconds ago, so there's no need to reload from disk or proactively
        // refresh via the resolver).
        let mut cred = resolver::session_credential(&creds_path, &pc);
        match client::get_me(&http, &api_url, &mut cred) {
            Ok(principal) => {
                // Cache display identity against the stored session.
                if let Some(session) = creds.session.as_mut() {
                    session.subject = principal.sub.clone();
                    session.username = principal.display_name().to_string();
                    storage::save(&creds_path, &creds)?;
                }
                println!(
                    "Logged in as {} ({})",
                    principal.display_name(),
                    profile::build_env_name()
                );
            }
            Err(e) => {
                // The tokens are valid and stored; only the identity lookup failed.
                println!(
                    "Logged in ({}). Could not fetch identity: {e}",
                    profile::build_env_name()
                );
            }
        }

        // Federation lives in the running daemon, which would otherwise only see
        // this login on its next poll. Poke it so it re-resolves the now-saved
        // credentials and federates immediately. Strict: if federation cannot be
        // established (no daemon, unreachable/untrusted router, apply timeout, or
        // no upstream), this returns an actionable error and the command exits
        // non-zero. The credentials were already saved above, so the user stays
        // authenticated — only the command fails.
        super::poke_federation_and_report(
            &dirs,
            config.federation.connect_timeout_secs,
            super::FederationPokeAction::Login,
        )
    }
}
