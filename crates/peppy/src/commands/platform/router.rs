//! `peppy platform router restart` and `start`: act on a project's cloud
//! router. One project has one router, so the project names it: the
//! `--workspace`/`--project` flags when they are given, else the project of
//! the context, else the project this machine is enrolled in. The command
//! prints the router it found before it acts.
//!
//! A restart applies every pending change. The router reads its trust anchors
//! when it starts, so a removed peer is refused, and its slot is free, only
//! from the next restart. A restart drops each peer link for a moment; the
//! peers connect again on their own.

use std::sync::Arc;

use clap::Subcommand;
use daemon_config::consts::PeppyDirs;

use crate::commands::Command;
use crate::commands::platform::{PlatformSession, ask_to_continue, select};
use crate::context::AppContext;
use crate::error::{Error, Result};
use auth::AuthError;
use auth::client::{self, RouterStatus};

/// The peer status the platform gives a removed peer until the router restarts.
const PENDING_RESTART: &str = "pending_restart";

#[derive(Subcommand)]
pub enum RouterCommands {
    /// Restart the router, which applies every pending change
    Restart {
        #[arg(long = "api-url")]
        api_url: Option<String>,
        /// The workspace, by id or exact name.
        #[arg(long)]
        workspace: Option<String>,
        /// The project, by id or exact name (else the project of the context, else of the enrollment).
        #[arg(long)]
        project: Option<String>,
        /// Skip the "this drops each peer link for a moment" prompt.
        #[arg(long = "yes", short = 'y')]
        yes: bool,
    },
    /// Start a stopped router
    Start {
        #[arg(long = "api-url")]
        api_url: Option<String>,
        /// The workspace, by id or exact name.
        #[arg(long)]
        workspace: Option<String>,
        /// The project, by id or exact name (else the project of the context, else of the enrollment).
        #[arg(long)]
        project: Option<String>,
    },
}

/// What the command asks the platform to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RouterAction {
    Restart,
    Start,
}

impl RouterAction {
    fn verb(self) -> &'static str {
        match self {
            Self::Restart => "restart",
            Self::Start => "start",
        }
    }
}

pub struct RouterCommand {
    pub action: RouterAction,
    pub api_url: Option<String>,
    pub workspace: Option<String>,
    pub project: Option<String>,
    /// Skip the confirmation prompt of a restart.
    pub yes: bool,
    /// Test seam: override the peppy data dirs.
    pub peppy_dirs: Option<PeppyDirs>,
}

impl From<RouterCommands> for RouterCommand {
    fn from(command: RouterCommands) -> Self {
        match command {
            RouterCommands::Restart {
                api_url,
                workspace,
                project,
                yes,
            } => Self {
                action: RouterAction::Restart,
                api_url,
                workspace,
                project,
                yes,
                peppy_dirs: None,
            },
            RouterCommands::Start {
                api_url,
                workspace,
                project,
            } => Self {
                action: RouterAction::Start,
                api_url,
                workspace,
                project,
                yes: true,
                peppy_dirs: None,
            },
        }
    }
}

impl Command for RouterCommand {
    fn execute(self, _ctx: &Arc<AppContext>) -> Result<()> {
        let session = PlatformSession::resolve(self.peppy_dirs, self.api_url.as_deref())?;
        let mut cred = session.credential()?;
        let enrollment = auth::enrollment::load(&session.dirs).map_err(Error::AuthEngine)?;
        let context = session.context()?;
        let target = select::resolve_target(
            &session.http,
            &session.api_url,
            &mut cred,
            self.workspace.as_deref(),
            self.project.as_deref(),
            context.as_ref(),
            enrollment.as_ref().map(|e| &e.document),
        )?;

        let before = client::router_status(
            &session.http,
            &session.api_url,
            &mut cred,
            &target.workspace_id,
            &target.project_id,
        )
        .map_err(|error| select::refusal_on(&target, error))?;
        print!("{}", describe(&target.label, &before));

        if self.action == RouterAction::Restart
            && !self.yes
            && !ask_to_continue(
                "A restart drops each peer link for a moment. Peers connect again on their own.",
            )?
        {
            println!("Restart aborted.");
            return Ok(());
        }

        let request = match self.action {
            RouterAction::Restart => client::restart_router,
            RouterAction::Start => client::start_router,
        };
        let after = request(
            &session.http,
            &session.api_url,
            &mut cred,
            &target.workspace_id,
            &target.project_id,
        )
        .map_err(|error| refusal(error, self.action, &target))?;
        println!(
            "The platform accepted the {}. The router is {}.",
            self.action.verb(),
            after.phase
        );
        Ok(())
    }
}

/// The error for a refused action. A `403` means the caller may not manage the
/// infrastructure of the project, which is a different permission from the one
/// an enrollment needs, so it gets its own words. The command read the router
/// of `target` just before, so a `403` here is about the action and not about
/// the project.
fn refusal(error: AuthError, action: RouterAction, target: &select::Target) -> Error {
    match error {
        AuthError::Problem(problem) if problem.status == 403 => Error::Auth(format!(
            "you cannot {} this router: it needs the permission to manage the infrastructure \
             of the project. Ask an admin of the workspace.",
            action.verb()
        )),
        other => select::refusal_on(target, other),
    }
}

