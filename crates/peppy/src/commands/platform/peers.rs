//! `peppy platform peers`: the peers enrolled in a project's cloud router, as
//! the platform reports them, with this machine marked by its peer id. The
//! project is the one the flags name, else the project of the context, else
//! the one this machine is enrolled in.

use std::sync::Arc;

use daemon_config::consts::PeppyDirs;

use crate::commands::Command;
use crate::commands::platform::{PlatformSession, date, select};
use crate::commands::table::render_columns;
use crate::context::AppContext;
use crate::error::Result;
use auth::client::{self, PeerStatus, RouterPeer};

pub struct PeersCommand {
    pub api_url: Option<String>,
    pub workspace: Option<String>,
    pub project: Option<String>,
    pub json: bool,
    /// Test seam: override the peppy data dirs.
    pub peppy_dirs: Option<PeppyDirs>,
}

impl Command for PeersCommand {
    fn execute(self, _ctx: &Arc<AppContext>) -> Result<()> {
        let session = PlatformSession::resolve(self.peppy_dirs, self.api_url.as_deref())?;
        let mut cred = session.credential()?;
        let (target, enrollment) = session.resolve_target(
            &mut cred,
            self.workspace.as_deref(),
            self.project.as_deref(),
        )?;
        let peers = client::list_peers(
            &session.http,
            &session.api_url,
            &mut cred,
            &target.workspace_id,
            &target.project_id,
        )
        .map_err(|error| select::refusal_on(&target, error))?;
        let select::Target {
            workspace_id,
            project_id,
            ..
        } = target;
        let this_machine = enrollment
            .as_ref()
            .filter(|e| e.document.project_id == project_id)
            .map(|e| e.document.peer_id.as_str());

        if self.json {
            let doc = serde_json::json!({
                "workspace_id": workspace_id,
                "project_id": project_id,
                "this_machine": this_machine,
                "peers": peers.iter().map(|p| serde_json::json!({
                    "id": p.id,
                    "name": p.name,
                    "certificate_cn": p.certificate_cn,
                    "status": p.status,
                    "certificate_expires_at": p.certificate_expires_at.to_rfc3339(),
                    "created_at": p.created_at.to_rfc3339(),
                })).collect::<Vec<_>>(),
            });
            println!("{doc}");
            return Ok(());
        }
        print!("{}", render_human(&project_id, &peers, this_machine));
        Ok(())
    }
}

/// The status of a peer in words for the person. `unknown` is a real state of
/// the platform: the router's admin space is off or did not answer, so the
/// platform has nothing to report. It is not a failure of the peer.
pub(crate) fn peer_status_label(status: &PeerStatus) -> &str {
    match status {
        PeerStatus::Unknown => "not reported by the platform",
        other => other.as_str(),
    }
}

pub(crate) fn render_human(
    project_id: &str,
    peers: &[RouterPeer],
    this_machine: Option<&str>,
) -> String {
    let mut out = format!("Project {project_id}\n\n");
    if peers.is_empty() {
        out.push_str("No peers; `peppy platform enroll` adds this machine.\n");
        return out;
    }
    let mut ordered: Vec<&RouterPeer> = peers.iter().collect();
    // This machine first, then by name, so the reader's own row is never buried.
    ordered.sort_by_key(|p| (Some(p.id.as_str()) != this_machine, p.name.clone()));
    let rows: Vec<[String; 5]> = ordered
        .iter()
        .map(|p| {
            [
                p.name.clone(),
                peer_status_label(&p.status).to_string(),
                date(&p.certificate_expires_at),
                p.id.clone(),
                if Some(p.id.as_str()) == this_machine {
                    "(this machine)".to_string()
                } else {
                    String::new()
                },
            ]
        })
        .collect();
    out.push_str(&render_columns(
        ["PEER", "STATUS", "CERT EXPIRES", "ID", ""],
        &rows,
    ));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use auth::test_support::router_peer as peer;

    #[test]
    fn this_machine_sorts_first_and_is_marked() {
        let out = render_human(
            "p-1",
            &[
                peer("peer-2", "bench", PeerStatus::Unknown),
                peer("peer-1", "robot-7", PeerStatus::Connected),
                peer("peer-3", "arm", PeerStatus::PendingRestart),
            ],
            Some("peer-1"),
        );
        assert_eq!(
            out,
            "Project p-1\n\n\
             PEER     STATUS                        CERT EXPIRES  ID\n\
             robot-7  connected                     2027-01-01    peer-1  (this machine)\n\
             arm      pending_restart               2027-01-01    peer-3\n\
             bench    not reported by the platform  2027-01-01    peer-2\n"
        );
    }

    /// Every status prints as the platform sent it, but `unknown`, which says
    /// what it means.
    #[test]
    fn only_unknown_is_put_into_words() {
        assert_eq!(
            peer_status_label(&PeerStatus::Unknown),
            "not reported by the platform"
        );
        for status in [
            PeerStatus::Connected,
            PeerStatus::PendingRestart,
            PeerStatus::Other("some_future_state".into()),
        ] {
            assert_eq!(peer_status_label(&status), status.as_str());
        }
    }

    #[test]
    fn an_empty_roster_explains_how_to_populate_it() {
        assert!(render_human("p-1", &[], None).contains("peppy platform enroll"));
    }
}
