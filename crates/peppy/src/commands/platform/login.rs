//! `peppy platform login`: OAuth 2.0 device-authorization login (RFC 8628).
//!
//! Fetches the public `/cli/auth-config`, runs OIDC discovery against the
//! returned issuer, performs the device flow (opening the browser on a TTY),
//! caches the tokens as the single session, and prints the resolved identity.
//! Signing in changes nothing about the daemon: joining a project's router is
//! `peppy platform enroll`.

use std::sync::Arc;

use daemon_config::consts::PeppyDirs;

use crate::commands::Command;
use crate::context::AppContext;
use crate::error::Result;
use auth::device::{self, TokenSet};
use auth::discovery::OidcEndpoints;
use auth::{cli_config, client, discovery, http::HttpClient, profile, resolver, storage};

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
        let super::PlatformSession {
            dirs,
            api_url,
            creds_path,
            http,
            ..
        } = super::PlatformSession::resolve(self.peppy_dirs, self.api_url.as_deref())?;

        let policy = profile::build_transport_policy();
        let cfg = cli_config::fetch(&http, &api_url, policy)?;
        let endpoints = discovery::discover(&http, &cfg.issuer, policy)?;
        let tokens = run_device_flow(
            &http,
            &endpoints,
            &cfg.client_id,
            &cfg.scopes,
            self.no_browser,
        )?;

        // Persist immediately so a transient `/me` failure can't lose a good login.
        // Load-resilient: a malformed or version-mismatched file fails to parse
        // with `Error::Auth`; start fresh rather than wedge login on it (the
        // stale file self-heals on this save).
        let mut creds = match storage::load(&creds_path) {
            Ok(creds) => creds,
            Err(auth::AuthError::Auth(_)) => storage::Credentials::default(),
            Err(e) => return Err(e.into()),
        };
        let pc = client::creds_from_login(&cfg, &api_url, &tokens);
        creds.session = Some(pc.clone());
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

        let enrolled = auth::enrollment::load(&dirs).is_ok_and(|e| e.is_some());
        if !enrolled {
            println!(
                "This machine is not enrolled in a project; run `peppy platform enroll` to join one."
            );
        }
        Ok(())
    }
}

/// The interactive shell around the engine's device-flow protocol: print the
/// verification URL and user code, open the browser on a TTY (best-effort,
/// suppressed by `no_browser` for headless/SSH use), and show a spinner while
/// polling the token endpoint for the user's approval.
fn run_device_flow(
    http: &HttpClient,
    endpoints: &OidcEndpoints,
    client_id: &str,
    scopes: &str,
    no_browser: bool,
) -> Result<TokenSet> {
    use std::io::IsTerminal;

    let da = device::start(
        http,
        endpoints,
        client_id,
        scopes,
        profile::build_transport_policy(),
    )?;

    let complete = da
        .verification_uri_complete
        .clone()
        .unwrap_or_else(|| da.verification_uri.clone());

    println!("To sign in, open:\n    {}", da.verification_uri);
    println!("and enter the code: {}", da.user_code);

    if !no_browser && std::io::stdout().is_terminal() {
        // Best-effort: a headless box without a browser just keeps the printed URL.
        if open::that(&complete).is_ok() {
            println!("(opened your browser…)");
        }
    }

    let spinner = crate::terminal::spinner("Waiting for you to approve in the browser…");
    let result = device::poll(http, &endpoints.token_endpoint, client_id, &da);
    if let Some(pb) = spinner {
        pb.finish_and_clear();
    }
    Ok(result?)
}
