//! `peppy platform peers`: the peers enrolled in a project's cloud router, as
//! the platform reports them, with this machine marked by its peer id. The
//! project defaults to the one this machine is enrolled in.

use std::sync::Arc;

use daemon_config::consts::PeppyDirs;

use crate::commands::Command;
use crate::commands::platform::{PlatformSession, select};
use crate::context::AppContext;
use crate::error::{Error, Result};
use auth::client::{self, RouterPeer};

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
        let enrollment = auth::enrollment::load(&session.dirs).map_err(Error::AuthEngine)?;

        // Flags name the project; otherwise it is the enrolled one.
        let (workspace_id, project_id) = match (&self.workspace, &self.project, &enrollment) {
            (None, None, Some(enrollment)) => (
                enrollment.document.workspace_id.clone(),
                enrollment.document.project_id.clone(),
            ),
            (None, None, None) => {
                return Err(Error::ExecutionFailed(
                    "this machine is not enrolled; pass --project <id|name> (and --workspace) \
                     to name the project whose peers to list"
                        .to_string(),
                ));
            }
            _ => {
                let selection = select::resolve_project(
                    &session.http,
                    &session.api_url,
                    &mut cred,
                    self.workspace.as_deref(),
                    self.project.as_deref(),
                )?;
                (selection.workspace.id, selection.project.id)
            }
        };
        let peers = client::list_peers(
            &session.http,
            &session.api_url,
            &mut cred,
            &workspace_id,
            &project_id,
        )?;
        let this_machine = enrollment
            .as_ref()
            .map(|e| e.document.peer_id.as_str())
            .filter(|_| {
                enrollment
                    .as_ref()
                    .is_some_and(|e| e.document.project_id == project_id)
            });

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

fn render_human(project_id: &str, peers: &[RouterPeer], this_machine: Option<&str>) -> String {
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
                p.status.clone(),
                p.certificate_expires_at.format("%Y-%m-%d").to_string(),
                p.id.clone(),
                if Some(p.id.as_str()) == this_machine {
                    "(this machine)".to_string()
                } else {
                    String::new()
                },
            ]
        })
        .collect();
    out.push_str(&table(["PEER", "STATUS", "CERT EXPIRES", "ID", ""], &rows));
    out
}

/// Renders `rows` under `headers` as space-aligned columns, each column as wide
/// as its widest value, with no trailing whitespace on a line.
pub(crate) fn table<const N: usize>(headers: [&str; N], rows: &[[String; N]]) -> String {
    let widths: Vec<usize> = (0..N)
        .map(|i| {
            rows.iter()
                .map(|r| r[i].chars().count())
                .chain(std::iter::once(headers[i].chars().count()))
                .max()
                .unwrap_or(0)
        })
        .collect();
    let line = |cells: Vec<&str>| {
        let mut s = String::new();
        for (i, cell) in cells.iter().enumerate() {
            if i > 0 {
                s.push_str("  ");
            }
            s.push_str(cell);
            let pad = widths[i].saturating_sub(cell.chars().count());
            s.push_str(&" ".repeat(pad));
        }
        format!("{}\n", s.trim_end())
    };
    let mut out = line(headers.to_vec());
    for row in rows {
        out.push_str(&line(row.iter().map(String::as_str).collect()));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn this_machine_sorts_first_and_is_marked() {
        let out = render_human(
            "p-1",
            &[
                peer("peer-2", "bench", "unknown"),
                peer("peer-1", "robot-7", "connected"),
                peer("peer-3", "arm", "pending_restart"),
            ],
            Some("peer-1"),
        );
        assert_eq!(
            out,
            "Project p-1\n\n\
             PEER     STATUS           CERT EXPIRES  ID\n\
             robot-7  connected        2027-01-01    peer-1  (this machine)\n\
             arm      pending_restart  2027-01-01    peer-3\n\
             bench    unknown          2027-01-01    peer-2\n"
        );
    }

    #[test]
    fn an_empty_roster_explains_how_to_populate_it() {
        assert!(render_human("p-1", &[], None).contains("peppy platform enroll"));
    }
}
