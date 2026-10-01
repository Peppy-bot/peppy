//! `peppy platform workspaces`: list the workspaces the signed-in account
//! belongs to. A `*` marks the workspace of the context.

use std::sync::Arc;

use daemon_config::consts::PeppyDirs;

use crate::commands::Command;
use crate::commands::platform::PlatformSession;
use crate::commands::table::render_columns;
use crate::context::AppContext;
use crate::error::Result;
use auth::client;

pub struct WorkspacesCommand {
    pub api_url: Option<String>,
    pub json: bool,
    /// Test seam: override the peppy data dirs.
    pub peppy_dirs: Option<PeppyDirs>,
}

impl Command for WorkspacesCommand {
    fn execute(self, _ctx: &Arc<AppContext>) -> Result<()> {
        let session = PlatformSession::resolve(self.peppy_dirs, self.api_url.as_deref())?;
        let mut cred = session.credential()?;
        let workspaces = client::list_workspaces(&session.http, &session.api_url, &mut cred)?;
        let context = session.context()?;
        let is_current = |workspace_id: &str| {
            context
                .as_ref()
                .is_some_and(|c| c.workspace.id == workspace_id)
        };

        if self.json {
            let doc: Vec<_> = workspaces
                .iter()
                .map(|w| {
                    serde_json::json!({
                        "id": w.id, "name": w.name, "tier": w.tier, "current": is_current(&w.id),
                    })
                })
                .collect();
            println!("{}", serde_json::Value::Array(doc));
            return Ok(());
        }
        if workspaces.is_empty() {
            println!("No workspaces; create one in the web app first.");
            return Ok(());
        }
        let rows: Vec<[String; 4]> = workspaces
            .iter()
            .map(|w| {
                [
                    current_mark(is_current(&w.id)),
                    w.id.clone(),
                    w.name.clone(),
                    w.tier.clone(),
                ]
            })
            .collect();
        print!("{}", render_columns(["", "ID", "NAME", "TIER"], &rows));
        Ok(())
    }
}

/// The first column of a list: `*` on the row of the context.
pub(crate) fn current_mark(current: bool) -> String {
    if current { "*" } else { "" }.to_string()
}
