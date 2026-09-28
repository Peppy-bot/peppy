//! `peppy platform unenroll`: leave the project's cloud router. Removes the
//! peer on the platform (unless `--local-only`), deletes the local enrollment
//! bundle, and pokes the daemon so it restarts standalone under `local`.
//!
//! The platform applies a removal when the cloud router restarts, because the
//! router reads its trust anchors at start. The restart is the person's to
//! request, so the command prints what waits and the command that applies it.

use std::sync::Arc;

use daemon_config::consts::PeppyDirs;

use crate::commands::Command;
use crate::commands::platform::router::{how_to_apply, pending_change_lines};
use crate::commands::platform::{
    FederationPokeAction, PlatformSession, confirm_restart, external_router_note,
    federation_is_managed, poke_federation_and_report,
};
use crate::context::AppContext;
use crate::error::{Error, Result};
use auth::client::{self, PeerRemoval};
use auth::enrollment::{self, EnrollmentDocument};

pub struct UnenrollCommand {
    pub api_url: Option<String>,
    /// Only delete the local enrollment; leave the platform's peer alone.
    pub local_only: bool,
    /// Skip the daemon-restart confirmation prompt.
    pub yes: bool,
    /// Test seam: override the peppy data dirs.
    pub peppy_dirs: Option<PeppyDirs>,
}

impl Command for UnenrollCommand {
    fn execute(self, ctx: &Arc<AppContext>) -> Result<()> {
        let session = PlatformSession::resolve(self.peppy_dirs, self.api_url.as_deref())?;
        let managed = federation_is_managed(session.daemon_state.as_ref(), &session.config);

        let Some(existing) = enrollment::load(&session.dirs).map_err(Error::AuthEngine)? else {
            println!("This machine is not enrolled.");
            return Ok(());
        };
        let document = &existing.document;

        if managed
            && !confirm_restart(
                ctx,
                self.yes,
                &FederationPokeAction::Unenroll,
                session.daemon_state.as_ref(),
            )?
        {
            println!("Unenrollment aborted.");
            return Ok(());
        }

        if !self.local_only {
            let mut cred = session.credential().map_err(|e| {
                match e {
                Error::AuthEngine(auth::AuthError::NotAuthenticated) => Error::Auth(
                    "not signed in; run `peppy platform login` to remove the peer on the platform, \
                     or pass --local-only to only delete the local enrollment"
                        .to_string(),
                ),
                other => other,
            }
            })?;
            let removal = client::remove_peer(
                &session.http,
                &session.api_url,
                &mut cred,
                &document.workspace_id,
                &document.project_id,
                &document.peer_id,
            )?;
            print!("{}", removal_report(document, &removal));
        }

        enrollment::remove(&session.dirs)?;
        println!("Deleted the local enrollment.");

        if !managed {
            println!("{}", external_router_note(&session.dirs));
            return Ok(());
        }
        poke_federation_and_report(&session.dirs, FederationPokeAction::Unenroll)
    }
}

/// What the platform did with the removal of `document`'s peer, and what the
/// person does next. A staged removal lists each change that waits for the
/// restart, in the platform's words, and gives the commands that apply it.
pub(crate) fn removal_report(document: &EnrollmentDocument, removal: &PeerRemoval) -> String {
    let peer = format!("{} ({})", document.peer_name, document.peer_id);
    let PeerRemoval::Staged(status) = removal else {
        return format!(
            "Peer {peer} was already removed from project {}.\n",
            document.project_id
        );
    };
    let mut out = format!(
        "Removed peer {peer} from project {}.\n",
        document.project_id
    );
    for line in pending_change_lines(status) {
        out.push_str(&format!("  pending: {line}\n"));
    }
    out.push_str(&format!(
        "The router refuses this peer, and frees its slot, from its next restart. The restart is \
         yours to request. {}\n",
        how_to_apply(&status.phase, &document.workspace_id, &document.project_id)
    ));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use auth::client::{PendingChange, RouterStatus};

    fn document() -> EnrollmentDocument {
        EnrollmentDocument {
            version: enrollment::ENROLLMENT_VERSION,
            api_url: "https://api.example".into(),
            workspace_id: "ws-1".into(),
            project_id: "p-1".into(),
            peer_id: "peer-1".into(),
            peer_name: "robot-7".into(),
            zenoh_id: pmi::RouterId::parse("7f3a9c1e").unwrap(),
            namespace: config::namespace::Namespace::parse("p-1").unwrap(),
            router: auth::RouterEndpoint::parse("rtr.example", 7447).unwrap(),
            certificate_expires_at: 2_000_000_000,
            enrolled_at: 1_700_000_000,
        }
    }

    #[test]
    fn a_staged_removal_prints_what_waits_and_the_restart_command() {
        let staged = PeerRemoval::Staged(RouterStatus {
            phase: "running".into(),
            desired_state: "running".into(),
            can_manage_infra: false,
            address: None,
            pending_changes: true,
            pending_change_entries: vec![PendingChange {
                kind: "peer".into(),
                description: "peer robot-7 removed".into(),
                staged_at: "2026-09-28T10:00:00Z".parse().unwrap(),
                deadline: None,
            }],
            peers: Vec::new(),
        });
        assert_eq!(
            removal_report(&document(), &staged),
            "Removed peer robot-7 (peer-1) from project p-1.\n\
             \x20 pending: peer robot-7 removed (staged 2026-09-28)\n\
             The router refuses this peer, and frees its slot, from its next restart. The \
             restart is yours to request. Restart the router:\n\
             \x20   peppy platform router restart --workspace ws-1 --project p-1\n"
        );
    }

    /// On a stopped router the removal takes a start and then a restart.
    #[test]
    fn a_removal_on_a_stopped_router_gives_the_two_commands() {
        let staged = PeerRemoval::Staged(RouterStatus {
            phase: "stopped".into(),
            desired_state: "stopped".into(),
            can_manage_infra: false,
            address: None,
            pending_changes: true,
            pending_change_entries: Vec::new(),
            peers: Vec::new(),
        });
        let report = removal_report(&document(), &staged);
        assert!(
            report.ends_with(
                "A start alone does not apply the changes that wait:\n\
                 \x20   peppy platform router start --workspace ws-1 --project p-1\n\
                 \x20   peppy platform router restart --workspace ws-1 --project p-1\n"
            ),
            "{report}"
        );
    }

    #[test]
    fn a_peer_that_is_already_gone_needs_no_restart() {
        assert_eq!(
            removal_report(&document(), &PeerRemoval::AlreadyRemoved),
            "Peer robot-7 (peer-1) was already removed from project p-1.\n"
        );
    }
}
