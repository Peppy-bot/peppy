//! `peppy platform login`: OAuth 2.0 device-authorization login (RFC 8628),
//! then the selection of the workspace and the project.
//!
//! Fetches the public `/cli/auth-config`, runs OIDC discovery against the
//! returned issuer, performs the device flow (printing the platform's own device
//! page with the code filled in, and opening it in the browser on a TTY), caches
//! the tokens as the single session, and prints the resolved identity. It then
//! runs the selection `peppy platform configure` runs, unless a context of this
//! identity exists. Signing in changes nothing about the daemon: joining a
//! project's router is `peppy platform enroll`.

use std::sync::Arc;

use daemon_config::consts::PeppyDirs;

use crate::commands::Command;
use crate::commands::platform::PlatformSession;
use crate::commands::platform::configure::{select_and_save, selected_report};
use crate::commands::platform::select::Ask;
use crate::context::AppContext;
use crate::error::Result;
use auth::device::{self, DeviceAuthorization, TokenSet};
use auth::discovery::OidcEndpoints;
use auth::{
    PlatformApi, cli_config, client, discovery, http::HttpClient, profile, resolver, storage,
};
use url::Url;

pub struct LoginCommand {
    /// Override the backend base URL (else the build's `resource_servers.api` /
    /// `PEPPY_API_URL`).
    pub api_url: Option<String>,
    /// Suppress the automatic browser launch.
    pub no_browser: bool,
    /// Sign in only; do not select a workspace and a project.
    pub no_configure: bool,
    /// How the selection asks the person: on the terminal, from a supplied
    /// reader, or not at all.
    pub ask: Ask,
    /// Test seam: override the peppy data dirs (defaults to the global root).
    /// The credentials file, the context and `peppy_config.json5` derive from
    /// it, so a test isolates all state under one tempdir without touching
    /// `PEPPY_HOME`.
    pub peppy_dirs: Option<PeppyDirs>,
}

