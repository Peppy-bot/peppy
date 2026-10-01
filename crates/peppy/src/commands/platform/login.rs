//! `peppy platform login`: OAuth 2.0 device-authorization login (RFC 8628),
//! then the enrollment of this machine in a project.
//!
//! Fetches the public `/cli/auth-config`, runs OIDC discovery against the
//! returned issuer, performs the device flow (printing the platform's own device
//! page with the code filled in, and opening it in the browser on a TTY), caches
//! the tokens as the single session, and prints the resolved identity.
//!
//! It then enrolls this machine as `peppy platform enroll` does, in the project
//! the person selects: the flags name the workspace and the project, the only
//! one is selected with no question, and with more than one the person selects
//! it on a menu that also has an entry to sign in only. The selected workspace
//! and project become the selection of the CLI. A machine that is enrolled
//! already keeps its enrollment, and `--no-enroll` signs in only.
//!
//! The command succeeds when the machine is enrolled, or when the person
//! declined the enrollment. Every other way the enrollment does not happen is
//! an error that says the session is kept and what to run next. What the CLI
//! can find out before the sign-in (no daemon to enroll, an enrollment the
//! flags would move) fails the command before the device flow starts, so the
//! person never approves a code for nothing.

use std::sync::Arc;

use daemon_config::consts::PeppyDirs;

use crate::commands::Command;
use crate::commands::platform::PlatformSession;
use crate::commands::platform::enroll::{DAEMON_NEEDED, START_THE_DAEMON, enroll_machine};
use crate::commands::platform::select::{self, Ask, Picked, ProjectInWorkspace, Question};
use crate::commands::platform::selection::save_selection;
use crate::context::AppContext;
use crate::error::{Error, Result};
use auth::device::{self, DeviceAuthorization, TokenSet};
use auth::discovery::OidcEndpoints;
use auth::{
    PlatformApi, cli_config, client, discovery, http::HttpClient, profile, resolver, storage,
};
use url::Url;

/// The menu entry that declines the enrollment.
const SIGN_IN_ONLY: &str = "Do not enroll this machine (sign in only)";

/// What a login that enrolls nothing ends with.
const SIGNED_IN_ONLY: &str =
    "Signed in only. `peppy platform enroll` enrolls this machine in a project.";

pub struct LoginCommand {
    /// Override the backend base URL (else the build's `resource_servers.api` /
    /// `PEPPY_API_URL`).
    pub api_url: Option<String>,
    /// Suppress the automatic browser launch.
    pub no_browser: bool,
    /// The workspace of the project to enroll in, by id or exact name.
    pub workspace: Option<String>,
    /// The project to enroll in, by id or exact name.
    pub project: Option<String>,
    /// Sign in only: do not enroll this machine.
    pub no_enroll: bool,
    /// Skip the daemon-restart confirmation prompt of the enrollment.
    pub yes: bool,
    /// How the selection asks the person: on the terminal, with scripted
    /// answers, or not at all.
    pub ask: Ask,
    /// Test seam: override the peppy data dirs (defaults to the global root).
    /// The credentials file, the selection, the enrollment, the daemon state
    /// and `peppy_config.json5` derive from it, so a test isolates all state
    /// under one tempdir without touching `PEPPY_HOME`.
    pub peppy_dirs: Option<PeppyDirs>,
}

