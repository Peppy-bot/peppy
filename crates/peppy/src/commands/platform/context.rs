//! `peppy platform context`: the workspace and the project the platform
//! commands act on. `show` prints the context, `list` prints every workspace
//! and project with the current ones marked, `use` switches, and `clear`
//! removes the context.
//!
//! The context is for the CLI only. A switch does not move this machine to a
//! different project and does not restart the daemon: that is `peppy platform
//! enroll --replace`.

use std::sync::Arc;

use clap::Subcommand;
use daemon_config::consts::PeppyDirs;

use crate::commands::Command;
use crate::commands::platform::PlatformSession;
use crate::commands::platform::configure::{select_and_save, selected_report};
use crate::commands::platform::select::Ask;
use crate::context::AppContext;
use crate::error::Result;
use auth::PlatformContext;
use auth::client::{self, Project, Workspace};
use auth::enrollment::EnrollmentDocument;

#[derive(Subcommand)]
pub enum ContextCommands {
    /// Show the selected workspace and project
    Show {
        #[arg(long = "api-url")]
        api_url: Option<String>,
        /// Emit machine-readable JSON.
        #[arg(long)]
        json: bool,
    },
    /// List every workspace and project, the selected ones marked
    List {
        #[arg(long = "api-url")]
        api_url: Option<String>,
        /// Emit machine-readable JSON.
        #[arg(long)]
        json: bool,
    },
    /// Switch to a different workspace and project
    Use {
        #[arg(long = "api-url")]
        api_url: Option<String>,
        /// The workspace, by id or exact name (else you select it from a list).
        #[arg(long)]
        workspace: Option<String>,
        /// The project, by id or exact name (else you select it from a list).
        #[arg(long)]
        project: Option<String>,
    },
    /// Remove the context
    Clear,
}

/// What the command does.
pub enum ContextAction {
    Show {
        json: bool,
    },
    List {
        json: bool,
    },
    Use {
        workspace: Option<String>,
        project: Option<String>,
    },
    Clear,
}

pub struct ContextCommand {
    pub action: ContextAction,
    pub api_url: Option<String>,
    /// How the selection asks the person: on the terminal, from a supplied
    /// reader, or not at all.
    pub ask: Ask,
    /// Test seam: override the peppy data dirs.
    pub peppy_dirs: Option<PeppyDirs>,
}

impl From<ContextCommands> for ContextCommand {
    fn from(command: ContextCommands) -> Self {
        let (action, api_url) = match command {
            ContextCommands::Show { api_url, json } => (ContextAction::Show { json }, api_url),
            ContextCommands::List { api_url, json } => (ContextAction::List { json }, api_url),
            ContextCommands::Use {
                api_url,
                workspace,
                project,
            } => (ContextAction::Use { workspace, project }, api_url),
            ContextCommands::Clear => (ContextAction::Clear, None),
        };
        Self {
            action,
            api_url,
            ask: Ask::Terminal,
            peppy_dirs: None,
        }
    }
}

impl Command for ContextCommand {
    fn execute(mut self, _ctx: &Arc<AppContext>) -> Result<()> {
        let session = PlatformSession::resolve(self.peppy_dirs, self.api_url.as_deref())?;
        match self.action {
            ContextAction::Show { json } => show(&session, json),
            ContextAction::List { json } => list(&session, json),
            ContextAction::Use { workspace, project } => {
                let mut cred = session.credential()?;
                let context = select_and_save(
                    &session,
                    &mut cred,
                    workspace.as_deref(),
                    project.as_deref(),
                    &mut self.ask,
                )?;
                print!("{}", selected_report(&session, &context));
                Ok(())
            }
            ContextAction::Clear => {
                auth::context::remove(&session.dirs)?;
                println!("The context is removed.");
                Ok(())
            }
        }
    }
}

fn show(session: &PlatformSession, json: bool) -> Result<()> {
    let context = session.context()?;
    if json {
        println!("{}", context_json(context.as_ref()));
        return Ok(());
    }
    let Some(context) = context else {
        println!("No context. Run `peppy platform configure` to select a project.");
        return Ok(());
    };
    print!("{}", selected_report(session, &context));
    Ok(())
}

fn list(session: &PlatformSession, json: bool) -> Result<()> {
    let mut cred = session.credential()?;
    let context = session.context()?;
    let mut tree = Vec::new();
    for workspace in client::list_workspaces(&session.http, &session.api_url, &mut cred)? {
        let projects: Vec<Project> =
            client::list_projects(&session.http, &session.api_url, &mut cred, &workspace.id)?
                .into_iter()
                .filter(|project| !project.is_archived())
                .collect();
        tree.push((workspace, projects));
    }
    if json {
        println!("{}", tree_json(&tree, context.as_ref()));
        return Ok(());
    }
    print!("{}", render_tree(&tree, context.as_ref()));
    Ok(())
}

/// The note for a machine that is enrolled in a different project than the
/// context names, or `None` when there is nothing to say.
pub(crate) fn enrollment_note(
    context: &PlatformContext,
    enrollment: Option<&EnrollmentDocument>,
) -> Option<String> {
    let enrollment = enrollment.filter(|document| document.project_id != context.project.id)?;
    Some(format!(
        "This machine is enrolled in project {}. The context does not move it. To move it: \
         `peppy platform enroll --replace`",
        enrollment.project_id
    ))
}

/// The context as JSON, `null` when there is none.
pub(crate) fn context_json(context: Option<&PlatformContext>) -> serde_json::Value {
    let Some(context) = context else {
        return serde_json::Value::Null;
    };
    serde_json::json!({
        "workspace": { "id": context.workspace.id, "name": context.workspace.name },
        "project": { "id": context.project.id, "name": context.project.name },
        "selected_at": context.selected_at,
    })
}

