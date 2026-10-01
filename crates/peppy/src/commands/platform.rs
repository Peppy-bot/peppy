//! The `peppy platform` command group: sign in and out of the platform
//! (`login`, `logout`, `whoami`), select the workspace and the project the
//! commands act on (`workspace`, `project`), look at the peers of a project
//! (`peers`), join or leave a project's cloud router (`enroll`, `unenroll`,
//! `status`), and restart or start that router (`router`). Each variant maps
//! to a handler in this module's directory; the OAuth device flow, token
//! storage, the platform API and the enrollment store they share live in the
//! separate `auth` engine crate, and the config, URL and credential preamble
//! they all repeat lives here as [`PlatformSession`].
//!
//! A session is what the CLI needs to call the API; an enrollment is what the
//! daemon needs to federate its router, and it outlives the session. `login`
//! signs in and then enrolls this machine, unless the person declines;
//! `enroll` enrolls a machine that has a session. The selection is the default
//! target of the commands: it names a workspace and a project to the CLI and
//! changes nothing on the daemon. `enroll` and `unenroll` change the daemon's
//! identity (its router id and session namespace), so they poke the running
//! daemon over its control socket and wait for it to restart under the new
//! identity; a poke that finds the identity unchanged verifies the link.

pub mod enroll;
pub mod login;
pub mod logout;
pub mod peers;
pub mod project;
pub mod router;
pub mod select;
pub mod selection;
pub mod status;
pub mod unenroll;
pub mod whoami;
pub mod workspace;

use std::sync::Arc;
use std::time::{Duration, Instant};

use clap::Subcommand;
use core_node_api::encoding::StackListRequest;
use core_node_api::{NodeStage, SerializedNodeGraph};
use daemon_config::consts::PeppyDirs;
use peppylib::core_node::transport::poll;

use auth::{FederationIdentity, PlatformApi, http::HttpClient, profile, storage};

use super::Command;
use crate::commands::CALLER_INSTANCE_ID;
use crate::error::Error;
use crate::{context::AppContext, error::Result};
use daemon::control::{self as daemon_control, PokeOutcome};
use daemon::state::DaemonState;

/// Shown when the managed router uses an operator-pinned config, so the daemon
/// cannot render the enrollment into it.
const PINNED_NOTE: &str = "Note: this daemon's router uses an operator-pinned ZENOH_CONFIG; \
     its federation is not managed by the enrollment.";

/// Shown after enroll/unenroll in external mode. No federation control task
/// exists in that mode, so the CLI deliberately leaves the operator's router
/// alone and tells them where the material is.
fn external_router_note(dirs: &PeppyDirs) -> String {
    format!(
        "Note: this daemon dials an operator-run router (`zenoh.external`); federation belongs \
         to the operator and was left untouched. The enrollment material is under {} for the \
         operator's router to dial the project router with. Restart the daemon to apply the new \
         namespace to its sessions.",
        dirs.peer_dir().display()
    )
}

/// Re-poke cadence and overall deadline while waiting for the daemon to restart
/// under the new identity. The deadline covers zenohd's readiness ceiling (30s)
/// plus slack.
const RESTART_POLL_INTERVAL: Duration = Duration::from_millis(250);
const RESTART_POLL_DEADLINE: Duration = Duration::from_secs(60);

/// Upper bound on the pre-prompt probe that asks the running daemon whether its
/// node stack holds any user nodes. Kept short so a sluggish or half-up daemon
/// (pid alive but its messaging router not yet reachable) delays the prompt
/// only briefly before we fall back to showing the warning.
const STACK_PROBE_TIMEOUT: Duration = Duration::from_secs(3);

/// Whether a federation poke follows an enrollment or an unenrollment. Affects
/// the user-facing wording and whether a federation failure is fatal: an
/// enrollment whose link cannot be verified fails (the enrollment is kept),
/// while an unenrollment is always best-effort.
pub(crate) enum FederationPokeAction {
    Enroll,
    Unenroll,
}

impl FederationPokeAction {
    fn subcommand(&self) -> &'static str {
        match self {
            Self::Enroll => "enroll",
            Self::Unenroll => "unenroll",
        }
    }
}

/// Whether enroll/unenroll should warn about a restart and poke the control
/// socket (managed mode) or leave federation to the operator (external mode).
///
/// A RUNNING daemon is authoritative: its state file records whether that
/// generation binds a control socket, so a config edited on disk after it
/// started can neither make a command poke a socket that does not exist
/// (external daemon, managed config on disk) nor skip the poke a managed daemon
/// needs (managed daemon, external config on disk). Only when no daemon is
/// running, so there is nothing to poke or restart either way, does the on-disk
/// `config` decide, matching what the next daemon start will do.
pub(crate) fn federation_is_managed(
    state: Option<&DaemonState>,
    config: &daemon_config::peppy_config::PeppyConfig,
) -> bool {
    match state {
        Some(state) if state.is_running() => state.has_federation_control(),
        _ => config.zenoh.external_endpoint().is_none(),
    }
}