impl Command for LoginCommand {
    fn execute(mut self, ctx: &Arc<AppContext>) -> Result<()> {
        let session = PlatformSession::resolve(self.peppy_dirs, self.api_url.as_deref())?;
        let enrollment = session.enrollment()?;
        let named_a_project = self.workspace.is_some() || self.project.is_some();
        if let Some(enrollment) = &enrollment
            && named_a_project
        {
            return Err(Error::ExecutionFailed(format!(
                "this machine is already enrolled in project {} as {}. To sign in only: `peppy \
                 platform login --no-enroll`. To move the machine after that: `peppy platform \
                 enroll --replace` with the same --workspace and --project",
                enrollment.document.project_id, enrollment.document.peer_name
            )));
        }
        let will_enroll = !self.no_enroll && enrollment.is_none();
        if will_enroll && session.running_daemon().is_none() {
            return Err(Error::ExecutionFailed(format!(
                "no peppy daemon is running, and `peppy platform login` enrolls this machine. \
                 {DAEMON_NEEDED} {START_THE_DAEMON}, or pass --no-enroll to sign in only."
            )));
        }

        let mut api = sign_in(&session, self.no_browser)?;
        forget_the_selection_of_another_identity(&session)?;

        if let Some(enrollment) = enrollment {
            println!(
                "This machine stays enrolled in project {} as {}. To move it to a different \
                 project: `peppy platform enroll --replace`",
                enrollment.document.project_id, enrollment.document.peer_name
            );
            return Ok(());
        }
        if self.no_enroll {
            println!("{SIGNED_IN_ONLY}");
            return Ok(());
        }

        let picked = choose_the_project_to_enroll_in(
            &session,
            &mut api,
            self.workspace.as_deref(),
            self.project.as_deref(),
            &mut self.ask,
        )
        .map_err(not_enrolled)?;
        let Picked::One(target) = picked else {
            println!("{SIGNED_IN_ONLY}");
            return Ok(());
        };
        save_selection(
            &session,
            &mut api,
            (&target.workspace).into(),
            Some((&target.project).into()),
        )
        .map_err(not_enrolled)?;

        // Read anew: the daemon can stop while the person signs in.
        let daemon = session.running_daemon().ok_or_else(|| {
            not_enrolled(Error::ExecutionFailed(format!(
                "the peppy daemon stopped. {START_THE_DAEMON}, then run `peppy platform enroll`: \
                 it enrolls this machine in the selected project"
            )))
        })?;
        enroll_machine(ctx, &session, &mut api, &daemon, &target, None, self.yes)
    }
}

/// The error of an enrollment that did not happen after a good sign-in.
fn not_enrolled(error: Error) -> Error {
    Error::Auth(format!(
        "signed in, but this machine is not enrolled: {error}"
    ))
}

/// Runs the device flow, stores the session, and prints the identity it
/// resolves. Returns the platform API with the new session.
fn sign_in(session: &PlatformSession, no_browser: bool) -> Result<PlatformApi> {
    let PlatformSession {
        api_url,
        creds_path,
        http,
        ..
    } = session;

    let policy = profile::build_transport_policy();
    let cfg = cli_config::fetch(http, api_url, policy)?;
    let endpoints = discovery::discover(http, &cfg.issuer, policy)?;
    let tokens = run_device_flow(
        http,
        &endpoints,
        &cfg.client_id,
        &cfg.scopes,
        &cfg.device_verification_uri,
        no_browser,
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
    let mut api = PlatformApi::new(http, api_url, resolver::session_credential(creds_path, &pc));
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
    Ok(api)
}

/// Removes a stored selection that is not one of this identity on this
/// backend: its ids belong to a different account. A selection that cannot be
/// read goes too, and so does every selection when the identity of the
/// session is not known.
fn forget_the_selection_of_another_identity(session: &PlatformSession) -> Result<()> {
    if matches!(session.selection(), Ok(Some(_))) {
        return Ok(());
    }
    Ok(auth::selection::remove(&session.dirs)?)
}

/// The workspace, then the project, to enroll this machine in. Each menu
/// starts on the selected one and ends with the entry that signs in only.
fn choose_the_project_to_enroll_in(
    session: &PlatformSession,
    api: &mut PlatformApi,
    workspace_flag: Option<&str>,
    project_flag: Option<&str>,
    ask: &mut Ask,
) -> Result<Picked<ProjectInWorkspace>> {
    let current = session.selection()?;
    let question = Question {
        title: "Workspace to enroll this machine in",
        flag: workspace_flag,
        default_id: None,
        current_id: current.as_ref().map(|s| s.workspace.id.as_str()),
        decline: Some(SIGN_IN_ONLY),
        remedy: "run `peppy platform enroll --workspace <id|name>` to enroll it",
    };
    let Picked::One(workspace) = select::pick_workspace(api, question, ask)? else {
        return Ok(Picked::Declined);
    };

    let title = format!("Project of {} to enroll this machine in", workspace.name);
    let remedy = format!(
        "run `peppy platform enroll --workspace {} --project <id|name>` to enroll it",
        workspace.id
    );
    let question = Question {
        title: &title,
        flag: project_flag,
        default_id: None,
        current_id: select::selected_project_in(current.as_ref(), &workspace)
            .map(|p| p.id.as_str()),
        decline: Some(SIGN_IN_ONLY),
        remedy: &remedy,
    };
    let Picked::One(project) = select::pick_project(api, &workspace, question, ask)? else {
        return Ok(Picked::Declined);
    };
    Ok(Picked::One(ProjectInWorkspace { workspace, project }))
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