/// Every workspace with its projects. A `*` marks the workspace and the
/// project of the context.
fn render_tree(tree: &[(Workspace, Vec<Project>)], context: Option<&PlatformContext>) -> String {
    if tree.is_empty() {
        return "No workspaces; create one in the web app first.\n".to_string();
    }
    let mark = |current: bool| if current { "*" } else { " " };
    let mut out = String::new();
    for (workspace, projects) in tree {
        let in_workspace = context.is_some_and(|c| c.workspace.id == workspace.id);
        out.push_str(&format!(
            "{} {} ({})\n",
            mark(in_workspace),
            workspace.name,
            workspace.id
        ));
        if projects.is_empty() {
            out.push_str("      (no projects)\n");
        }
        for project in projects {
            let current = in_workspace && context.is_some_and(|c| c.project.id == project.id);
            out.push_str(&format!(
                "    {} {} ({})\n",
                mark(current),
                project.name,
                project.id
            ));
        }
    }
    out
}

fn tree_json(
    tree: &[(Workspace, Vec<Project>)],
    context: Option<&PlatformContext>,
) -> serde_json::Value {
    let workspaces: Vec<_> = tree
        .iter()
        .map(|(workspace, projects)| {
            let in_workspace = context.is_some_and(|c| c.workspace.id == workspace.id);
            serde_json::json!({
                "id": workspace.id,
                "name": workspace.name,
                "current": in_workspace,
                "projects": projects.iter().map(|project| serde_json::json!({
                    "id": project.id,
                    "name": project.name,
                    "current": in_workspace
                        && context.is_some_and(|c| c.project.id == project.id),
                })).collect::<Vec<_>>(),
            })
        })
        .collect();
    serde_json::Value::Array(workspaces)
}

#[cfg(test)]
mod tests {
    use super::*;
    use auth::context::{CONTEXT_VERSION, Named};

    fn context() -> PlatformContext {
        PlatformContext {
            version: CONTEXT_VERSION,
            api_origin: "https://api.example".into(),
            subject: "user-123".into(),
            workspace: Named {
                id: "ws-2".into(),
                name: "Robotics lab".into(),
            },
            project: Named {
                id: "p-2".into(),
                name: "Field".into(),
            },
            selected_at: 1_700_000_000,
        }
    }

    fn enrolled_in(project_id: &str) -> EnrollmentDocument {
        EnrollmentDocument {
            version: auth::enrollment::ENROLLMENT_VERSION,
            api_url: "https://api.example".into(),
            workspace_id: "ws-2".into(),
            project_id: project_id.into(),
            peer_id: "peer-1".into(),
            peer_name: "robot-7".into(),
            zenoh_id: pmi::RouterId::parse("7f3a9c1e").unwrap(),
            namespace: config::namespace::Namespace::parse(project_id).unwrap(),
            router: auth::RouterEndpoint::parse("rtr.example", 7447).unwrap(),
            certificate_issued_at: 1_700_000_000,
            certificate_expires_at: 2_000_000_000,
            enrolled_at: 1_700_000_000,
        }
    }

    fn workspace(id: &str, name: &str) -> Workspace {
        Workspace {
            id: id.into(),
            name: name.into(),
            tier: "free".into(),
        }
    }

    fn project(id: &str, workspace_id: &str, name: &str) -> Project {
        Project {
            id: id.into(),
            workspace_id: workspace_id.into(),
            name: name.into(),
            archived_at: None,
        }
    }

    #[test]
    fn the_note_is_only_for_a_machine_enrolled_in_a_different_project() {
        assert_eq!(enrollment_note(&context(), None), None);
        assert_eq!(enrollment_note(&context(), Some(&enrolled_in("p-2"))), None);
        let note = enrollment_note(&context(), Some(&enrolled_in("p-1"))).expect("a note");
        assert!(note.contains("enrolled in project p-1"), "{note}");
        assert!(note.contains("peppy platform enroll --replace"), "{note}");
    }

    #[test]
    fn the_tree_marks_the_workspace_and_the_project_of_the_context() {
        let tree = vec![
            (
                workspace("ws-1", "Alice's workspace"),
                // The same project id in a different workspace is not the
                // project of the context.
                vec![project("p-2", "ws-1", "Demo")],
            ),
            (
                workspace("ws-2", "Robotics lab"),
                vec![
                    project("p-1", "ws-2", "Lab"),
                    project("p-2", "ws-2", "Field"),
                ],
            ),
            (workspace("ws-3", "Empty"), Vec::new()),
        ];
        assert_eq!(
            render_tree(&tree, Some(&context())),
            "  Alice's workspace (ws-1)\n\
             \x20     Demo (p-2)\n\
             * Robotics lab (ws-2)\n\
             \x20     Lab (p-1)\n\
             \x20   * Field (p-2)\n\
             \x20 Empty (ws-3)\n\
             \x20     (no projects)\n"
        );
        assert!(!render_tree(&tree, None).contains('*'));
    }

    #[test]
    fn the_json_forms_carry_the_same_marks() {
        assert_eq!(context_json(None), serde_json::Value::Null);
        assert_eq!(context_json(Some(&context()))["project"]["id"], "p-2");

        let tree = vec![(
            workspace("ws-2", "Robotics lab"),
            vec![
                project("p-1", "ws-2", "Lab"),
                project("p-2", "ws-2", "Field"),
            ],
        )];
        let doc = tree_json(&tree, Some(&context()));
        assert_eq!(doc[0]["current"], true);
        assert_eq!(doc[0]["projects"][0]["current"], false);
        assert_eq!(doc[0]["projects"][1]["current"], true);
    }
}
