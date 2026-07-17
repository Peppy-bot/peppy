//! `peppy auth login`: OAuth 2.0 device-authorization login (RFC 8628).
//!
//! Fetches the public `/cli/auth-config`, runs OIDC discovery against the returned
//! issuer, performs the device flow (opening the browser on a TTY), caches the
//! tokens as the single session, and prints the resolved identity.

use std::sync::Arc;

use daemon_config::consts::PeppyDirs;

use crate::commands::Command;
use crate::context::AppContext;
use crate::error::{Error, Result};
use auth::device::{self, TokenSet};
use auth::discovery::OidcEndpoints;
use auth::{cli_config, client, discovery, http::HttpClient, profile, resolver, storage};

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
        let federation = config.zenoh.federation().copied();
        let api_url = profile::resolve_api_url(self.api_url.as_deref(), &config.resource_servers)?;
        let creds_path = storage::credentials_path(&dirs);
        let http = HttpClient::new();

        // With a managed router, warn (before authentication begins) that a login
        // changing the organization namespace restarts the daemon and wipes the
        // running node stack. Bypassed by `--yes`, and skipped when no daemon is
        // running or its node stack holds no user nodes (so the restart wipes
        // nothing). External mode never pokes or restarts the daemon.
        if federation.is_some()
            && !super::confirm_restart(ctx, self.yes, &super::FederationPokeAction::Login)?
        {
            println!("Login aborted.");
            return Ok(());
        }

        let cfg = cli_config::fetch(&http, &api_url)?;
        let endpoints = discovery::discover(&http, &cfg.issuer)?;
        let tokens = run_device_flow(
            &http,
            &endpoints,
            &cfg.client_id,
            &cfg.scopes,
            self.no_browser,
        )?;

        // Persist immediately so a transient `/me` failure can't lose a good login.
        // Load-resilient: a malformed / pre-`organization_id` / version-mismatched
        // file fails to parse with `Error::Auth`; start fresh rather than wedge
        // login on it (the stale file self-heals on this save).
        let mut creds = match storage::load(&creds_path) {
            Ok(creds) => creds,
            Err(auth::AuthError::Auth(_)) => storage::Credentials::default(),
            Err(e) => return Err(e.into()),
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

        // Managed-router federation lives in the running daemon, which would
        // otherwise only see this login on its next poll. Poke it so it
        // re-resolves the now-saved credentials and federates immediately.
        // Strict: if federation cannot be established (no daemon,
        // unreachable/untrusted router, apply timeout, or no upstream), this
        // returns an actionable error and the command exits non-zero. The
        // credentials were already saved above, so the user stays authenticated;
        // only the command fails. External mode leaves federation untouched and
        // tells the operator that sessions change on the next manual restart.
        match federation {
            Some(federation) => super::poke_federation_and_report(
                &dirs,
                federation.connect_timeout_secs,
                super::FederationPokeAction::Login,
            ),
            None => {
                println!("{}", super::EXTERNAL_ROUTER_NOTE);
                Ok(())
            }
        }
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

    let da = device::start(http, endpoints, client_id, scopes)?;

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
