//! Resolving which workspace and project a command acts on. A flag names one
//! by id or exact name; without a flag the only candidate is picked, and any
//! other count is an error that lists the choices, so a script never lands on
//! a guessed project.

use auth::client::{self, Project, Workspace};
use auth::http::HttpClient;

use crate::error::{Error, Result};

/// The workspace and project a command resolved.
pub(crate) struct Selection {
    pub workspace: Workspace,
    pub project: Project,
}

pub(crate) fn resolve_workspace(
    http: &HttpClient,
    api_url: &str,
    cred: &mut auth::Credential,
    flag: Option<&str>,
) -> Result<Workspace> {
    let workspaces = client::list_workspaces(http, api_url, cred)?;
    pick(workspaces, flag, "workspace", |w| (&w.id, &w.name))
}

pub(crate) fn resolve_project(
    http: &HttpClient,
    api_url: &str,
    cred: &mut auth::Credential,
    workspace_flag: Option<&str>,
    project_flag: Option<&str>,
) -> Result<Selection> {
    let workspace = resolve_workspace(http, api_url, cred, workspace_flag)?;
    let projects: Vec<Project> = client::list_projects(http, api_url, cred, &workspace.id)?
        .into_iter()
        .filter(|project| !project.is_archived())
        .collect();
    let project = pick(projects, project_flag, "project", |p| (&p.id, &p.name))?;
    Ok(Selection { workspace, project })
}

/// Picks one candidate: the one `flag` names by id or exact name, else the
/// only one there is.
fn pick<T>(
    candidates: Vec<T>,
    flag: Option<&str>,
    what: &str,
    id_and_name: impl Fn(&T) -> (&String, &String),
) -> Result<T> {
    let choices = || {
        candidates
            .iter()
            .map(|c| {
                let (id, name) = id_and_name(c);
                format!("  {id}  {name}")
            })
            .collect::<Vec<_>>()
            .join("\n")
    };
    if let Some(flag) = flag {
        let listing = choices();
        return candidates
            .into_iter()
            .find(|c| {
                let (id, name) = id_and_name(c);
                id == flag || name == flag
            })
            .ok_or_else(|| {
                Error::ExecutionFailed(format!(
                    "no {what} with id or name {flag:?}; the {what}s you can use are:\n{listing}"
                ))
            });
    }
    match candidates.len() {
        1 => Ok(candidates.into_iter().next().expect("one candidate")),
        0 => Err(Error::ExecutionFailed(format!(
            "no {what} is available to this account; create one in the web app first"
        ))),
        _ => Err(Error::ExecutionFailed(format!(
            "more than one {what} is available; pass --{what} <id|name> to pick one:\n{}",
            choices()
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn named(id: &str, name: &str) -> (String, String) {
        (id.to_string(), name.to_string())
    }

    #[test]
    fn a_flag_picks_by_id_or_exact_name() {
        let candidates = vec![named("id-1", "Lab"), named("id-2", "Field")];
        let by_id = pick(candidates.clone(), Some("id-2"), "project", |c| {
            (&c.0, &c.1)
        })
        .unwrap();
        assert_eq!(by_id.1, "Field");
        let by_name = pick(candidates.clone(), Some("Lab"), "project", |c| (&c.0, &c.1)).unwrap();
        assert_eq!(by_name.0, "id-1");
        let err = pick(candidates, Some("lab"), "project", |c| (&c.0, &c.1)).unwrap_err();
        assert!(
            err.to_string().contains("id-1  Lab"),
            "lists the choices: {err}"
        );
    }

    #[test]
    fn without_a_flag_only_a_single_candidate_is_picked() {
        let one = vec![named("id-1", "Lab")];
        assert_eq!(
            pick(one, None, "project", |c| (&c.0, &c.1)).unwrap().0,
            "id-1"
        );

        let none: Vec<(String, String)> = Vec::new();
        assert!(
            pick(none, None, "workspace", |c| (&c.0, &c.1))
                .unwrap_err()
                .to_string()
                .contains("no workspace")
        );

        let two = vec![named("id-1", "Lab"), named("id-2", "Field")];
        let err = pick(two, None, "project", |c| (&c.0, &c.1)).unwrap_err();
        assert!(err.to_string().contains("--project"), "{err}");
    }
}
