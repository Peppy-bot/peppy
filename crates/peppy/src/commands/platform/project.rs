//! `peppy platform project`: the selected project. `show` prints it, `list`
//! prints the projects of a workspace with the selected one marked, `use`
//! selects one, and `clear` removes the selected project and keeps its
//! workspace selected.
//!
//! The workspace of `list` and `use` is the one `--workspace` names, else the
//! selected one, else the only one. Archived projects own no running router:
//! `list` hides them from the table and `--json` includes them with their
//! `archived_at`, and `use` does not offer them.

use std::sync::Arc;

use clap::Subcommand;
use daemon_config::consts::PeppyDirs;

use crate::commands::Command;
use crate::commands::platform::PlatformSession;
use crate::commands::platform::select::{self, Ask, Question};
use crate::commands::platform::selection::{project_json, save_selection, selection_report};
use crate::commands::platform::workspace::selected_mark;
use crate::commands::table::render_columns;
use crate::context::AppContext;
use crate::error::Result;

#[derive(Subcommand)]
pub enum ProjectCommands {
    /// Show the selected project
    Show {
        /// Emit machine-readable JSON.
        #[arg(long)]
        json: bool,
    },
    /// List the projects of a workspace (the selected one by default), the selected project marked
    List {
        /// The workspace, by id or exact name (else the selected one, else the only one).
        #[arg(long)]
        workspace: Option<String>,
        /// Emit machine-readable JSON (includes archived projects).
        #[arg(long)]
        json: bool,
    },
    /// Select the project the platform commands act on
    Use {
        /// The project, by id or exact name (else the only one, else you select it from a menu).
        project: Option<String>,
        /// The workspace of the project, by id or exact name (else the selected one, else the only one, else you select it from a menu).
        #[arg(long)]
        workspace: Option<String>,
    },
    /// Remove the selected project; its workspace stays selected
    Clear,
}

pub struct ProjectCommand {
    pub command: ProjectCommands,
    pub api_url: Option<String>,
    /// How `use` asks the person: on the terminal, with scripted answers, or
    /// not at all.
    pub ask: Ask,
    /// Test seam: override the peppy data dirs.
    pub peppy_dirs: Option<PeppyDirs>,
}

impl Command for ProjectCommand {
    fn execute(mut self, _ctx: &Arc<AppContext>) -> Result<()> {
        let session = PlatformSession::resolve(self.peppy_dirs, self.api_url.as_deref())?;
        match self.command {
            ProjectCommands::Show { json } => show(&session, json),
            ProjectCommands::List { workspace, json } => list(&session, workspace.as_deref(), json),
            ProjectCommands::Use { project, workspace } => use_project(
                &session,
                workspace.as_deref(),
                project.as_deref(),
                &mut self.ask,
            ),
            ProjectCommands::Clear => clear(&session),
        }
    }
}

fn show(session: &PlatformSession, json: bool) -> Result<()> {
    let selection = session.selection()?;
    if json {
        println!("{}", project_json(selection.as_ref()));
        return Ok(());
    }
    match selection {
        Some(selection) => print!("{}", selection_report(session, &selection)),
        None => println!("No project is selected: `peppy platform project use` selects one."),
    }
    Ok(())
}

fn list(session: &PlatformSession, workspace_flag: Option<&str>, json: bool) -> Result<()> {
    let mut api = session.api()?;
    let selection = session.selection()?;
    let workspace = select::resolve_workspace(
        &mut api,
        workspace_flag,
        selection.as_ref().map(|s| s.workspace.id.as_str()),
        &mut Ask::Never,
    )?;
    let selected_project = select::selected_project_in(selection.as_ref(), &workspace);
    let is_selected = |project_id: &str| selected_project.is_some_and(|p| p.id == project_id);
    let projects = api.list_projects(&workspace.id)?;

    if json {
        let doc: Vec<_> = projects
            .iter()
            .map(|p| {
                serde_json::json!({
                    "id": p.id,
                    "workspace_id": p.workspace_id,
                    "name": p.name,
                    "archived_at": p.archived_at.map(|t| t.to_rfc3339()),
                    "selected": is_selected(&p.id),
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
                selected_mark(is_selected(&p.id)),
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

fn use_project(
    session: &PlatformSession,
    workspace_flag: Option<&str>,
    project_flag: Option<&str>,
    ask: &mut Ask,
) -> Result<()> {
    let mut api = session.api()?;
    // A selection that cannot be read is replaced, so it is no default.
    let current = session.selection().ok().flatten();
    let workspace = select::resolve_workspace(
        &mut api,
        workspace_flag,
        current.as_ref().map(|s| s.workspace.id.as_str()),
        ask,
    )?;
    let title = format!("Project of {}", workspace.name);
    let question = Question {
        title: &title,
        flag: project_flag,
        default_id: None,
        current_id: select::selected_project_in(current.as_ref(), &workspace)
            .map(|p| p.id.as_str()),
        decline: None,
        remedy: "name it: `peppy platform project use <id|name>`",
    };
    let project = select::pick_project(&mut api, &workspace, question, ask)?.required("project")?;
    let selection = save_selection(
        session,
        &mut api,
        (&workspace).into(),
        Some((&project).into()),
    )?;
    print!("{}", selection_report(session, &selection));
    Ok(())
}

fn clear(session: &PlatformSession) -> Result<()> {
    let Some(mut selection) = session.selection()? else {
        println!("No project is selected.");
        return Ok(());
    };
    if selection.project.take().is_none() {
        println!("No project is selected.");
        return Ok(());
    }
    selection.selected_at = auth::storage::now_unix();
    auth::selection::save(&session.dirs, &selection)?;
    println!(
        "No project is selected; {} stays selected.",
        selection.workspace_label()
    );
    Ok(())
}