/// The daemon state file under `dirs`, or `None` when it is absent or
/// unreadable. Read from the same `dirs` the command resolved, so a test seam
/// isolates it.
///
/// The file outlives the daemon process, so a successful read is not proof of
/// liveness; consumers that need "is a daemon actually up" check
/// [`DaemonState::is_running`] themselves.
pub(crate) fn read_daemon_state(dirs: &PeppyDirs) -> Option<DaemonState> {
    DaemonState::read_from(&DaemonState::state_file_in(dirs.root())).ok()
}

/// What every `peppy platform` command resolves before it can talk to the
/// backend: load (and seed) the peppy config with the daemon's own strict
/// semantics, resolve the API URL through the profile fallback, locate the
/// credentials file, and build an HTTP client. The daemon's state rides along
/// because it decides managed-vs-external for `unenroll` and backs `status`;
/// reading it never fails the command, since a machine with no daemon running
/// is a normal case. `enroll` reads the daemon anew with
/// [`PlatformSession::running_daemon`].
pub(crate) struct PlatformSession {
    pub dirs: PeppyDirs,
    pub config: daemon_config::peppy_config::PeppyConfig,
    pub api_url: String,
    pub creds_path: std::path::PathBuf,
    pub http: HttpClient,
    /// The running daemon's recorded state. `None` when no daemon is running,
    /// or its state file is absent or unreadable.
    pub daemon_state: Option<DaemonState>,
}

impl PlatformSession {
    pub(crate) fn resolve(peppy_dirs: Option<PeppyDirs>, api_url: Option<&str>) -> Result<Self> {
        let dirs = peppy_dirs.unwrap_or_default();
        let config =
            daemon_config::peppy_config::load_or_create(&dirs).map_err(Error::DaemonConfig)?;
        let resolved_api_url = profile::resolve_api_url(api_url, &config.resource_servers)?;
        Ok(Self {
            creds_path: storage::credentials_path(&dirs),
            daemon_state: read_daemon_state(&dirs),
            dirs,
            config,
            api_url: resolved_api_url,
            http: HttpClient::new(),
        })
    }

    /// The platform API with the bearer of the cached session, or the
    /// not-authenticated error naming `peppy platform login`.
    pub(crate) fn api(&self) -> Result<PlatformApi> {
        let credential = auth::resolver::resolve(&self.creds_path, &self.http)?;
        Ok(PlatformApi::new(&self.http, &self.api_url, credential))
    }

    /// This machine's enrollment, `None` when it is not enrolled. A present
    /// enrollment that cannot be read is an error that names `peppy platform
    /// enroll`.
    pub(crate) fn enrollment(&self) -> Result<Option<auth::Enrollment>> {
        Ok(auth::enrollment::load(&self.dirs)?)
    }

    /// The origin of the platform API this session talks to, as a selection
    /// records it.
    pub(crate) fn api_origin(&self) -> Result<String> {
        Ok(profile::normalize_api_origin(&self.api_url)?)
    }

    /// The session as the credentials file holds it now, or `None` when
    /// there is no session or the file cannot be read.
    pub(crate) fn cached_session(&self) -> Option<auth::ProfileCreds> {
        storage::load(&self.creds_path).ok()?.session
    }

    /// The subject of the signed-in identity as the session cached it, or
    /// `None` when there is no session or its identity is not known.
    pub(crate) fn subject(&self) -> Option<String> {
        self.cached_session()
            .map(|session| session.subject)
            .filter(|subject| !subject.is_empty())
    }

    /// The selection the commands of this session use: the stored one when it
    /// belongs to this backend and this identity, else `None`. A stored file
    /// that cannot be read is an error that names `peppy platform project
    /// use`.
    pub(crate) fn selection(&self) -> Result<Option<auth::PlatformSelection>> {
        let Some(subject) = self.subject() else {
            return Ok(None);
        };
        let api_origin = self.api_origin()?;
        Ok(auth::selection::load(&self.dirs)?
            .filter(|selection| selection.belongs_to(&api_origin, &subject)))
    }

    /// The daemon that runs on this machine now, read anew from its state file
    /// under `dirs`, or `None` when no daemon runs. An enrollment needs it: the
    /// peer takes the name of its core node, and it is what joins the router.
    pub(crate) fn running_daemon(&self) -> Option<DaemonState> {
        read_daemon_state(&self.dirs).filter(DaemonState::is_running)
    }

