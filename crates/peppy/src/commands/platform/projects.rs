//! `peppy platform projects`: list the projects of a workspace. Archived
//! projects own no running router and are hidden from the table; `--json`
//! includes them with their `archived_at`.

use std::sync::Arc;

use daemon_config::consts::PeppyDirs;

use crate::commands::Command;
use crate::commands::platform::{PlatformSession, select};
use crate::context::AppContext;
use crate::error::Result;
use auth::client;

pub struct ProjectsCommand {
    pub api_url: Option<String>,
    pub workspace: Option<String>,
    pub json: bool,
    /// Test seam: override the peppy data dirs.
    pub peppy_dirs: Option<PeppyDirs>,
}

impl Command for ProjectsCommand {
    fn execute(self, _ctx: &Arc<AppContext>) -> Result<()> {
        let session = PlatformSession::resolve(self.peppy_dirs, self.api_url.as_deref())?;
        let mut cred = session.credential()?;
        let workspace = select::resolve_workspace(
            &session.http,
            &session.api_url,
            &mut cred,
            self.workspace.as_deref(),
        )?;
        let projects =
            client::list_projects(&session.http, &session.api_url, &mut cred, &workspace.id)?;

        if self.json {
            let doc: Vec<_> = projects
                .iter()
                .map(|p| {
                    serde_json::json!({
                        "id": p.id,
                        "workspace_id": p.workspace_id,
                        "name": p.name,
                        "archived_at": p.archived_at.map(|t| t.to_rfc3339()),
                    })
                })
                .collect();
            println!("{}", serde_json::Value::Array(doc));
            return Ok(());
        }
        println!("Workspace {} ({})\n", workspace.name, workspace.id);
        let rows: Vec<[String; 2]> = projects
            .iter()
            .filter(|p| !p.is_archived())
            .map(|p| [p.id.clone(), p.name.clone()])
            .collect();
        if rows.is_empty() {
            println!("No projects; create one in the web app first.");
            return Ok(());
        }
        print!("{}", super::peers::table(["ID", "NAME"], &rows));
        Ok(())
    }
}
