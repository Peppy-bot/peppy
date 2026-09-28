//! Resolving which workspace and project a command acts on. A flag names one
//! by id or exact name; without a flag the only candidate is picked, and any
//! other count is an error that lists the choices, so a script never lands on
//! a guessed project.
//!
//! A command on an existing router (`peers`, `router`) resolves a [`Target`]:
//! the flags when there are flags, else the project this machine is enrolled
//! in, else nothing. One project has one router, so the project names it.

use auth::client::{self, Project, Workspace};
use auth::enrollment::EnrollmentDocument;
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

/// The project whose router a command acts on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Target {
    pub workspace_id: String,
    pub project_id: String,
    /// How to name the project to the person.
    pub label: String,
}

/// Where a [`Target`] comes from, decided from the flags and the enrollment
/// alone, before any call to the platform.
#[derive(Debug, PartialEq, Eq)]
enum TargetSource<'a> {
    /// At least one flag was given. The flags always win.
    Flags,
    /// No flag, and this machine is enrolled.
    Enrolled(&'a EnrollmentDocument),
    /// No flag and no enrollment: the command has no router to act on.
    Unnamed,
}

fn target_source<'a>(
    workspace_flag: Option<&str>,
    project_flag: Option<&str>,
    enrollment: Option<&'a EnrollmentDocument>,
) -> TargetSource<'a> {
    if workspace_flag.is_some() || project_flag.is_some() {
        return TargetSource::Flags;
    }
    match enrollment {
        Some(document) => TargetSource::Enrolled(document),
        None => TargetSource::Unnamed,
    }
}

/// Resolves the project a command acts on. With no flag and no enrollment the
/// command stops and lists the projects: it never selects a project for the
/// person, because the commands that use this act on every peer of a router.
pub(crate) fn resolve_target(
    http: &HttpClient,
    api_url: &str,
    cred: &mut auth::Credential,
    workspace_flag: Option<&str>,
    project_flag: Option<&str>,
    enrollment: Option<&EnrollmentDocument>,
) -> Result<Target> {
    match target_source(workspace_flag, project_flag, enrollment) {
        TargetSource::Flags => {
            let selection = resolve_project(http, api_url, cred, workspace_flag, project_flag)?;
            Ok(Target {
                label: format!(
                    "project {} ({}) in workspace {}",
                    selection.project.name, selection.project.id, selection.workspace.name
                ),
                workspace_id: selection.workspace.id,
                project_id: selection.project.id,
            })
        }
        TargetSource::Enrolled(document) => Ok(Target {
            workspace_id: document.workspace_id.clone(),
            project_id: document.project_id.clone(),
            label: format!(
                "project {} (the project this machine is enrolled in)",
                document.project_id
            ),
        }),
        TargetSource::Unnamed => {
            // The list only helps the person to name a project. When it cannot
            // be read, the error still says what to pass.
            let listing = project_listing(http, api_url, cred)
                .unwrap_or_else(|error| format!("  (the list could not be read: {error})"));
            Err(Error::ExecutionFailed(format!(
                "this machine is not enrolled, so no project is selected; pass --project \
                 <id|name> (and --workspace <id|name>) to name one. The projects you can use \
                 are:\n{listing}"
            )))
        }
    }
}

/// Every project of every workspace of the account that is not archived, one
/// per line, for an error that asks the person to name one.
fn project_listing(
    http: &HttpClient,
    api_url: &str,
    cred: &mut auth::Credential,
) -> Result<String> {
    let mut lines = Vec::new();
    for workspace in client::list_workspaces(http, api_url, cred)? {
        for project in client::list_projects(http, api_url, cred, &workspace.id)? {
            if project.is_archived() {
                continue;
            }
            lines.push(format!(
                "  {}  {}  (workspace {})",
                project.id, project.name, workspace.name
            ));
        }
    }
    if lines.is_empty() {
        return Ok("  (none; create one in the web app first)".to_string());
    }
    Ok(lines.join("\n"))
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

    fn enrolled() -> EnrollmentDocument {
        EnrollmentDocument {
            version: auth::enrollment::ENROLLMENT_VERSION,
            api_url: "https://api.example".into(),
            workspace_id: "ws-1".into(),
            project_id: "p-1".into(),
            peer_id: "peer-1".into(),
            peer_name: "robot-7".into(),
            zenoh_id: pmi::RouterId::parse("7f3a9c1e").unwrap(),
            namespace: config::namespace::Namespace::parse("p-1").unwrap(),
            router: auth::RouterEndpoint::parse("rtr.example", 7447).unwrap(),
            certificate_expires_at: 2_000_000_000,
            enrolled_at: 1_700_000_000,
        }
    }

    /// The rule that names the router: a flag always wins, the enrollment is
    /// the default, and with neither the command has no target.
    #[test]
    fn the_flags_win_then_the_enrollment_then_nothing() {
        let document = enrolled();
        for (workspace, project) in [
            (Some("ws"), Some("p")),
            (None, Some("p")),
            (Some("ws"), None),
        ] {
            assert_eq!(
                target_source(workspace, project, Some(&document)),
                TargetSource::Flags,
                "flags win over an enrollment"
            );
            assert_eq!(target_source(workspace, project, None), TargetSource::Flags);
        }
        assert_eq!(
            target_source(None, None, Some(&document)),
            TargetSource::Enrolled(&document)
        );
        assert_eq!(target_source(None, None, None), TargetSource::Unnamed);
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