    /// The project whose router a command acts on (`peers`, `router`): the
    /// flags, else the selected project, else the project this machine is
    /// enrolled in. Returns the enrollment with it, for the command to mark
    /// this machine.
    pub(crate) fn resolve_target(
        &self,
        api: &mut PlatformApi,
        workspace_flag: Option<&str>,
        project_flag: Option<&str>,
    ) -> Result<(select::Target, Option<auth::Enrollment>)> {
        let enrollment = self.enrollment()?;
        let selection = self.selection()?;
        let target = select::resolve_target(
            api,
            workspace_flag,
            project_flag,
            selection.as_ref(),
            enrollment.as_ref().map(|e| &e.document),
        )?;
        Ok((target, enrollment))
    }
}

/// Rejects `--core-node` for the whole `platform` group.
///
/// `--core-node` redirects a command at another machine's daemon, and no
/// command in this group addresses a daemon that way: `enroll` and `unenroll`
/// poke the *local* daemon over its control socket, and the rest talk only to
/// the platform. Accepting the flag and ignoring it would silently answer a
/// different question than the one asked, so the whole group refuses it rather
/// than each command deciding for itself.
fn reject_core_node_override(ctx: &AppContext) -> Result<()> {
    let Some(core_node) = ctx.core_node_override() else {
        return Ok(());
    };
    Err(Error::ExecutionFailed(format!(
        "`--core-node {core_node}` is not valid for `peppy platform` commands: they act on this \
         machine's daemon and on your platform account, never on another core node. \
         Run `peppy stack list` to see other core nodes in your project."
    )))
}

/// Confirms (before any platform call) an enroll/unenroll that restarts the
/// daemon and wipes the running node stack, unless `--yes` was passed. Callers
/// skip this entirely for `zenoh.external`, where the command never pokes or
/// restarts the daemon. Returns `Ok(true)` to proceed. Only prompts when a
/// daemon is actually running (else there is nothing to restart), stdin is a
/// TTY (so a script is never blocked on a prompt, and never waits on the probe
/// of the node stack), and the daemon is running at least one user node (else
/// the restart wipes nothing worth warning about).
pub(crate) fn confirm_restart(
    ctx: &Arc<AppContext>,
    yes: bool,
    action: &FederationPokeAction,
    daemon_state: Option<&DaemonState>,
) -> Result<bool> {
    use std::io::IsTerminal;

    if yes || !std::io::stdin().is_terminal() {
        return Ok(true);
    }
    // A readable state file can outlive a crashed daemon, so probe the recorded
    // pid for real liveness rather than treating readability as "a daemon is up".
    if !daemon_state.is_some_and(DaemonState::is_running) {
        return Ok(true);
    }
    // The restart only wipes a node stack worth warning about when the daemon is
    // actually running user nodes. A stack that holds nothing but the synthetic
    // core-node root loses nothing on restart, so the warning would be noise.
    if !daemon_has_user_nodes(ctx) {
        return Ok(true);
    }
    let verb = match action {
        FederationPokeAction::Enroll => "Enrolling",
        FederationPokeAction::Unenroll => "Unenrolling",
    };
    ask_to_continue(&format!(
        "{verb} changes this machine's router identity and namespace, which restarts the \
         messaging daemon and wipes the running node stack."
    ))
}

/// Prints `warning` and asks the person to continue. Returns `Ok(true)` to
/// proceed. With no terminal on stdin there is no person to ask, so the answer
/// is yes: a script is never blocked on a prompt.
pub(crate) fn ask_to_continue(warning: &str) -> Result<bool> {
    use std::io::IsTerminal;

    if !std::io::stdin().is_terminal() {
        return Ok(true);
    }
    crate::commands::confirm::confirm_prompt(&format!("{warning}\nContinue? [y/N] "), None)
}

