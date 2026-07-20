//! `peppy platform login`: establish OAuth credentials and delegate certificate
//! enrollment/application to the running daemon. The CLI never writes identity
//! files or performs certificate rotation in the normal path.

use std::sync::Arc;

use daemon::control as daemon_control;
use daemon_config::consts::PeppyDirs;
use secrecy::ExposeSecret;

use crate::commands::Command;
use crate::context::AppContext;
use crate::error::{Error, Result};
use auth::device::{self, TokenSet};
use auth::discovery::OidcEndpoints;
use auth::{cli_config, client, discovery, http::HttpClient, profile, resolver, storage};

pub struct LoginCommand {
    pub api_url: Option<String>,
    pub no_browser: bool,
    pub yes: bool,
    pub peppy_dirs: Option<PeppyDirs>,
    /// Presence of the CLI process's `PEPPY_API_KEY`. The value is never sent
    /// over the control protocol; PAT login delegates validation to the daemon.
    pub pat: Option<String>,
}

impl Command for LoginCommand {
    fn execute(self, ctx: &Arc<AppContext>) -> Result<()> {
        let dirs = self.peppy_dirs.unwrap_or_default();

        // This must remain the first external action. In particular,
        // `load_or_create` may complete config on disk, and neither OAuth nor
        // PAT validation may begin until client and daemon agree on protocol v1.
        super::require_daemon_hello(&dirs, false)?;

        let config =
            daemon_config::peppy_config::load_or_create(&dirs).map_err(Error::DaemonConfig)?;
        let socket = daemon_control::federation_control_socket_path(&dirs);
        let daemon_status =
            daemon_control::status(&socket, super::CONTROL_HELLO_TIMEOUT).map_err(|error| {
                super::control_error("inspect the daemon authentication mode", error, false)
            })?;
        if self.pat.is_none() && daemon_status.pat_active {
            return Err(Error::Auth(
                "the daemon service PEPPY_API_KEY is active; remove it and restart the daemon before starting OAuth login"
                    .into(),
            ));
        }
        if self.pat.is_some() && !daemon_status.pat_active {
            return Err(Error::Auth(
                "this shell has PEPPY_API_KEY, but the running daemon service does not; configure the key for the daemon and restart it before PAT login"
                    .into(),
            ));
        }
        if !super::confirm_restart(ctx, self.yes, &super::FederationPokeAction::Login)? {
            println!("Login aborted.");
            return Ok(());
        }

        let api_url = profile::resolve_api_url(self.api_url.as_deref(), &config.resource_servers)?;
        let creds_path = storage::credentials_path(&dirs);
        let http = HttpClient::new();

        // A PAT never enters credentials.json5 or the control request. The
        // CLI first validates its own ambient value, then the daemon independently
        // validates the PAT captured from its service environment. This avoids
        // treating one process's successful check as proof about the other.
        if let Some(pat) = self.pat {
            let mut credential = resolver::resolve(&creds_path, &http, Some(pat))?;
            let principal = client::get_me(&http, &api_url, &mut credential).map_err(|error| {
                Error::ExecutionFailed(format!(
                    "the CLI PEPPY_API_KEY could not be validated: {error}; no credentials or \
                     certificate material were changed"
                ))
            })?;
            // The shell validation may take network time. Re-read the live
            // generation immediately before mutation so its authoritative PAT
            // mode and timeout budgets—not a pre-validation snapshot—govern
            // this request and any restart wait.
            super::require_daemon_hello(&dirs, false)?;
            let mutation_status = daemon_control::status(&socket, super::CONTROL_HELLO_TIMEOUT)
                .map_err(|error| {
                    super::control_error("refresh the daemon authentication mode", error, false)
                })?;
            if !mutation_status.pat_active {
                return Err(Error::Auth(
                    "the daemon service PEPPY_API_KEY changed while PAT login was being validated; retry against the current daemon"
                        .into(),
                ));
            }
            let control_settings = super::identity_control_settings(&dirs, &config);
            let control_timeout = super::identity_control_timeout(control_settings.timeout_secs);
            let restart_timeout = super::identity_restart_timeout(control_settings);
            let external = mutation_status.operator_managed && !mutation_status.pinned;
            let expected_api_origin = auth::identity::normalize_api_origin(&api_url)?;
            let result = daemon_control::enroll_current_credential(
                &socket,
                control_timeout,
                None,
                Some(principal.sub.clone()),
                Some(expected_api_origin),
            )
            .map_err(|error| {
                super::control_error("log in with the daemon PEPPY_API_KEY", error, false)
            })?;
            println!(
                "Authenticated via PEPPY_API_KEY as {} ({}).",
                principal.display_name(),
                profile::build_env_name()
            );
            return super::complete_login(
                &dirs,
                &socket,
                restart_timeout,
                result,
                external,
                daemon_control::AuthenticationState::Pat,
                None,
            );
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
        // Allocate the opaque login revision before Prepare, but do not publish
        // the session yet. The daemon durably binds its fail-closed transition
        // to this exact revision, so a later concurrent Prepare supersedes it.
        let pc = client::creds_from_login(&cfg, &api_url, &tokens);

        // The daemon durably enters fail-closed state before the new session
        // becomes observable. Errors or a process crash after this point leave
        // that gate armed; only the exact enrollment commit clears it.
        // Device authorization is intentionally unbounded by daemon control
        // settings. Refresh the live generation now, immediately before the
        // first mutation, in case it restarted or its configuration changed
        // while the user was in the browser.
        super::require_daemon_hello(&dirs, false)?;
        let prepare_status = daemon_control::status(&socket, super::CONTROL_HELLO_TIMEOUT)
            .map_err(|error| {
                super::control_error("refresh the daemon authentication mode", error, false)
            })?;
        if prepare_status.pat_active {
            return Err(Error::Auth(
                "the daemon service PEPPY_API_KEY became active during OAuth authorization; remove it, restart the daemon, and retry"
                    .into(),
            ));
        }
        let prepare_settings = super::identity_control_settings(&dirs, &config);
        super::prepare_login_transition(
            &dirs,
            super::identity_control_timeout(prepare_settings.timeout_secs),
            pc.session_revision,
        )?;

        // Fresh login creates a new opaque session revision. Only OAuth state
        // is published here; identity material remains daemon-owned.
        auth::identity::publish_oauth_session(&dirs, pc.clone())?;

        let mut credential = resolver::session_credential(&creds_path, &pc);
        let principal = client::get_me(&http, &api_url, &mut credential).map_err(|error| {
            Error::ExecutionFailed(format!(
                "OAuth tokens were saved, but the authenticated platform identity could not be \
                 resolved: {error}. Re-run `peppy platform login`."
            ))
        })?;

        // `/me` may reactively refresh. Capture its exact post-request snapshot
        // and CAS the display fields without allowing a concurrent fresh login
        // to receive this response.
        let exact_session = resolver::ensure_session_credential_current(&credential)?
            .ok_or(auth::AuthError::NotAuthenticated)?;
        let updated = storage::update(&creds_path, |creds| {
            let Some(session) = creds.session.as_mut() else {
                return Err(auth::AuthError::NotAuthenticated);
            };
            if session.session_revision != exact_session.session_revision {
                return Err(auth::AuthError::StaleSessionRevision);
            }
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

        println!(
            "Logged in as {} ({}).",
            principal.display_name(),
            profile::build_env_name()
        );

        // The revision is the only login identity sent over the local protocol.
        // Bearers, refresh tokens, private keys and certificate bodies never
        // cross the socket.
        // `/me` and credential publication are another observable interval.
        // Refresh once more so enrollment and a possible restart use budgets
        // from the daemon generation that is about to acknowledge them.
        super::require_daemon_hello(&dirs, false)?;
        let enrollment_status = daemon_control::status(&socket, super::CONTROL_HELLO_TIMEOUT)
            .map_err(|error| {
                super::control_error("refresh the daemon authentication mode", error, false)
            })?;
        if enrollment_status.pat_active {
            return Err(Error::Auth(
                "the daemon service PEPPY_API_KEY became active before OAuth enrollment; the saved session was retained, but identity remains fail-closed"
                    .into(),
            ));
        }
        let enrollment_settings = super::identity_control_settings(&dirs, &config);
        let control_timeout = super::identity_control_timeout(enrollment_settings.timeout_secs);
        let restart_timeout = super::identity_restart_timeout(enrollment_settings);
        let external = enrollment_status.operator_managed && !enrollment_status.pinned;
        let result = daemon_control::enroll_current_credential(
            &socket,
            control_timeout,
            Some(updated.session_revision),
            None,
            None,
        )
        .map_err(|error| {
            super::control_error("enroll the current platform credential", error, false)
        })?;
        super::complete_login(
            &dirs,
            &socket,
            restart_timeout,
            result,
            external,
            daemon_control::AuthenticationState::Oauth,
            Some(updated.session_revision),
        )
    }
}

fn run_device_flow(
    http: &HttpClient,
    endpoints: &OidcEndpoints,
    client_id: &str,
    scopes: &str,
    no_browser: bool,
) -> Result<TokenSet> {
    use std::io::IsTerminal;

    let authorization = device::start(http, endpoints, client_id, scopes)?;
    let complete = authorization
        .verification_uri_complete
        .clone()
        .unwrap_or_else(|| authorization.verification_uri.clone());

    println!("To sign in, open:\n    {}", authorization.verification_uri);
    println!("and enter the code: {}", authorization.user_code);
    if !no_browser && std::io::stdout().is_terminal() && open::that(&complete).is_ok() {
        println!("(opened your browser…)");
    }

    let spinner = crate::terminal::spinner("Waiting for you to approve in the browser…");
    let result = device::poll(http, &endpoints.token_endpoint, client_id, &authorization);
    if let Some(progress) = spinner {
        progress.finish_and_clear();
    }
    Ok(result?)
}
