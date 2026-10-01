//! `peppy platform enroll`: join a project's cloud router. Picks the
//! workspace and project, mints this machine's key pair and signing request,
//! exchanges the request for a signed certificate, writes the enrollment
//! bundle, and pokes the daemon so it restarts under the new identity and
//! verifies the mutual-TLS link. The private key never leaves the machine.
//!
//! The daemon must run: the peer takes the name of its core node, and the
//! daemon is what joins the router. `peppy platform login` enrolls through the
//! same [`enroll_machine`].

use std::sync::Arc;

use daemon::state::DaemonState;
use daemon_config::consts::PeppyDirs;

use crate::commands::Command;
use crate::commands::platform::router::restart_command;
use crate::commands::platform::select::{self, ProjectInWorkspace};
use crate::commands::platform::unenroll::removal_report;
use crate::commands::platform::{
    FederationPokeAction, PlatformSession, confirm_restart, date_of, external_router_note, peers,
    poke_federation_and_report,
};
use crate::context::AppContext;
use crate::error::{Error, Result};
use auth::client::{PeerStatus, PlatformApi, RouterPeer};
use auth::csr::{self, PeerName};
use auth::enrollment::{self, Enrollment, EnrollmentBundle, IssuedMaterial};
use auth::selection::project_label;
use auth::{AuthError, Problem, ProblemKind, storage};

pub struct EnrollCommand {
    pub api_url: Option<String>,
    pub workspace: Option<String>,
    pub project: Option<String>,
    /// Replace an existing enrollment: enroll anew, then remove the old peer.
    pub replace: bool,
    /// Skip the daemon-restart confirmation prompt.
    pub yes: bool,
    /// Test seam: override the peppy data dirs.
    pub peppy_dirs: Option<PeppyDirs>,
}

impl Command for EnrollCommand {
    fn execute(self, ctx: &Arc<AppContext>) -> Result<()> {
        let session = PlatformSession::resolve(self.peppy_dirs, self.api_url.as_deref())?;

        let existing = session.enrollment()?;
        if let Some(existing) = &existing
            && !self.replace
        {
            return Err(Error::ExecutionFailed(format!(
                "this machine is already enrolled in project {} as {} (peer {}); pass --replace \
                 to enroll anew, or run `peppy platform unenroll` first",
                existing.document.project_id,
                existing.document.peer_name,
                existing.document.peer_id
            )));
        }

        // The daemon is found before the session is resolved, so a machine
        // that cannot enroll refreshes no token.
        let daemon = session.running_daemon().ok_or_else(|| {
            Error::ExecutionFailed(format!(
                "no peppy daemon is running. {DAEMON_NEEDED} {START_THE_DAEMON}, then run `peppy \
                 platform enroll` again."
            ))
        })?;
        let mut api = session.api()?;
        let selection = session.selection()?;
        let target = select::resolve_project(
            &mut api,
            self.workspace.as_deref(),
            self.project.as_deref(),
            selection.as_ref(),
            &mut select::Ask::Never,
        )?;
        enroll_machine(
            ctx, &session, &mut api, &daemon, &target, existing, self.yes,
        )
    }
}

/// Why an enrollment needs the daemon, for the person who has none running.
pub(crate) const DAEMON_NEEDED: &str = "The enrollment needs it: this machine enrolls under the \
     name of its core node, and the daemon is what joins the project router.";

/// How the person starts the daemon.
pub(crate) const START_THE_DAEMON: &str = "Start it with `peppy service serve` (or run it in the \
     background with `peppy service install`)";