/// Whether the running daemon's node stack holds any user node, by querying its
/// live stack over the messaging session (the same query `peppy stack list`
/// uses). Drives the restart prompt: an empty stack means the restart wipes
/// nothing the user staged, so the warning is skipped.
///
/// Best effort: connecting to the daemon and reading its stack can fail or stall
/// (it is mid-restart, its messaging router is not up yet, the query times out).
/// Any such outcome returns `true` so the caller still shows the warning rather
/// than silently dropping it. The whole probe is bounded by
/// [`STACK_PROBE_TIMEOUT`] because opening the session can itself stall when the
/// router is unreachable, which the per-query timeout alone would not cover.
fn daemon_has_user_nodes(ctx: &Arc<AppContext>) -> bool {
    let probe = async {
        let conn = ctx.connect_to_daemon().await?;
        // Deliberately targets the *local* daemon (not `conn.target_core_node`):
        // this probe backs the "this restarts the local daemon" warning, so a
        // global `--core-node` override must not redirect it.
        let response = poll(
            &StackListRequest::new(),
            conn.messenger,
            &conn.core_node_name,
            CALLER_INSTANCE_ID,
            &conn.core_node_name,
            STACK_PROBE_TIMEOUT,
        )
        .await?;
        let graph = crate::commands::parse_stack_graph(&response.graph_json)?;
        Ok::<bool, Error>(stack_has_user_nodes(&graph))
    };

    crate::commands::block_on(async move {
        Ok(
            match tokio::time::timeout(STACK_PROBE_TIMEOUT, probe).await {
                Ok(Ok(has_user_nodes)) => has_user_nodes,
                Ok(Err(_)) | Err(_) => true,
            },
        )
    })
    .unwrap_or(true)
}

/// Whether a serialized stack graph contains a user node, i.e. any node other
/// than the synthetic [`NodeStage::Root`] entity the daemon always carries for
/// itself. A node entity counts as present regardless of its instances' states,
/// since a node whose only instances have finished is still in the stack and
/// would be wiped by a restart. Pure over the graph so the decision is
/// unit-testable without a live daemon.
fn stack_has_user_nodes(graph: &SerializedNodeGraph) -> bool {
    graph.nodes.iter().any(|node| node.stage != NodeStage::Root)
}

/// After the enrollment changed, poke the running daemon over its control
/// socket so it re-reads the enrollment *immediately*, and report the result.
///
/// The socket path is derived from the same `dirs` the command used (so a test
/// seam isolates it). An identity change makes the daemon restart its whole
/// generation: the first ack is `Restarting`, and [`await_restart`] polls the
/// (path-stable) control socket until the daemon is back under the identity we
/// just wrote, then reports the settled outcome.
///
/// For [`FederationPokeAction::Enroll`] this is **strict**: if the link cannot
/// be verified, it returns an actionable [`Error::Auth`]. The caller has already
/// persisted the enrollment, so the daemon joins on its next start; only the
/// command exits non-zero. For [`FederationPokeAction::Unenroll`] it is
/// best-effort and never returns `Err`.
pub(crate) fn poke_federation_and_report(
    dirs: &PeppyDirs,
    action: FederationPokeAction,
) -> Result<()> {
    let socket = daemon_control::federation_control_socket_path(dirs);
    let read_timeout = daemon_control::POKE_READ_TIMEOUT;
    let spinner = crate::terminal::spinner("Waiting for the daemon to apply the enrollment");
    let outcome = daemon_control::poke_refederate(&socket, read_timeout);
    if let Some(pb) = spinner {
        pb.finish_and_clear();
    }
    if matches!(outcome, PokeOutcome::Restarting) {
        return await_restart(dirs, &socket, read_timeout, &action);
    }
    report(outcome, &action)
}

fn report(outcome: PokeOutcome, action: &FederationPokeAction) -> Result<()> {
    match action {
        FederationPokeAction::Enroll => report_enroll(outcome),
        FederationPokeAction::Unenroll => {
            report_unenroll(outcome);
            Ok(())
        }
    }
}

/// Polls until the daemon is back under the identity the enrollment now
/// prescribes, then reports the settled federation outcome. Detects a
/// concurrent enrollment change (a second enroll/unenroll mid-restart) and a
/// never-recovers timeout. Bounded by [`RESTART_POLL_DEADLINE`].
fn await_restart(
    dirs: &PeppyDirs,
    socket: &std::path::Path,
    read_timeout: Duration,
    action: &FederationPokeAction,
) -> Result<()> {
    let expected = FederationIdentity::on_disk(dirs)?;
    let subcommand = action.subcommand();
    let spinner =
        crate::terminal::spinner("Waiting for the daemon to restart under the new identity");
    let deadline = Instant::now() + RESTART_POLL_DEADLINE;
    let result = loop {
        if Instant::now() >= deadline {
            break Err(Error::Auth(format!(
                "the daemon did not come back under namespace `{}` within the timeout; \
                 check the `peppy service serve` logs and re-run `peppy platform {subcommand}`",
                expected.namespace
            )));
        }
        std::thread::sleep(RESTART_POLL_INTERVAL);

        // A concurrent enroll/unenroll rewrote the enrollment mid-restart, so the
        // daemon will not come back under what we wrote.
        if FederationIdentity::on_disk(dirs)? != expected {
            break Err(Error::Auth(format!(
                "the enrollment changed during the restart; re-run `peppy platform {subcommand}`"
            )));
        }

        // The (path-stable) daemon state records the live generation's identity,
        // written before the control socket binds. While the daemon is down or
        // the old generation is still up, this is unreadable or carries the old
        // value.
        let back = matches!(
            DaemonState::read_from(&DaemonState::state_file_in(dirs.root())),
            Ok(state) if state.runs_under(&expected)
        );
        if !back {
            continue;
        }

        // Back under the expected identity. Confirm the settled federation state
        // with a fresh poke (which now finds the identity unchanged and verifies
        // the link).
        match daemon_control::poke_refederate(socket, read_timeout) {
            // Still settling: the new generation wrote its state (so we got here)
            // but its control socket may not have bound yet, so a poke can
            // transiently find no socket or time out. Keep polling until it
            // actually answers (or the deadline above fires).
            PokeOutcome::Restarting | PokeOutcome::DaemonNotRunning | PokeOutcome::TimedOut => {
                continue;
            }
            other => break report(other, action),
        }
    };
    if let Some(pb) = spinner {
        pb.finish_and_clear();
    }
    result
}

