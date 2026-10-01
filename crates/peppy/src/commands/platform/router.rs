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
use crate::commands::platform::{PlatformSession, ask_to_continue, date, select};
use crate::context::AppContext;
use crate::error::{Error, Result};
use auth::client::{PeerStatus, PlatformApi, RouterPhase, RouterStatus};
use auth::{AuthError, ProblemKind};

#[derive(Subcommand)]
pub enum RouterCommands {
    /// Restart the router, which applies every pending change
    Restart {
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

/// The command of the terminal: the group's `--api-url` is set by the caller.
impl From<RouterCommands> for RouterCommand {
    fn from(command: RouterCommands) -> Self {
        match command {
            RouterCommands::Restart {
                workspace,
                project,
                yes,
            } => Self {
                action: RouterAction::Restart,
                api_url: None,
                workspace,
                project,
                yes,
                peppy_dirs: None,
            },
            RouterCommands::Start { workspace, project } => Self {
                action: RouterAction::Start,
                api_url: None,
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
        let mut api = session.api()?;
        let (target, _) =
            session.resolve_target(&mut api, self.workspace.as_deref(), self.project.as_deref())?;

        let before = api
            .router_status(&target.workspace_id, &target.project_id)
            .map_err(|error| select::refusal_on(&target, error))?;
        print!("{}", describe(&target.label, &before));

        // A stopped router has nothing to restart. Say so before the question,
        // which is about a router that runs.
        if self.action == RouterAction::Restart && before.phase == RouterPhase::Stopped {
            return Err(stopped_router(&target));
        }
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
            RouterAction::Restart => PlatformApi::restart_router,
            RouterAction::Start => PlatformApi::start_router,
        };
        let after = request(&mut api, &target.workspace_id, &target.project_id)
            .map_err(|error| refusal(error, self.action, &target))?;
        println!(
            "The platform accepted the {}. The router is {}.",
            self.action.verb(),
            after.phase
        );
        if self.action == RouterAction::Start && after.pending_changes {
            println!(
                "A start does not apply the changes that wait. When the router runs, restart \
                 it:\n    {}",
                restart_command(&target.workspace_id, &target.project_id)
            );
        }
        Ok(())
    }
}

/// The error for a refused action. A `403` means the caller may not manage the
/// infrastructure of the project, which is a different permission from the one
/// an enrollment needs, so it gets its own words. The command read the router
/// of `target` just before, so a `403` here is about the action and not about
/// the project. A restart the platform refuses because the router is stopped
/// names the two commands that apply the changes.
fn refusal(error: AuthError, action: RouterAction, target: &select::Target) -> Error {
    match error {
        AuthError::Problem(problem) if problem.kind == ProblemKind::RouterStopped => {
            stopped_router(target)
        }
        AuthError::Problem(problem) if problem.status == 403 => Error::Auth(format!(
            "you cannot {} this router: it needs the permission to manage the infrastructure \
             of the project. Ask an admin of the workspace.",
            action.verb()
        )),
        other => select::refusal_on(target, other),
    }
}

/// The error for a restart of a router that is stopped.
fn stopped_router(target: &select::Target) -> Error {
    Error::Auth(format!(
        "the router is stopped, so there is nothing to restart. {}",
        how_to_apply(
            &RouterPhase::Stopped,
            &target.workspace_id,
            &target.project_id,
        )
    ))
}

/// The router a command found, for the person to check before it acts.
fn describe(label: &str, status: &RouterStatus) -> String {
    let waiting = status
        .peers
        .iter()
        .filter(|peer| peer.status == PeerStatus::PendingRestart)
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
            let staged = date(&change.staged_at);
            match change.deadline {
                Some(deadline) => format!(
                    "{} (staged {staged}, the platform restarts the router on {})",
                    change.description,
                    date(&deadline)
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

/// The command that starts the router of a project, spelled with its ids.
pub(crate) fn start_command(workspace_id: &str, project_id: &str) -> String {
    format!("peppy platform router start --workspace {workspace_id} --project {project_id}")
}

/// How the person applies the changes that wait on a router in `phase`: a
/// restart. A stopped router takes two commands, because the platform refuses
/// the restart of a stopped router and a start alone applies nothing.
pub(crate) fn how_to_apply(phase: &RouterPhase, workspace_id: &str, project_id: &str) -> String {
    let restart = restart_command(workspace_id, project_id);
    if *phase != RouterPhase::Stopped {
        return format!("Restart the router:\n    {restart}");
    }
    format!(
        "Start the router, then restart it. A start alone does not apply the changes that \
         wait:\n    {}\n    {restart}",
        start_command(workspace_id, project_id)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use auth::client::{PendingChange, RouterPeerStatus};
    use auth::test_support::problem;

    fn status(peers: &[PeerStatus], pending: Vec<PendingChange>) -> RouterStatus {
        RouterStatus {
            phase: RouterPhase::Running,
            desired_state: "running".into(),
            address: None,
            pending_changes: !pending.is_empty(),
            pending_change_entries: pending,
            peers: peers
                .iter()
                .enumerate()
                .map(|(i, status)| RouterPeerStatus {
                    id: format!("peer-{i}"),
                    status: status.clone(),
                })
                .collect(),
        }
    }

    fn removal() -> PendingChange {
        PendingChange {
            description: "peer robot-7 removed".into(),
            staged_at: "2026-09-28T10:00:00Z".parse().unwrap(),
            deadline: None,
        }
    }

    #[test]
    fn the_description_names_the_router_its_peers_and_what_waits() {
        let out = describe(
            "project Lab (p-1) in workspace Alice's workspace",
            &status(
                &[
                    PeerStatus::Connected,
                    PeerStatus::Unknown,
                    PeerStatus::PendingRestart,
                ],
                vec![removal()],
            ),
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
        let out = describe("project p-1", &status(&[PeerStatus::Connected], Vec::new()));
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
        let forbidden = AuthError::Problem(problem(ProblemKind::Other("about:blank".into()), 403));
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

    fn context_target() -> select::Target {
        select::Target {
            workspace_id: "ws-1".into(),
            project_id: "p-1".into(),
            label: "project p-1".into(),
            source: select::TargetSource::Context,
        }
    }

    /// The platform refuses the restart of a stopped router. The error gives
    /// the start and then the restart, and says that the start alone applies
    /// nothing.
    #[test]
    fn a_restart_of_a_stopped_router_names_the_start_then_the_restart() {
        let stopped = AuthError::Problem(problem(ProblemKind::RouterStopped, 409));
        assert_eq!(
            refusal(stopped, RouterAction::Restart, &context_target()).to_string(),
            "the router is stopped, so there is nothing to restart. Start the router, then \
             restart it. A start alone does not apply the changes that wait:\n\
             \x20   peppy platform router start --workspace ws-1 --project p-1\n\
             \x20   peppy platform router restart --workspace ws-1 --project p-1"
        );
    }

    #[test]
    fn a_router_that_runs_takes_one_command_to_apply_its_changes() {
        for phase in [
            RouterPhase::Running,
            RouterPhase::Degraded,
            RouterPhase::Provisioning,
            RouterPhase::Restarting,
            RouterPhase::Other("hibernating".into()),
        ] {
            assert_eq!(
                how_to_apply(&phase, "ws-1", "p-1"),
                "Restart the router:\n    peppy platform router restart --workspace ws-1 --project p-1",
                "{phase}"
            );
        }
        assert!(how_to_apply(&RouterPhase::Stopped, "ws-1", "p-1").contains("router start"));
    }

    #[test]
    fn the_restart_command_names_the_project_by_its_ids() {
        assert_eq!(
            restart_command("ws-1", "p-1"),
            "peppy platform router restart --workspace ws-1 --project p-1"
        );
    }
}