/// Enrolls this machine in the project of `target` under the core-node name
/// of the running `daemon`, writes the bundle, removes the peer of the
/// `existing` enrollment it replaces, and pokes the daemon so it restarts
/// under the new identity. The private key never leaves the machine.
pub(crate) fn enroll_machine(
    ctx: &Arc<AppContext>,
    session: &PlatformSession,
    api: &mut PlatformApi,
    daemon: &DaemonState,
    target: &ProjectInWorkspace,
    existing: Option<Enrollment>,
    yes: bool,
) -> Result<()> {
    let name = PeerName::parse(&daemon.core_node_name)?;
    let managed = daemon.has_federation_control();

    // Confirm before anything is minted, so an aborted enrollment leaves no
    // peer behind on the platform.
    if managed && !confirm_restart(ctx, yes, &FederationPokeAction::Enroll, Some(daemon))? {
        println!("Enrollment aborted.");
        return Ok(());
    }

    let ProjectInWorkspace { workspace, project } = target;
    println!(
        "Enrolling {name} in {}.",
        project_label(&project.name, &project.id, &workspace.name)
    );
    let identity = csr::generate_peer_identity(&name)?;
    let spinner = crate::terminal::spinner("Waiting for the platform to sign the certificate");
    let answer = api.enroll_peer(&workspace.id, &project.id, &identity.csr_pem);
    if let Some(pb) = spinner {
        pb.finish_and_clear();
    }
    let enrolled = answer.map_err(|error| explain_refusal(api, target, error))?;
    let bundle = EnrollmentBundle {
        peer_key_pem: identity.private_key_pem,
        issued: IssuedMaterial::for_enrollment(
            &session.api_url,
            &workspace.id,
            &project.id,
            enrolled,
            storage::now_unix(),
        )?,
    };
    enrollment::save(&session.dirs, &bundle)?;
    println!(
        "The platform signed the peer certificate. It expires on {}, and the daemon renews it \
         from {}, while this machine has a session.",
        date_of(bundle.issued.certificate.not_after),
        date_of(bundle.issued.certificate.renewal_due_at())
    );

    // The new enrollment is on disk, so the old peer is now surplus on the
    // platform. Best effort: a failure here leaves a stale peer to remove from
    // the web app, never a machine without an enrollment.
    if let Some(old) = existing {
        match api.remove_peer(
            &old.document.workspace_id,
            &old.document.project_id,
            &old.document.peer_id,
        ) {
            Ok(removal) => print!("{}", removal_report(&old.document, &removal)),
            Err(e) => println!(
                "Warning: could not remove the previous peer {} ({e}); remove it in the web app.",
                old.document.peer_id
            ),
        }
    }

    if !managed {
        println!("{}", external_router_note(&session.dirs));
        return Ok(());
    }
    poke_federation_and_report(&session.dirs, FederationPokeAction::Enroll)
}

/// Puts a refused enrollment into words the person can act on. The refusals
/// with a remedy in the CLI are matched on the kind of the problem; every
/// other refusal prints as the platform sent it.
fn explain_refusal(api: &mut PlatformApi, target: &ProjectInWorkspace, error: AuthError) -> Error {
    let AuthError::Problem(problem) = &error else {
        return Error::AuthEngine(error);
    };
    match problem.kind {
        ProblemKind::PeerLimitReached => {
            // The peers are read only when the platform does not say how many
            // slots a restart frees. Best effort: the refusal is the answer,
            // the list only explains it.
            let peers = match problem.pending_removals {
                Some(_) => None,
                None => api
                    .list_peers(&target.workspace.id, &target.project.id)
                    .ok(),
            };
            Error::Auth(full_router_message(
                problem,
                peers.as_deref(),
                &target.workspace.id,
                &target.project.id,
            ))
        }
        ProblemKind::ProvisionerUnavailable => Error::Auth(try_again_message(problem)),
        _ => Error::AuthEngine(error),
    }
}

/// The words for a router that has no slot a restart can free: a peer has to
/// go first.
const REMOVE_A_PEER_FIRST: &str = "Remove a peer (`peppy platform unenroll` on its machine), \
     then restart the router.";

/// The message for a router that admits no more peers: the refusal and the
/// remedy. A removed peer keeps its slot until the router restarts, so the
/// remedy depends on how many slots a restart frees:
///
/// * the platform says a number above zero: restart the router;
/// * the platform says zero: a restart does not help, remove a peer first;
/// * the platform does not say: `peers` lists the peers, and the ones that
///   read `pending_restart` are the slots a restart frees.
fn full_router_message(
    problem: &Problem,
    peers: Option<&[RouterPeer]>,
    workspace_id: &str,
    project_id: &str,
) -> String {
    let restart = restart_command(workspace_id, project_id);
    match (problem.pending_removals, peers) {
        (Some(0), _) => {
            format!("{problem}\n\nA restart of the router frees no slot. {REMOVE_A_PEER_FIRST}")
        }
        (Some(freed), _) => {
            format!("{problem}\n\nA restart of the router frees {freed} slot(s):\n    {restart}")
        }
        (None, None) => problem.to_string(),
        (None, Some(peers)) => {
            let listing = peers::render_human(project_id, peers, None);
            let waiting = peers
                .iter()
                .filter(|peer| peer.status == PeerStatus::PendingRestart)
                .count();
            match waiting {
                0 => format!("{problem}\n\n{listing}\nEach slot is in use. {REMOVE_A_PEER_FIRST}"),
                _ => format!(
                    "{problem}\n\n{listing}\n{waiting} removed peer(s) keep a slot until the \
                     router restarts. To free the slot:\n    {restart}"
                ),
            }
        }
    }
}