/// Strict reporting for an enroll poke: a verified link and an operator-pinned
/// router print and return `Ok`; a daemon that is not running is a note (the
/// enrollment is on disk and the daemon joins on its next start); every other
/// outcome returns an actionable [`Error::Auth`] (the enrollment is kept).
fn report_enroll(outcome: PokeOutcome) -> Result<()> {
    match outcome {
        PokeOutcome::Applied(Some(locator)) => {
            println!("Router link verified ({locator}).");
            Ok(())
        }
        PokeOutcome::Pinned => {
            println!("{PINNED_NOTE}");
            Ok(())
        }
        PokeOutcome::DaemonNotRunning => {
            println!(
                "No running peppy daemon was found; it joins the project router when it starts \
                 (`peppy service serve`)."
            );
            Ok(())
        }
        PokeOutcome::Unreachable(reason) => Err(Error::Auth(format!(
            "enrolled, but the daemon could not establish the mutual-TLS link to the project \
             router: {reason}. The enrollment is kept and the router keeps retrying; run \
             `peppy platform status` to check the link again."
        ))),
        PokeOutcome::DaemonError(msg) => Err(Error::Auth(format!(
            "enrolled, but the daemon could not apply the enrollment: {msg}. Check the \
             `peppy service serve` logs and run `peppy platform status`."
        ))),
        PokeOutcome::TimedOut => Err(Error::Auth(
            "enrolled, but the daemon did not answer within the timeout. Check the \
             `peppy service serve` logs and run `peppy platform status`."
                .to_string(),
        )),
        PokeOutcome::Applied(None) => Err(Error::Auth(
            "enrolled, but the daemon reports no enrollment. Check that it runs under the same \
             PEPPY_HOME, then run `peppy platform status`."
                .to_string(),
        )),
        // `Restarting` is intercepted by `poke_federation_and_report` (it drives
        // the restart poll), so it should not reach here; treat defensively.
        PokeOutcome::Restarting => Err(Error::Auth(
            "the daemon is restarting to apply the enrollment; run `peppy platform status` \
             once it is back."
                .to_string(),
        )),
    }
}

/// Best-effort reporting for an unenroll poke: print a one-line status and
/// never fail. A daemon that did not get the poke boots standalone next time.
fn report_unenroll(outcome: PokeOutcome) {
    match outcome {
        PokeOutcome::Applied(None) => println!("The daemon runs standalone again."),
        PokeOutcome::Applied(Some(locator)) => {
            println!("Note: the daemon still reports a project link ({locator}).")
        }
        PokeOutcome::Pinned => println!("{PINNED_NOTE}"),
        PokeOutcome::Unreachable(msg) | PokeOutcome::DaemonError(msg) => {
            println!(
                "Note: the daemon could not apply the change now ({msg}); it will on its next start."
            )
        }
        PokeOutcome::DaemonNotRunning => {
            println!("No running daemon; it starts standalone next time.")
        }
        PokeOutcome::TimedOut => {
            println!(
                "The daemon did not answer within the timeout; it applies the change on its next start."
            )
        }
        PokeOutcome::Restarting => {
            println!("The daemon is restarting under the local namespace.")
        }
    }
}

/// A time as a calendar date for human output.
pub(crate) fn date(time: &chrono::DateTime<chrono::Utc>) -> String {
    time.format("%Y-%m-%d").to_string()
}

