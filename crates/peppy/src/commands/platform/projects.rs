//! `peppy platform projects`: list the projects of a workspace: the one the
//! flag names, else the workspace of the context, else the only one. Archived
//! projects own no running router and are hidden from the table; `--json`
//! includes them with their `archived_at`. A `*` marks the project of the
//! context.

use std::sync::Arc;

use daemon_config::consts::PeppyDirs;

use crate::commands::Command;
use crate::commands::platform::workspaces::current_mark;
use crate::commands::platform::{PlatformSession, select};
use crate::commands::table::render_columns;
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
        let context = session.context()?;
        let workspace = select::resolve_workspace(
            &session.http,
            &session.api_url,
            &mut cred,
            self.workspace.as_deref(),
            context.as_ref().map(|c| c.workspace.id.as_str()),
            &mut select::Ask::Never,
        )?;
        let is_current = |project_id: &str| {
            context
                .as_ref()
                .is_some_and(|c| c.workspace.id == workspace.id && c.project.id == project_id)
        };
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
                        "current": is_current(&p.id),
                    })
                })
                .collect();
            println!("{}", serde_json::Value::Array(doc));
            return Ok(());
        }
        println!("Workspace {} ({})\n", workspace.name, workspace.id);
        let rows: Vec<[String; 3]> = projects
            .iter()
            .filter(|p| !p.is_archived())
            .map(|p| {
                [
                    current_mark(is_current(&p.id)),
                    p.id.clone(),
                    p.name.clone(),
                ]
            })
            .collect();
        if rows.is_empty() {
            println!("No projects; create one in the web app first.");
            return Ok(());
        }
        print!("{}", render_columns(["", "ID", "NAME"], &rows));
        Ok(())
    }
}
