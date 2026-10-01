//! `peppy platform workspace`: the selected workspace. `show` prints it,
//! `list` prints the workspaces of the account with the selected one marked,
//! `use` selects one, and `clear` removes the selection: the workspace and its
//! project.
//!
//! A workspace that `use` selects keeps the selected project when it is the
//! workspace of that project. A different workspace starts with no project:
//! `peppy platform project use` selects one.

use std::sync::Arc;

use clap::Subcommand;
use daemon_config::consts::PeppyDirs;

use crate::commands::Command;
use crate::commands::platform::PlatformSession;
use crate::commands::platform::select::{self, Ask, Question};
use crate::commands::platform::selection::{save_selection, selection_report, workspace_json};
use crate::commands::table::render_columns;
use crate::context::AppContext;
use crate::error::Result;

#[derive(Subcommand)]
pub enum WorkspaceCommands {
    /// Show the selected workspace
    Show {
        /// Emit machine-readable JSON.
        #[arg(long)]
        json: bool,
    },
    /// List the workspaces you belong to, the selected one marked
    List {
        /// Emit machine-readable JSON.
        #[arg(long)]
        json: bool,
    },
    /// Select the workspace the platform commands act on
    Use {
        /// The workspace, by id or exact name (else the only one, else you select it from a menu).
        workspace: Option<String>,
    },
    /// Remove the selected workspace and its project
    Clear,
}

pub struct WorkspaceCommand {
    pub command: WorkspaceCommands,
    pub api_url: Option<String>,
    /// How `use` asks the person: on the terminal, with scripted answers, or
    /// not at all.
    pub ask: Ask,
    /// Test seam: override the peppy data dirs.
    pub peppy_dirs: Option<PeppyDirs>,
}

impl Command for WorkspaceCommand {
    fn execute(mut self, _ctx: &Arc<AppContext>) -> Result<()> {
        let session = PlatformSession::resolve(self.peppy_dirs, self.api_url.as_deref())?;
        match self.command {
            WorkspaceCommands::Show { json } => show(&session, json),
            WorkspaceCommands::List { json } => list(&session, json),
            WorkspaceCommands::Use { workspace } => {
                use_workspace(&session, workspace.as_deref(), &mut self.ask)
            }
            WorkspaceCommands::Clear => {
                auth::selection::remove(&session.dirs)?;
                println!("No workspace and no project are selected.");
                Ok(())
            }
        }
    }
}

fn show(session: &PlatformSession, json: bool) -> Result<()> {
    let selection = session.selection()?;
    if json {
        println!("{}", workspace_json(selection.as_ref()));
        return Ok(());
    }
    match selection {
        Some(selection) => println!("Selected {}.", selection.workspace_label()),
        None => println!("No workspace is selected: `peppy platform workspace use` selects one."),
    }
    Ok(())
}

fn list(session: &PlatformSession, json: bool) -> Result<()> {
    let workspaces = session.api()?.list_workspaces()?;
    let selection = session.selection()?;
    let is_selected = |workspace_id: &str| {
        selection
            .as_ref()
            .is_some_and(|s| s.workspace.id == workspace_id)
    };

    if json {
        let doc: Vec<_> = workspaces
            .iter()
            .map(|w| {
                serde_json::json!({
                    "id": w.id, "name": w.name, "tier": w.tier, "selected": is_selected(&w.id),
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
                selected_mark(is_selected(&w.id)),
                w.id.clone(),
                w.name.clone(),
                w.tier.clone(),
            ]
        })
        .collect();
    print!("{}", render_columns(["", "ID", "NAME", "TIER"], &rows));
    Ok(())
}

fn use_workspace(session: &PlatformSession, flag: Option<&str>, ask: &mut Ask) -> Result<()> {
    let mut api = session.api()?;
    // A selection that cannot be read is replaced, so it starts the menu
    // nowhere.
    let current = session.selection().ok().flatten();
    let question = Question {
        title: "Workspace",
        flag,
        default_id: None,
        current_id: current.as_ref().map(|s| s.workspace.id.as_str()),
        decline: None,
        remedy: "name it: `peppy platform workspace use <id|name>`",
    };
    let workspace = select::pick_workspace(&mut api, question, ask)?.required("workspace")?;
    let project = select::selected_project_in(current.as_ref(), &workspace).cloned();
    let selection = save_selection(session, &mut api, (&workspace).into(), project)?;
    print!("{}", selection_report(session, &selection));
    Ok(())
}

/// The first column of a list: `*` on the row of the selected one.
pub(crate) fn selected_mark(selected: bool) -> String {
    if selected { "*" } else { "" }.to_string()
}