/// A unix timestamp as a calendar date for human output, or the number itself
/// when it is not a date.
pub(crate) fn date_of(unix: i64) -> String {
    chrono::DateTime::from_timestamp(unix, 0)
        .map(|time| date(&time))
        .unwrap_or_else(|| unix.to_string())
}

#[derive(Subcommand)]
pub enum PlatformCommands {
    /// Sign in to the platform via the browser (OAuth device flow), then enroll this machine in a project
    Login {
        /// Print the verification URL/code instead of opening a browser.
        #[arg(long = "no-browser")]
        no_browser: bool,
        /// The workspace of the project to enroll in, by id or exact name (else the only one, else you select it from a menu).
        #[arg(long, conflicts_with = "no_enroll")]
        workspace: Option<String>,
        /// The project to enroll in, by id or exact name (else the only one, else you select it from a menu).
        #[arg(long, conflicts_with = "no_enroll")]
        project: Option<String>,
        /// Sign in only; do not enroll this machine.
        #[arg(long = "no-enroll")]
        no_enroll: bool,
        /// Skip the "this restarts the daemon and wipes the node stack" prompt of the enrollment.
        #[arg(long = "yes", short = 'y', conflicts_with = "no_enroll")]
        yes: bool,
    },
    /// Sign out: revoke the session's tokens and clear them locally (the enrollment stays)
    Logout,
    /// Show the signed-in identity, backend, and token status
    Whoami {
        /// Emit machine-readable JSON.
        #[arg(long)]
        json: bool,
    },
    /// Show, list, select or clear the workspace the platform commands act on
    Workspace {
        #[command(subcommand)]
        command: workspace::WorkspaceCommands,
    },
    /// Show, list, select or clear the project the platform commands act on
    Project {
        #[command(subcommand)]
        command: project::ProjectCommands,
    },
    /// Enroll this machine, under the core-node name of its running daemon, as a peer of a project's cloud router
    Enroll {
        /// The workspace, by id or exact name (else the selected one, else the only one).
        #[arg(long)]
        workspace: Option<String>,
        /// The project, by id or exact name (else the selected one, else the workspace's only project).
        #[arg(long)]
        project: Option<String>,
        /// Replace an existing enrollment: enroll anew, then remove the old peer.
        #[arg(long)]
        replace: bool,
        /// Skip the "this restarts the daemon and wipes the node stack" prompt.
        #[arg(long = "yes", short = 'y')]
        yes: bool,
    },
    /// Remove this machine from its project's cloud router
    Unenroll {
        /// Only delete the local enrollment; do not remove the peer on the platform.
        #[arg(long = "local-only")]
        local_only: bool,
        /// Skip the "this restarts the daemon and wipes the node stack" prompt.
        #[arg(long = "yes", short = 'y')]
        yes: bool,
    },
    /// Show this machine's enrollment and the state of its router link
    Status {
        /// Emit machine-readable JSON.
        #[arg(long)]
        json: bool,
    },
    /// List the peers of a project's cloud router (the selected project by default)
    Peers {
        /// The workspace, by id or exact name.
        #[arg(long)]
        workspace: Option<String>,
        /// The project, by id or exact name.
        #[arg(long)]
        project: Option<String>,
        /// Emit machine-readable JSON.
        #[arg(long)]
        json: bool,
    },
    /// Restart or start a project's cloud router (the selected project by default)
    Router {
        #[command(subcommand)]
        command: router::RouterCommands,
    },
}

pub struct PlatformCommand {
    /// The `--api-url` of the group, accepted before and after the subcommand.
    pub api_url: Option<String>,
    pub command: PlatformCommands,
}

