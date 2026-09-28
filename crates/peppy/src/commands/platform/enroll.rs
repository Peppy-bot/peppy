//! `peppy platform enroll`: join a project's cloud router. Picks the
//! workspace and project, mints this machine's key pair and signing request,
//! exchanges the request for a signed certificate, writes the enrollment
//! bundle, and pokes the daemon so it restarts under the new identity and
//! verifies the mutual-TLS link. The private key never leaves the machine.

use std::sync::Arc;

use daemon_config::consts::PeppyDirs;

use crate::commands::Command;
use crate::commands::platform::router::restart_command;
use crate::commands::platform::unenroll::removal_report;
use crate::commands::platform::{
    FederationPokeAction, PlatformSession, confirm_restart, date_of, external_router_note,
    federation_is_managed, peers, poke_federation_and_report, select,
};
use crate::context::AppContext;
use crate::error::{Error, Result};
use auth::client::RouterPeer;
use auth::csr::{self, PeerName};
use auth::enrollment::{self, EnrollmentBundle};
use auth::{AuthError, Problem, ProblemKind, client, storage};

pub struct EnrollCommand {
    pub api_url: Option<String>,
    pub workspace: Option<String>,
    pub project: Option<String>,
    /// The peer's name; defaults to this machine's core-node name.
    pub name: Option<String>,
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
        let managed = federation_is_managed(session.daemon_state.as_ref(), &session.config);

        let existing = enrollment::load(&session.dirs).map_err(Error::AuthEngine)?;
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

        let mut cred = session.credential()?;
        let name = peer_name(
            self.name.as_deref(),
            session
                .daemon_state
                .as_ref()
                .map(|s| s.core_node_name.as_str()),
            session.config.core_node_name.as_deref(),
        )?;
        let context = session.context()?;
        let selection = select::resolve_project(
            &session.http,
            &session.api_url,
            &mut cred,
            self.workspace.as_deref(),
            self.project.as_deref(),
            context.as_ref(),
            &mut select::Ask::Never,
        )?;

        // Confirm before anything is minted, so an aborted enrollment leaves
        // no peer behind on the platform.
        if managed
            && !confirm_restart(
                ctx,
                self.yes,
                &FederationPokeAction::Enroll,
                session.daemon_state.as_ref(),
            )?
        {
            println!("Enrollment aborted.");
            return Ok(());
        }

        let identity = csr::generate_peer_identity(&name)?;
        let enrolled = match client::enroll_peer(
            &session.http,
            &session.api_url,
            &mut cred,
            &selection.workspace.id,
            &selection.project.id,
            &identity.csr_pem,
        ) {
            Ok(enrolled) => enrolled,
            Err(error) => {
                return Err(explain_refusal(&session, &mut cred, &selection, error));
            }
        };
        let bundle = EnrollmentBundle::from_platform(
            &session.api_url,
            &selection.workspace.id,
            &selection.project.id,
            enrolled,
            identity.private_key_pem,
            storage::now_unix(),
        )?;
        enrollment::save(&session.dirs, &bundle)?;
        println!(
            "Enrolled {} in project {} ({}) of workspace {}.",
            bundle.document.peer_name,
            selection.project.name,
            selection.project.id,
            selection.workspace.name
        );
        println!(
            "The peer certificate expires on {}; re-run `peppy platform enroll --replace` before then.",
            date_of(bundle.document.certificate_expires_at)
        );