/// The message for a certificate the platform could not sign in time, with the
/// delay it asked for. The enrollment is not sent again by the CLI: the call is
/// not idempotent.
fn try_again_message(problem: &Problem) -> String {
    match problem.retry_after_secs {
        Some(secs) => format!("{problem}. Run the command again in {secs} seconds."),
        None => format!("{problem}. Run the command again in a moment."),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use auth::test_support::router_peer as peer;

    fn problem(kind: ProblemKind, retry_after_secs: Option<u64>) -> Problem {
        Problem {
            title: "Peer limit reached".into(),
            retry_after_secs,
            ..auth::test_support::problem(kind, 422)
        }
    }

    #[test]
    fn a_full_router_with_a_removed_peer_names_the_restart() {
        let message = full_router_message(
            &problem(ProblemKind::PeerLimitReached, None),
            Some(&[
                peer("peer-1", "arm", PeerStatus::Connected),
                peer("peer-2", "bench", PeerStatus::PendingRestart),
            ]),
            "ws-1",
            "p-1",
        );
        assert!(
            message.starts_with("Peer limit reached\n\nProject p-1\n"),
            "{message}"
        );
        assert!(message.contains("bench"), "{message}");
        assert!(
            message.ends_with(
                "1 removed peer(s) keep a slot until the router restarts. To free the slot:\n    \
                 peppy platform router restart --workspace ws-1 --project p-1"
            ),
            "{message}"
        );
    }

    #[test]
    fn a_full_router_with_no_removed_peer_asks_for_a_removal() {
        let message = full_router_message(
            &problem(ProblemKind::PeerLimitReached, None),
            Some(&[peer("peer-1", "arm", PeerStatus::Connected)]),
            "ws-1",
            "p-1",
        );
        assert!(message.contains("Each slot is in use"), "{message}");
        assert!(!message.contains("router restart --workspace"), "{message}");
    }

    /// When the platform says how many slots a restart frees, the CLI takes
    /// its word and does not read the peers.
    #[test]
    fn the_slots_the_platform_reports_decide_the_remedy() {
        let with = |pending_removals: Option<u32>| Problem {
            pending_removals,
            ..problem(ProblemKind::PeerLimitReached, None)
        };
        let listed = [peer("peer-2", "bench", PeerStatus::PendingRestart)];

        assert_eq!(
            full_router_message(&with(Some(2)), None, "ws-1", "p-1"),
            "Peer limit reached\n\nA restart of the router frees 2 slot(s):\n    \
             peppy platform router restart --workspace ws-1 --project p-1"
        );
        let none_freed = full_router_message(&with(Some(0)), Some(&listed), "ws-1", "p-1");
        assert_eq!(
            none_freed,
            "Peer limit reached\n\nA restart of the router frees no slot. Remove a peer \
             (`peppy platform unenroll` on its machine), then restart the router.",
            "zero is the platform's answer: the list does not change it"
        );
        assert!(
            full_router_message(&with(None), Some(&listed), "ws-1", "p-1")
                .contains("1 removed peer(s) keep a slot"),
            "with no number from the platform, the peers give the answer"
        );
    }

    #[test]
    fn a_full_router_whose_peers_cannot_be_listed_prints_the_refusal_alone() {
        assert_eq!(
            full_router_message(
                &problem(ProblemKind::PeerLimitReached, None),
                None,
                "ws-1",
                "p-1"
            ),
            "Peer limit reached"
        );
    }

    #[test]
    fn the_try_again_message_gives_the_delay_the_platform_asked_for() {
        assert_eq!(
            try_again_message(&problem(ProblemKind::ProvisionerUnavailable, Some(5))),
            "Peer limit reached. Run the command again in 5 seconds."
        );
        assert_eq!(
            try_again_message(&problem(ProblemKind::ProvisionerUnavailable, None)),
            "Peer limit reached. Run the command again in a moment."
        );
    }
}