impl Command for PlatformCommand {
    fn execute(self, app_ctx: &Arc<AppContext>) -> Result<()> {
        // Refused for the whole group, before any command does work: see
        // `reject_core_node_override`.
        reject_core_node_override(app_ctx)?;
        let api_url = self.api_url;
        match self.command {
            PlatformCommands::Login {
                no_browser,
                workspace,
                project,
                no_enroll,
                yes,
            } => login::LoginCommand {
                api_url,
                no_browser,
                workspace,
                project,
                no_enroll,
                yes,
                ask: select::Ask::Terminal,
                peppy_dirs: None,
            }
            .execute(app_ctx),
            PlatformCommands::Logout => logout::LogoutCommand {
                api_url,
                peppy_dirs: None,
            }
            .execute(app_ctx),
            PlatformCommands::Whoami { json } => whoami::WhoamiCommand {
                api_url,
                json,
                peppy_dirs: None,
            }
            .execute(app_ctx),
            PlatformCommands::Workspace { command } => workspace::WorkspaceCommand {
                command,
                api_url,
                ask: select::Ask::Terminal,
                peppy_dirs: None,
            }
            .execute(app_ctx),
            PlatformCommands::Project { command } => project::ProjectCommand {
                command,
                api_url,
                ask: select::Ask::Terminal,
                peppy_dirs: None,
            }
            .execute(app_ctx),
            PlatformCommands::Enroll {
                workspace,
                project,
                replace,
                yes,
            } => enroll::EnrollCommand {
                api_url,
                workspace,
                project,
                replace,
                yes,
                peppy_dirs: None,
            }
            .execute(app_ctx),
            PlatformCommands::Unenroll { local_only, yes } => unenroll::UnenrollCommand {
                api_url,
                local_only,
                yes,
                peppy_dirs: None,
            }
            .execute(app_ctx),
            PlatformCommands::Status { json } => status::StatusCommand {
                api_url,
                json,
                peppy_dirs: None,
            }
            .execute(app_ctx),
            PlatformCommands::Peers {
                workspace,
                project,
                json,
            } => peers::PeersCommand {
                api_url,
                workspace,
                project,
                json,
                peppy_dirs: None,
            }
            .execute(app_ctx),
            PlatformCommands::Router { command } => router::RouterCommand {
                api_url,
                ..router::RouterCommand::from(command)
            }
            .execute(app_ctx),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        federation_is_managed, read_daemon_state, report_enroll, report_unenroll,
        stack_has_user_nodes,
    };
    use core_node_api::{
        InstanceState, NodeStage, SerializedInstance, SerializedNode, SerializedNodeGraph,
    };
    use daemon::control::PokeOutcome;
    use daemon::state::DaemonState;
    use daemon_config::consts::PeppyDirs;
    use daemon_config::peppy_config::{
        ExternalZenohConfig, ManagedZenohConfig, PeppyConfig, ZenohConfig,
    };
    use std::collections::BTreeMap;

    fn managed_config() -> PeppyConfig {
        PeppyConfig {
            zenoh: ZenohConfig::Managed(ManagedZenohConfig::default()),
            ..PeppyConfig::default()
        }
    }

    fn external_config() -> PeppyConfig {
        PeppyConfig {
            zenoh: ZenohConfig::External(ExternalZenohConfig {
                endpoint: "tcp/router.example:7447".to_string(),
            }),
            ..PeppyConfig::default()
        }
    }

    fn state(router_id: Option<&str>) -> DaemonState {
        DaemonState::new(
            "cn-test",
            "127.0.0.1",
            7447,
            "test",
            5,
            config::namespace::Namespace::local(),
            router_id.map(|id| pmi::RouterId::parse(id).unwrap()),
        )
    }

    /// Writes a daemon state file under `dirs` whose recorded pid is this test
    /// process (so `is_running` holds): a managed router when `router_id`
    /// names one, an operator-run router otherwise.
    fn write_running_state(dirs: &PeppyDirs, router_id: Option<&str>) {
        DaemonState::write_to(&DaemonState::state_file_in(dirs.root()), &state(router_id))
            .expect("write daemon state");
    }

    #[test]
    fn with_no_daemon_running_the_disk_config_decides() {
        let dir = tempfile::tempdir().expect("tempdir");
        let dirs = PeppyDirs::new(dir.path());
        assert!(
            federation_is_managed(read_daemon_state(&dirs).as_ref(), &managed_config()),
            "no state file: a managed disk config means a poke"
        );
        assert!(
            !federation_is_managed(read_daemon_state(&dirs).as_ref(), &external_config()),
            "no state file: an external disk config means no poke"
        );
    }

    #[test]
    fn a_running_daemon_beats_the_disk_config() {
        let dir = tempfile::tempdir().expect("tempdir");
        let dirs = PeppyDirs::new(dir.path());

        // Managed daemon, external config on disk: the poke must still happen.
        write_running_state(&dirs, Some("7f3a"));
        assert!(
            federation_is_managed(read_daemon_state(&dirs).as_ref(), &external_config()),
            "a running managed daemon must be poked even if the disk config went external"
        );

        // External daemon, managed config on disk: there is no control socket.
        write_running_state(&dirs, None);
        assert!(
            !federation_is_managed(read_daemon_state(&dirs).as_ref(), &managed_config()),
            "a running external daemon has no control socket to poke"
        );
    }

    #[test]
    fn a_stale_state_file_from_a_dead_daemon_falls_back_to_the_disk_config() {
        let dir = tempfile::tempdir().expect("tempdir");
        let dirs = PeppyDirs::new(dir.path());
        let mut stale = state(None);
        // A pid outside the valid range names no live process, so the state is
        // stale and the disk config decides again.
        stale.daemon_pid = Some(u32::MAX);
        DaemonState::write_to(&DaemonState::state_file_in(dirs.root()), &stale)
            .expect("write daemon state");

        assert!(
            federation_is_managed(read_daemon_state(&dirs).as_ref(), &managed_config()),
            "a dead daemon's state must not override the disk config"
        );
    }

    /// Builds an instance-less node fixed at `stage`. The bindings/instances are
    /// irrelevant to the user-node predicate, which keys only on `stage`.
    fn node_with_stage(name: &str, stage: NodeStage) -> SerializedNode {
        SerializedNode {
            name: name.to_string(),
            tag: "v1".to_string(),
            core_node: "test-core".to_string(),
            config_path: format!("/tmp/{name}.json5"),
            artifact_path: None,
            stage,
            instances: Vec::new(),
        }
    }

    fn graph_of(nodes: Vec<SerializedNode>) -> SerializedNodeGraph {
        SerializedNodeGraph {
            nodes,
            edges: Vec::new(),
        }
    }

    #[test]
    fn a_stack_with_only_the_core_root_has_no_user_nodes() {
        let graph = graph_of(vec![node_with_stage("core", NodeStage::Root)]);
        assert!(
            !stack_has_user_nodes(&graph),
            "a daemon carrying only its synthetic root is an empty stack"
        );
    }

    #[test]
    fn a_stack_with_a_user_node_alongside_the_root_has_user_nodes() {
        let graph = graph_of(vec![
            node_with_stage("core", NodeStage::Root),
            node_with_stage("sensor", NodeStage::Added),
        ]);
        assert!(stack_has_user_nodes(&graph));
    }

    #[test]
    fn an_empty_graph_has_no_user_nodes() {
        assert!(!stack_has_user_nodes(&graph_of(Vec::new())));
    }

    #[test]
    fn a_user_node_with_only_terminal_instances_still_counts() {
        // "Empty" is about node entities present in the stack, not running
        // instances: a node whose only instance has finished is still in the
        // stack and would be wiped by a restart, so it must keep the warning.
        let mut recorder = node_with_stage("recorder", NodeStage::Ready);
        recorder.instances = vec![SerializedInstance {
            clock: Default::default(),
            instance_id: "rec-1".to_string(),
            state: InstanceState::Finished,
            healthy: true,
            slot_bindings: BTreeMap::new(),
            pairing_slots: BTreeMap::new(),
            endpoints: Vec::new(),
        }];
        let graph = graph_of(vec![node_with_stage("core", NodeStage::Root), recorder]);
        assert!(stack_has_user_nodes(&graph));
    }

    #[test]
    fn enroll_is_ok_for_a_verified_link_a_pinned_router_and_no_daemon() {
        for outcome in [
            PokeOutcome::Applied(Some("tls/rtr:7447".to_string())),
            PokeOutcome::Pinned,
            PokeOutcome::DaemonNotRunning,
        ] {
            assert!(report_enroll(outcome).is_ok());
        }
    }

    #[test]
    fn enroll_fails_strictly_for_every_not_in_effect_outcome() {
        for outcome in [
            PokeOutcome::Applied(None),
            PokeOutcome::Unreachable("UnknownCA".to_string()),
            PokeOutcome::DaemonError("boom".to_string()),
            PokeOutcome::TimedOut,
        ] {
            assert!(
                report_enroll(outcome).is_err(),
                "enroll must fail when the link is not verified"
            );
        }
    }

    #[test]
    fn unreachable_enroll_error_carries_the_reason_and_keeps_the_enrollment() {
        let err = report_enroll(PokeOutcome::Unreachable(
            "received fatal alert: UnknownCA".to_string(),
        ))
        .expect_err("an unreachable router fails enroll");
        let msg = err.to_string();
        assert!(
            msg.contains("UnknownCA"),
            "the probe reason is surfaced: {msg}"
        );
        assert!(msg.contains("enrollment is kept"), "{msg}");
        assert!(msg.contains("peppy platform status"), "{msg}");
    }

    #[test]
    fn unenroll_is_always_best_effort() {
        for outcome in [
            PokeOutcome::Applied(None),
            PokeOutcome::Applied(Some("tls/rtr:7447".to_string())),
            PokeOutcome::Pinned,
            PokeOutcome::Unreachable("x".to_string()),
            PokeOutcome::DaemonError("y".to_string()),
            PokeOutcome::DaemonNotRunning,
            PokeOutcome::TimedOut,
            PokeOutcome::Restarting,
        ] {
            report_unenroll(outcome);
        }
    }
}
