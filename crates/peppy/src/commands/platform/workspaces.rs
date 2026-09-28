//! `peppy platform workspaces`: list the workspaces the signed-in account
//! belongs to.

use std::sync::Arc;

use daemon_config::consts::PeppyDirs;

use crate::commands::Command;
use crate::commands::platform::PlatformSession;
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

        if self.json {
            let doc: Vec<_> = workspaces
                .iter()
                .map(|w| serde_json::json!({ "id": w.id, "name": w.name, "tier": w.tier }))
                .collect();
            println!("{}", serde_json::Value::Array(doc));
            return Ok(());
        }
        if workspaces.is_empty() {
            println!("No workspaces; create one in the web app first.");
            return Ok(());
        }
        let rows: Vec<[String; 3]> = workspaces
            .iter()
            .map(|w| [w.id.clone(), w.name.clone(), w.tier.clone()])
            .collect();
        print!("{}", super::peers::table(["ID", "NAME", "TIER"], &rows));
        Ok(())
    }
}