impl Command for LoginCommand {
    fn execute(mut self, _ctx: &Arc<AppContext>) -> Result<()> {
        let session = PlatformSession::resolve(self.peppy_dirs, self.api_url.as_deref())?;
        let PlatformSession {
            api_url,
            creds_path,
            http,
            ..
        } = &session;

        let policy = profile::build_transport_policy();
        let cfg = cli_config::fetch(http, api_url, policy)?;
        let endpoints = discovery::discover(http, &cfg.issuer, policy)?;
        let tokens = run_device_flow(
            http,
            &endpoints,
            &cfg.client_id,
            &cfg.scopes,
            &cfg.device_verification_uri,
            self.no_browser,
        )?;

        // Persist immediately so a transient `/me` failure can't lose a good login.
        // Load-resilient: a malformed or version-mismatched file fails to parse
        // with `Error::Auth`; start fresh rather than wedge login on it (the
        // stale file self-heals on this save).
        let mut creds = match storage::load(creds_path) {
            Ok(creds) => creds,
            Err(auth::AuthError::Auth(_)) => storage::Credentials::default(),
            Err(e) => return Err(e.into()),
        };
        let pc = client::creds_from_login(&cfg, api_url, &tokens);
        creds.session = Some(pc.clone());
        storage::save(creds_path, &creds)?;

        // Fetch identity using the in-memory credential (the token was minted
        // seconds ago, so there's no need to reload from disk or proactively
        // refresh via the resolver).
        let mut api =
            PlatformApi::new(http, api_url, resolver::session_credential(creds_path, &pc));
        match api.get_me() {
            Ok(principal) => {
                // Cache display identity against the stored session.
                if let Some(session) = creds.session.as_mut() {
                    session.subject = principal.sub.clone();
                    session.username = principal.display_name().to_string();
                    storage::save(creds_path, &creds)?;
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

        if !self.no_configure {
            select_after_sign_in(&session, &mut api, &mut self.ask);
        }

        let enrolled = auth::enrollment::load(&session.dirs).is_ok_and(|e| e.is_some());
        if !enrolled {
            println!(
                "This machine is not enrolled in a project; run `peppy platform enroll` to join one."
            );
        }
        Ok(())
    }
}

/// Selects the workspace and the project after a sign-in. A context of this
/// backend and this identity is kept as it is. Every other stored context
/// belongs to a different account, so it is removed before the selection.
///
/// The sign-in is good whatever happens here, so a selection that cannot
/// complete prints its reason and the command that runs it again, and does
/// not fail the login.
fn select_after_sign_in(session: &PlatformSession, api: &mut PlatformApi, ask: &mut Ask) {
    if let Ok(Some(context)) = session.context() {
        print!("{}", selected_report(session, &context));
        return;
    }
    let selected = auth::context::remove(&session.dirs)
        .map_err(crate::error::Error::from)
        .and_then(|()| select_and_save(session, api, None, None, ask));
    match selected {
        Ok(context) => print!("{}", selected_report(session, &context)),
        Err(e) => println!(
            "No workspace and project were selected: {e}\n\
             Run `peppy platform configure` to select them."
        ),
    }
}

/// The interactive shell around the engine's device-flow protocol: print the
/// page to open and the user code, open the browser on a TTY (best-effort,
/// suppressed by `no_browser` for headless/SSH use), and show a spinner while
/// polling the token endpoint for the user's approval.
fn run_device_flow(
    http: &HttpClient,
    endpoints: &OidcEndpoints,
    client_id: &str,
    scopes: &str,
    device_page: &Url,
    no_browser: bool,
) -> Result<TokenSet> {
    use std::io::IsTerminal;

    let da = device::start(http, endpoints, client_id, scopes)?;

    let prompt = sign_in_prompt(device_page, &da);
    print!("{}", prompt.text);

    if !no_browser && std::io::stdout().is_terminal() {
        // Best-effort: a headless box without a browser just keeps the printed URL.
        if open::that(prompt.link.as_str()).is_ok() {
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

/// What a person is shown and sent to for a started device login: the
/// platform's own device page, whose link already carries the code, and the
/// code itself, so the person can compare it with the one the page shows.
/// Built from the authorization and the page alone, so nothing but the
/// platform's page can reach the terminal or the browser.
struct SignInPrompt {
    text: String,
    link: Url,
}

fn sign_in_prompt(device_page: &Url, authorization: &DeviceAuthorization) -> SignInPrompt {
    let user_code = &authorization.user_code;
    let link = device::verification_link(device_page, user_code);
    let text = format!(
        "To sign in, open:\n    {link}\nand check that the page shows the code: {user_code}\n"
    );
    SignInPrompt { text, link }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// T1: an identity provider answer that names its own pages, on another
    /// host, reaches neither the printed text nor the browser.
    #[test]
    fn only_the_platform_page_is_printed_or_opened() {
        let authorization: DeviceAuthorization = serde_json::from_value(json!({
            "device_code": "the-device-code",
            "user_code": "ABCD-EFGH",
            "verification_uri": "https://issuer.example.test/device",
            "verification_uri_complete": "https://issuer.example.test/device?user_code=ABCD-EFGH",
            "expires_in": 300,
            "interval": 5,
        }))
        .expect("a provider answer");
        let page = Url::parse("https://app.example.test/device").expect("page");

        let prompt = sign_in_prompt(&page, &authorization);

        assert_eq!(
            prompt.text,
            "To sign in, open:\n    https://app.example.test/device?user_code=ABCD-EFGH\n\
             and check that the page shows the code: ABCD-EFGH\n"
        );
        assert_eq!(
            prompt.link.as_str(),
            "https://app.example.test/device?user_code=ABCD-EFGH"
        );
        assert!(!prompt.text.contains("issuer.example.test"));
        assert_eq!(prompt.link.host_str(), Some("app.example.test"));
    }
}
