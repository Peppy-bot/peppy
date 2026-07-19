//! `peppy platform login`: OAuth 2.0 device-authorization login (RFC 8628),
//! or an immediate PAT-authenticated federation when `PEPPY_API_KEY` is set.
//!
//! OAuth path: fetches the public `/cli/auth-config`, runs OIDC discovery
//! against the returned issuer, performs the device flow (opening the browser
//! on a TTY), caches the tokens as the single session, and prints the resolved
//! identity. PAT path: verifies the key against `/me`, never persists the PAT
//! itself, enrolls the production core-node certificate, and goes straight to
//! the federation poke.

use std::sync::Arc;

use daemon_config::consts::PeppyDirs;
use secrecy::ExposeSecret;

use crate::commands::Command;
use crate::context::AppContext;
use crate::error::{Error, Result};
use auth::device::{self, TokenSet};
use auth::discovery::OidcEndpoints;
use auth::{cli_config, client, discovery, http::HttpClient, identity, profile, resolver, storage};

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
    /// The `PEPPY_API_KEY` PAT, injected by the dispatcher (never read from
    /// the environment here). `Some` skips the OAuth device flow entirely.
    pub pat: Option<String>,
}

impl Command for LoginCommand {
    fn execute(self, ctx: &Arc<AppContext>) -> Result<()> {
        let dirs = self.peppy_dirs.unwrap_or_default();
        // Loads (and seeds/completes) peppy_config.json5 with the same strict,
        // fail-loud semantics the daemon uses; resource_servers supplies the
        // per-profile URL fallback.
        let config =
            daemon_config::peppy_config::load_or_create(&dirs).map_err(Error::DaemonConfig)?;
        // Managed vs external follows the RUNNING daemon's mode (from its state
        // file), not the disk config, which may have been edited since it
        // started; only with no daemon running does the disk config decide.
        let federation = super::federation_poke_timeout_secs(&dirs, &config);
        let api_url = profile::resolve_api_url(self.api_url.as_deref(), &config.resource_servers)?;
        let creds_path = storage::credentials_path(&dirs);
        let http = HttpClient::new();

        // With a managed router, warn (before authentication begins) that a login
        // changing the workspace namespace restarts the daemon and wipes the
        // running node stack. Bypassed by `--yes`, and skipped when no daemon is
        // running or its node stack holds no user nodes (so the restart wipes
        // nothing). External mode never pokes or restarts the daemon.
        if federation.is_some()
            && !super::confirm_restart(ctx, self.yes, &super::FederationPokeAction::Login)?
        {
            println!("Login aborted.");
            return Ok(());
        }

        // PAT fast path: `PEPPY_API_KEY` is valid platform authentication on
        // its own and takes precedence over stored OAuth credentials, so login
        // skips the device flow entirely and applies federation immediately.
        // The PAT itself is never persisted (it is environment-scoped and
        // `platform logout` cannot clear it). Production still persists the
        // non-secret certificate metadata and protected key/chain generation.
        // Strict: a rejected key or unreachable backend fails before any poke.
        if let Some(pat) = self.pat {
            let _auth_operation = identity::acquire_platform_auth_operation(&dirs)?;
            let identity_maintenance = identity::acquire_identity_maintenance(&dirs)?;
            let mut cred = resolver::resolve(&creds_path, &http, Some(pat))?;
            let principal = client::get_me(&http, &api_url, &mut cred)?;
            let mut durable_auth_changed = false;
            let result = (|| -> Result<()> {
                let rotation = if identity::production_identity_required() {
                    // Only after the PAT is proven valid, replace any stale stored
                    // OAuth mode without writing the PAT. Normalizing here heals
                    // v2/corrupt credentials before certificate publication. The
                    // debug shared-certificate path retains its historical
                    // environment-only/no-file PAT behavior. Arm fail-closed
                    // cleanup before the durable transaction so even an uncertain
                    // post-publication I/O error cannot leave an old link applied.
                    identity::arm_binding_incomplete(&dirs)?;
                    durable_auth_changed = true;
                    identity_maintenance.prepare_pat_login()?;
                    let core_node_name = running_core_node_name(&dirs)?;
                    Some(identity_maintenance.enroll_and_activate(
                        &http,
                        &api_url,
                        &mut cred,
                        &principal.sub,
                        &core_node_name,
                    )?)
                } else {
                    drop(identity_maintenance);
                    None
                };
                let had_rotation = rotation.is_some();
                if let Some(rotation) = rotation {
                    // Transfer apply/probe ownership to the daemon through the
                    // durable unverified marker before issuing the control poke.
                    // The CLI must not hold a second armed receipt that could race
                    // a daemon operation continuing after a client-side timeout.
                    rotation.retain_for_restart()?;
                }
                // The activated identity and its durable receipt are now owned
                // by the daemon handoff. Only now may resolution consider the
                // new PAT binding before the immediate login poke.
                if durable_auth_changed {
                    identity::clear_binding_incomplete(&dirs)?;
                }
                println!(
                    "Authenticated via PEPPY_API_KEY as {} ({}).",
                    principal.display_name(),
                    profile::build_env_name()
                );
                let result =
                    super::finish_federation(&dirs, federation, super::FederationPokeAction::Login);
                match result {
                    Ok(()) => Ok(()),
                    Err(error) => {
                        let rollback = rollback_if_no_daemon_owns_rotation(&dirs, had_rotation);
                        Err(Error::ExecutionFailed(format!(
                            "{error}{}\nPEPPY_API_KEY must also be present in the running daemon's service environment before it can renew or pull federation configuration.",
                            rollback
                                .err()
                                .map(|rollback| format!(
                                    "; core-node certificate rollback also failed: {rollback}"
                                ))
                                .unwrap_or_default()
                        )))
                    }
                }
            })();
            return if durable_auth_changed {
                fail_closed_after_auth_change(&dirs, federation, result)
            } else {
                result
            };
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

        // Serialize the durable session change through enrollment/publication
        // and the final daemon poke. The nested rotation guard is acquired
        // before the first credentials write, so daemon maintenance cannot race
        // this login's account binding.
        let _auth_operation = identity::acquire_platform_auth_operation(&dirs)?;
        let identity_maintenance = identity::acquire_identity_maintenance(&dirs)?;

        // Persist immediately so a transient `/me` failure can't lose a good
        // login. The transaction heals malformed legacy state and changes only
        // session/router, preserving a concurrent identity mirror.
        let pc = client::creds_from_login(&cfg, &api_url, &tokens);
        identity::arm_binding_incomplete(&dirs)?;

        // From this marker publication onward, every error must also make a
        // best-effort daemon pass that drops any previously applied binding.
        // Keep the new OAuth session intact so the user can retry enrollment.
        let result = (|| -> Result<()> {
            storage::update_or_default(&creds_path, |creds| {
                creds.session = Some(pc.clone());
                // Drop any cached router config: it is identity-bound, and this
                // login may be a different user/backend. The next connect re-pulls.
                creds.router = None;
                Ok(())
            })?;

            // Fetch identity using the in-memory credential (the token was minted
            // seconds ago, so there's no need to reload from disk or proactively
            // refresh via the resolver).
            let mut cred = resolver::session_credential(&creds_path, &pc);
            let principal = client::get_me(&http, &api_url, &mut cred).map_err(|error| {
                Error::ExecutionFailed(format!(
                    "OAuth tokens were saved, but the authenticated platform identity could not be resolved: {error}. Re-run `peppy platform login` to finish certificate enrollment."
                ))
            })?;
            // `/me` can reactively refresh and persist the token pair. Capture its
            // exact post-request session context, then CAS the display update so a
            // concurrent same-origin login cannot receive this principal's subject
            // or have its bearer used for enrollment below.
            let exact_session = resolver::ensure_session_credential_current(&cred)?
                .ok_or(auth::AuthError::NotAuthenticated)?;
            let updated_session = storage::update(&creds_path, |creds| {
                let Some(session) = creds.session.as_mut() else {
                    return Err(auth::AuthError::NotAuthenticated);
                };
                if session.api_url != exact_session.api_url
                    || session.issuer != exact_session.issuer
                    || session.client_id != exact_session.client_id
                    || session.subject != exact_session.subject
                    || session.access_token.expose_secret()
                        != exact_session.access_token.expose_secret()
                    || session.refresh_token.expose_secret()
                        != exact_session.refresh_token.expose_secret()
                {
                    return Err(auth::AuthError::NotAuthenticated);
                }
                session.subject = principal.sub.clone();
                session.username = principal.display_name().to_string();
                Ok(session.clone())
            })?;
            cred = resolver::session_credential(&creds_path, &updated_session);
            println!(
                "Logged in as {} ({})",
                principal.display_name(),
                profile::build_env_name()
            );

            // In production, the running daemon's captured name is authoritative.
            // Enroll only after `/me` resolved; any enrollment failure leaves the
            // valid OAuth session stored for an explicit retry.
            let rotation = if identity::production_identity_required() {
                let core_node_name = running_core_node_name(&dirs)?;
                match identity_maintenance.enroll_and_activate(
                    &http,
                    &api_url,
                    &mut cred,
                    &principal.sub,
                    &core_node_name,
                ) {
                    Ok(rotation) => Some(rotation),
                    Err(error) => {
                        return Err(Error::ExecutionFailed(format!(
                            "OAuth login succeeded, but core-node certificate enrollment failed: {error}. The session was retained; re-run `peppy platform login`."
                        )));
                    }
                }
            } else {
                drop(identity_maintenance);
                None
            };
            let had_rotation = rotation.is_some();
            if let Some(rotation) = rotation {
                rotation.retain_for_restart()?;
            }
            // Production handed the activated receipt to the daemon; debug has
            // completed its shared-certificate session binding. Clear the
            // crash-durable gate immediately before the login refederation.
            identity::clear_binding_incomplete(&dirs)?;

            // Managed-router federation lives in the running daemon, which would
            // otherwise only see this login on its next poll. Poke it so it
            // re-resolves the now-saved credentials and federates immediately.
            // Strict: if federation cannot be established (no daemon,
            // unreachable/untrusted router, apply timeout, or no upstream), this
            // returns an actionable error and the command exits non-zero. The
            // credentials were already saved above, so the user stays authenticated;
            // only the command fails. External mode leaves federation untouched and
            // tells the operator that sessions change on the next manual restart.
            let result =
                super::finish_federation(&dirs, federation, super::FederationPokeAction::Login);
            match result {
                Ok(()) => Ok(()),
                Err(error) if had_rotation => {
                    let rollback = rollback_if_no_daemon_owns_rotation(&dirs, true);
                    Err(Error::ExecutionFailed(format!(
                        "{error}{}",
                        rollback
                            .err()
                            .map(|rollback| format!(
                                "; core-node certificate rollback also failed: {rollback}"
                            ))
                            .unwrap_or_default()
                    )))
                }
                Err(error) => Err(error),
            }
        })();
        fail_closed_after_auth_change(&dirs, federation, result)
    }
}

/// A durable login-mode change invalidates whatever account/workspace binding
/// the running daemon may still have applied. Preserve the newly stored auth
/// state for retry, but on every later error ask the daemon to resolve it in
/// logout/fail-closed mode so a stale prior link is not left active.
fn fail_closed_after_auth_change<T>(
    dirs: &PeppyDirs,
    federation: Option<u64>,
    result: Result<T>,
) -> Result<T> {
    if result.is_err() {
        // A login poke itself can fail after the normal pre-poke clear. Re-arm
        // before the best-effort cleanup poke so the daemon is forced
        // standalone rather than reusing a same-subject prior identity.
        let _ = identity::arm_binding_incomplete(dirs);
        let _ = super::finish_federation(dirs, federation, super::FederationPokeAction::FailClosed);
    }
    result
}

/// Once the rotation receipt is handed to the daemon, a control timeout does
/// not authorize the CLI to delete files the daemon may still be applying. If
/// no daemon is alive, however, nobody can own the marker and rollback is safe
/// and immediate. A live daemon handles commit/rollback (including prior-path
/// reapply) inside its federation poll.
fn rollback_if_no_daemon_owns_rotation(dirs: &PeppyDirs, had_rotation: bool) -> auth::Result<()> {
    if !had_rotation {
        return Ok(());
    }
    let running = daemon::state::DaemonState::read_from(
        &daemon::state::DaemonState::state_file_in(dirs.root()),
    )
    .is_ok_and(|state| state.is_running());
    if !running {
        identity::rollback_unverified_rotation(dirs)?;
    }
    Ok(())
}

/// Reads the exact immutable name captured by the live `service serve`
/// generation. Production enrollment never re-resolves config or generates a
/// name independently in the CLI.
fn running_core_node_name(dirs: &PeppyDirs) -> Result<String> {
    let path = daemon::state::DaemonState::state_file_in(dirs.root());
    let state = daemon::state::DaemonState::read_from(&path).map_err(|error| {
        Error::ExecutionFailed(format!(
            "cannot enroll a core-node certificate without a running daemon state at {}: {error}. Start `peppy service serve`, then retry login.",
            path.display()
        ))
    })?;
    if !state.is_running() {
        return Err(Error::ExecutionFailed(
            "cannot enroll a core-node certificate because the recorded daemon is not running; start `peppy service serve`, then retry login"
                .into(),
        ));
    }
    if state.core_node_name.is_empty() {
        return Err(Error::ExecutionFailed(
            "the running daemon state has no core_node_name; restart the daemon before login"
                .into(),
        ));
    }
    Ok(state.core_node_name)
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