/// The router a command found, for the person to check before it acts.
fn describe(label: &str, status: &RouterStatus) -> String {
    let waiting = status
        .peers
        .iter()
        .filter(|peer| peer.status == PENDING_RESTART)
        .count();
    let mut out = format!("Router of {label}\n");
    out.push_str(&format!("  phase    : {}\n", status.phase));
    match waiting {
        0 => out.push_str(&format!("  peers    : {}\n", status.peers.len())),
        _ => out.push_str(&format!(
            "  peers    : {} ({waiting} waits for a restart)\n",
            status.peers.len()
        )),
    }
    for line in pending_change_lines(status) {
        out.push_str(&format!("  pending  : {line}\n"));
    }
    out
}

/// One line for each change that waits for a restart, in the platform's own
/// words, oldest first.
pub(crate) fn pending_change_lines(status: &RouterStatus) -> Vec<String> {
    status
        .pending_change_entries
        .iter()
        .map(|change| {
            let staged = change.staged_at.format("%Y-%m-%d");
            match change.deadline {
                Some(deadline) => format!(
                    "{} (staged {staged}, the platform restarts the router on {})",
                    change.description,
                    deadline.format("%Y-%m-%d")
                ),
                None => format!("{} (staged {staged})", change.description),
            }
        })
        .collect()
}

/// The command that restarts the router of a project, spelled with its ids so
/// it also works on a machine that is not enrolled.
pub(crate) fn restart_command(workspace_id: &str, project_id: &str) -> String {
    format!("peppy platform router restart --workspace {workspace_id} --project {project_id}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use auth::client::{PendingChange, RouterPeerStatus};
    use auth::{Problem, ProblemKind};

    fn status(peers: &[&str], pending: Vec<PendingChange>) -> RouterStatus {
        RouterStatus {
            phase: "running".into(),
            desired_state: "running".into(),
            can_manage_infra: true,
            address: None,
            pending_changes: !pending.is_empty(),
            pending_change_entries: pending,
            peers: peers
                .iter()
                .enumerate()
                .map(|(i, status)| RouterPeerStatus {
                    id: format!("peer-{i}"),
                    status: status.to_string(),
                    last_seen_at: None,
                })
                .collect(),
        }
    }

    fn removal() -> PendingChange {
        PendingChange {
            kind: "peer".into(),
            description: "peer robot-7 removed".into(),
            staged_at: "2026-09-28T10:00:00Z".parse().unwrap(),
            deadline: None,
        }
    }

    #[test]
    fn the_description_names_the_router_its_peers_and_what_waits() {
        let out = describe(
            "project Lab (p-1) in workspace Alice's workspace",
            &status(&["connected", "unknown", PENDING_RESTART], vec![removal()]),
        );
        assert_eq!(
            out,
            "Router of project Lab (p-1) in workspace Alice's workspace\n\
             \x20 phase    : running\n\
             \x20 peers    : 3 (1 waits for a restart)\n\
             \x20 pending  : peer robot-7 removed (staged 2026-09-28)\n"
        );
    }

    #[test]
    fn a_router_with_nothing_pending_prints_no_pending_line() {
        let out = describe("project p-1", &status(&["connected"], Vec::new()));
        assert_eq!(
            out,
            "Router of project p-1\n  phase    : running\n  peers    : 1\n"
        );
    }

    #[test]
    fn a_pending_change_with_a_deadline_prints_it() {
        let mut change = removal();
        change.deadline = Some("2026-10-05T00:00:00Z".parse().unwrap());
        assert_eq!(
            pending_change_lines(&status(&[], vec![change])),
            [
                "peer robot-7 removed (staged 2026-09-28, the platform restarts the router on 2026-10-05)"
            ]
        );
    }

    #[test]
    fn a_403_is_worded_as_a_missing_permission() {
        let forbidden = AuthError::Problem(Problem {
            kind: ProblemKind::Other("about:blank".into()),
            status: 403,
            title: "Forbidden".into(),
            detail: None,
            retry_after_secs: None,
        });
        let target = select::Target {
            workspace_id: "ws-1".into(),
            project_id: "p-1".into(),
            label: "project p-1".into(),
            source: select::TargetSource::Context,
        };
        let message = refusal(forbidden, RouterAction::Restart, &target).to_string();
        assert!(message.contains("cannot restart"), "{message}");
        assert!(message.contains("manage the infrastructure"), "{message}");
        assert!(
            !message.contains("out of date"),
            "the router of the context was read just before: {message}"
        );

        let other = refusal(AuthError::Http("boom".into()), RouterAction::Start, &target);
        assert_eq!(other.to_string(), "boom");
    }

    #[test]
    fn the_restart_command_names_the_project_by_its_ids() {
        assert_eq!(
            restart_command("ws-1", "p-1"),
            "peppy platform router restart --workspace ws-1 --project p-1"
        );
    }
}