        // The new enrollment is on disk, so the old peer is now surplus on the
        // platform. Best effort: a failure here leaves a stale peer to remove
        // from the web app, never a machine without an enrollment.
        if let Some(old) = existing {
            match client::remove_peer(
                &session.http,
                &session.api_url,
                &mut cred,
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
}

/// Puts a refused enrollment into words the person can act on. The refusals
/// with a remedy in the CLI are matched on the kind of the problem; every
/// other refusal prints as the platform sent it.
fn explain_refusal(
    session: &PlatformSession,
    cred: &mut auth::Credential,
    selection: &select::Selection,
    error: AuthError,
) -> Error {
    let AuthError::Problem(problem) = &error else {
        return Error::AuthEngine(error);
    };
    match problem.kind {
        ProblemKind::PeerLimitReached => {
            // Best effort: the refusal is the answer, the list only explains it.
            let peers = client::list_peers(
                &session.http,
                &session.api_url,
                cred,
                &selection.workspace.id,
                &selection.project.id,
            )
            .ok();
            Error::Auth(full_router_message(
                problem,
                peers.as_deref(),
                &selection.workspace.id,
                &selection.project.id,
            ))
        }
        ProblemKind::ProvisionerUnavailable => Error::Auth(try_again_message(problem)),
        _ => Error::AuthEngine(error),
    }
}

/// The message for a router that admits no more peers: the refusal, the peers
/// that hold the slots, and the remedy. A removed peer keeps its slot until the
/// router restarts, so when one waits the remedy is the restart.
fn full_router_message(
    problem: &Problem,
    peers: Option<&[RouterPeer]>,
    workspace_id: &str,
    project_id: &str,
) -> String {
    let mut out = problem.to_string();
    let Some(peers) = peers else {
        return out;
    };
    out.push_str("\n\n");
    out.push_str(&peers::render_human(project_id, peers, None));
    let waiting = peers
        .iter()
        .filter(|peer| peer.status == "pending_restart")
        .count();
    out.push('\n');
    if waiting == 0 {
        out.push_str(
            "Each slot is in use. Remove a peer (`peppy platform unenroll` on its machine), \
             then restart the router.",
        );
        return out;
    }
    out.push_str(&format!(
        "{waiting} removed peer(s) keep a slot until the router restarts. To free the slot:\n    {}",
        restart_command(workspace_id, project_id)
    ));
    out
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

/// The name this machine enrolls under: the flag, else the running daemon's
/// core-node name, else the configured one. The daemon derives a
/// machine-specific default that the CLI cannot see, so a machine whose daemon
/// has never run and whose config names nothing must be told the name.
fn peer_name(
    flag: Option<&str>,
    daemon_core_node_name: Option<&str>,
    configured_core_node_name: Option<&str>,
) -> Result<PeerName> {
    let raw = flag
        .or(daemon_core_node_name)
        .or(configured_core_node_name)
        .ok_or_else(|| {
            Error::ExecutionFailed(
                "no name for this machine: start the daemon once, set `core_node_name` in \
                 peppy_config.json5, or pass --name <name>"
                    .to_string(),
            )
        })?;
    PeerName::parse(raw).map_err(Error::AuthEngine)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_peer_name_prefers_the_flag_then_the_daemon_then_the_config() {
        assert_eq!(
            peer_name(Some("flag"), Some("cn-daemon"), Some("cn-config"))
                .unwrap()
                .as_str(),
            "flag"
        );
        assert_eq!(
            peer_name(None, Some("cn-daemon"), Some("cn-config"))
                .unwrap()
                .as_str(),
            "cn-daemon"
        );
        assert_eq!(
            peer_name(None, None, Some("cn-config")).unwrap().as_str(),
            "cn-config"
        );
        let err = peer_name(None, None, None).unwrap_err();
        assert!(err.to_string().contains("--name"), "{err}");
        assert!(peer_name(Some(" bad "), None, None).is_err());
    }

    fn problem(kind: ProblemKind, retry_after_secs: Option<u64>) -> Problem {
        Problem {
            kind,
            status: 422,
            title: "Peer limit reached".into(),
            detail: None,
            retry_after_secs,
        }
    }

    fn peer(id: &str, name: &str, status: &str) -> RouterPeer {
        RouterPeer {
            id: id.into(),
            name: name.into(),
            certificate_cn: name.into(),
            status: status.into(),
            certificate_expires_at: "2027-01-01T00:00:00Z".parse().unwrap(),
            created_at: "2026-10-03T00:00:00Z".parse().unwrap(),
        }
    }

    #[test]
    fn a_full_router_with_a_removed_peer_names_the_restart() {
        let message = full_router_message(
            &problem(ProblemKind::PeerLimitReached, None),
            Some(&[
                peer("peer-1", "arm", "connected"),
                peer("peer-2", "bench", "pending_restart"),
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
            Some(&[peer("peer-1", "arm", "connected")]),
            "ws-1",
            "p-1",
        );
        assert!(message.contains("Each slot is in use"), "{message}");
        assert!(!message.contains("router restart --workspace"), "{message}");
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
