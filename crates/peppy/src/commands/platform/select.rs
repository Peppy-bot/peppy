//! Resolving which workspace and project a command acts on.
//!
//! One candidate is picked in this order: the flag (by id or exact name), the
//! default the caller gives (the context), the only candidate there is, the
//! answer of the person when the caller may ask. Any other case is an error
//! that lists the choices, so a script never lands on a guessed project.
//!
//! A command on an existing router (`peers`, `router`) resolves a [`Target`]:
//! the flags, else the context, else the project this machine is enrolled in,
//! else nothing. One project has one router, so the project names it.

use std::io::{BufRead, IsTerminal};

use auth::client::{self, Project, Workspace};
use auth::context::{CONTEXT_VERSION, Named, PlatformContext, project_label};
use auth::enrollment::EnrollmentDocument;
use auth::http::HttpClient;
use auth::{AuthError, storage};

use crate::commands::confirm::choose_prompt;
use crate::commands::platform::PlatformSession;
use crate::error::{Error, Result};

/// The workspace and project a command resolved.
pub(crate) struct Selection {
    pub workspace: Workspace,
    pub project: Project,
}

/// Whether a resolution may ask the person when more than one candidate is
/// left, and where it reads the answer.
pub enum Ask {
    /// Do not ask: stop with the list of the choices.
    Never,
    /// Ask on the terminal. With no terminal on stdin this is [`Ask::Never`].
    Terminal,
    /// Read the answers from this reader.
    Reader(Box<dyn BufRead>),
}

pub(crate) fn resolve_workspace(
    http: &HttpClient,
    api_url: &str,
    cred: &mut auth::Credential,
    flag: Option<&str>,
    default_workspace_id: Option<&str>,
    ask: &mut Ask,
) -> Result<Workspace> {
    let workspaces = client::list_workspaces(http, api_url, cred)?;
    pick(
        Candidates {
            what: "workspace",
            title: "Workspaces".to_string(),
            items: workspaces,
            id_and_name: |w: &Workspace| (&w.id, &w.name),
            option: |w: &Workspace| match w.tier.as_str() {
                "" => w.name.clone(),
                tier => format!("{}   ({tier})", w.name),
            },
        },
        flag,
        default_workspace_id,
        ask,
    )
}

pub(crate) fn resolve_project(
    http: &HttpClient,
    api_url: &str,
    cred: &mut auth::Credential,
    workspace_flag: Option<&str>,
    project_flag: Option<&str>,
    context: Option<&PlatformContext>,
    ask: &mut Ask,
) -> Result<Selection> {
    let workspace = resolve_workspace(
        http,
        api_url,
        cred,
        workspace_flag,
        context.map(|context| context.workspace.id.as_str()),
        ask,
    )?;
    // The project of the context is the default only in the workspace of the
    // context: in a different workspace its id names nothing.
    let default_project_id = context
        .filter(|context| context.workspace.id == workspace.id)
        .map(|context| context.project.id.as_str());
    let project = resolve_project_in(
        http,
        api_url,
        cred,
        &workspace,
        project_flag,
        default_project_id,
        ask,
    )?;
    Ok(Selection { workspace, project })
}

/// Picks one project of `workspace`.
fn resolve_project_in(
    http: &HttpClient,
    api_url: &str,
    cred: &mut auth::Credential,
    workspace: &Workspace,
    flag: Option<&str>,
    default_project_id: Option<&str>,
    ask: &mut Ask,
) -> Result<Project> {
    let projects = active_projects(http, api_url, cred, &workspace.id)?;
    pick(
        Candidates {
            what: "project",
            title: format!("Projects of {}", workspace.name),
            items: projects,
            id_and_name: |p: &Project| (&p.id, &p.name),
            option: |p: &Project| p.name.clone(),
        },
        flag,
        default_project_id,
        ask,
    )
}

/// Runs the selection and returns the context it makes, for the caller to
/// save. The flags name the workspace and the project; what no flag names is
/// the only candidate, or the answer of the person.
///
/// A selection is how the person changes the context, so the project of the
/// stored context is never a default here. Its workspace is one in a single
/// case: `--project` alone names a project of the workspace the person works
/// in.
pub(crate) fn select_context(
    session: &PlatformSession,
    cred: &mut auth::Credential,
    workspace_flag: Option<&str>,
    project_flag: Option<&str>,
    ask: &mut Ask,
) -> Result<PlatformContext> {
    let subject = match session.subject() {
        Some(subject) => subject,
        None => client::get_me(&session.http, &session.api_url, cred)?.sub,
    };
    let current = session.context().ok().flatten();
    let default_workspace_id = current
        .as_ref()
        .filter(|_| workspace_flag.is_none() && project_flag.is_some())
        .map(|context| context.workspace.id.as_str());
    let workspace = resolve_workspace(
        &session.http,
        &session.api_url,
        cred,
        workspace_flag,
        default_workspace_id,
        ask,
    )?;
    let project = resolve_project_in(
        &session.http,
        &session.api_url,
        cred,
        &workspace,
        project_flag,
        None,
        ask,
    )?;
    let selection = Selection { workspace, project };
    Ok(PlatformContext {
        version: CONTEXT_VERSION,
        api_origin: session.api_origin()?,
        subject,
        workspace: Named {
            id: selection.workspace.id,
            name: selection.workspace.name,
        },
        project: Named {
            id: selection.project.id,
            name: selection.project.name,
        },
        selected_at: storage::now_unix(),
    })
}

/// What named the project of a [`Target`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TargetSource {
    Flags,
    Context,
    Enrollment,
}

/// The project whose router a command acts on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Target {
    pub workspace_id: String,
    pub project_id: String,
    /// How to name the project to the person.
    pub label: String,
    pub source: TargetSource,
}

/// Where a [`Target`] comes from, decided from the flags, the context and the
/// enrollment alone, before any call to the platform.
#[derive(Debug, PartialEq, Eq)]
enum TargetDecision<'a> {
    /// At least one flag was given. The flags always win.
    Flags,
    /// No flag, and the person selected a context.
    Context(&'a PlatformContext),
    /// No flag and no context, and this machine is enrolled.
    Enrolled(&'a EnrollmentDocument),
    /// Nothing names a project: the command has no router to act on.
    Unnamed,
}

fn decide_target<'a>(
    workspace_flag: Option<&str>,
    project_flag: Option<&str>,
    context: Option<&'a PlatformContext>,
    enrollment: Option<&'a EnrollmentDocument>,
) -> TargetDecision<'a> {
    if workspace_flag.is_some() || project_flag.is_some() {
        return TargetDecision::Flags;
    }
    if let Some(context) = context {
        return TargetDecision::Context(context);
    }
    match enrollment {
        Some(document) => TargetDecision::Enrolled(document),
        None => TargetDecision::Unnamed,
    }
}

/// Resolves the project a command acts on. When nothing names one the command
/// stops and lists the projects: it never selects a project for the person,
/// because the commands that use this act on every peer of a router.
pub(crate) fn resolve_target(
    http: &HttpClient,
    api_url: &str,
    cred: &mut auth::Credential,
    workspace_flag: Option<&str>,
    project_flag: Option<&str>,
    context: Option<&PlatformContext>,
    enrollment: Option<&EnrollmentDocument>,
) -> Result<Target> {
    match decide_target(workspace_flag, project_flag, context, enrollment) {
        TargetDecision::Flags => {
            let selection = resolve_project(
                http,
                api_url,
                cred,
                workspace_flag,
                project_flag,
                context,
                &mut Ask::Never,
            )?;
            Ok(Target {
                label: project_label(
                    &selection.project.name,
                    &selection.project.id,
                    &selection.workspace.name,
                ),
                workspace_id: selection.workspace.id,
                project_id: selection.project.id,
                source: TargetSource::Flags,
            })
        }
        TargetDecision::Context(context) => Ok(Target {
            workspace_id: context.workspace.id.clone(),
            project_id: context.project.id.clone(),
            label: format!("{} (the context)", context.label()),
            source: TargetSource::Context,
        }),
        TargetDecision::Enrolled(document) => Ok(Target {
            workspace_id: document.workspace_id.clone(),
            project_id: document.project_id.clone(),
            label: format!(
                "project {} (the project this machine is enrolled in)",
                document.project_id
            ),
            source: TargetSource::Enrollment,
        }),
        TargetDecision::Unnamed => {
            // The list only helps the person to name a project. When it cannot
            // be read, the error still says what to do.
            let listing = project_listing(http, api_url, cred)
                .unwrap_or_else(|error| format!("  (the list could not be read: {error})"));
            Err(Error::ExecutionFailed(format!(
                "no project is selected: run `peppy platform configure` to select one, or pass \
                 --project <id|name> (and --workspace <id|name>). The projects you can use \
                 are:\n{listing}"
            )))
        }
    }
}

/// The error for a call the platform refused on `target`. A context holds ids
/// from the time of the selection, so a `403` or a `404` on a target that came
/// from the context can mean that the project was archived or that the person
/// lost access: the refusal then names the command that selects again.
pub(crate) fn refusal_on(target: &Target, error: AuthError) -> Error {
    match &error {
        AuthError::Problem(problem)
            if target.source == TargetSource::Context && matches!(problem.status, 403 | 404) =>
        {
            Error::Auth(format!(
                "{problem}. The context can be out of date. Run `peppy platform configure`."
            ))
        }
        _ => Error::AuthEngine(error),
    }
}

/// The projects of `workspace_id` that are not archived. An archived project
/// owns no router, so no command acts on it.
fn active_projects(
    http: &HttpClient,
    api_url: &str,
    cred: &mut auth::Credential,
    workspace_id: &str,
) -> Result<Vec<Project>> {
    Ok(client::list_projects(http, api_url, cred, workspace_id)?
        .into_iter()
        .filter(|project| !project.is_archived())
        .collect())
}

/// Every workspace of the account with its projects that are not archived,
/// in the order the platform lists them.
pub(crate) fn active_project_tree(
    http: &HttpClient,
    api_url: &str,
    cred: &mut auth::Credential,
) -> Result<Vec<(Workspace, Vec<Project>)>> {
    client::list_workspaces(http, api_url, cred)?
        .into_iter()
        .map(|workspace| {
            let projects = active_projects(http, api_url, cred, &workspace.id)?;
            Ok((workspace, projects))
        })
        .collect()
}

/// Every project of every workspace of the account that is not archived, one
/// per line, for an error that asks the person to name one.
fn project_listing(
    http: &HttpClient,
    api_url: &str,
    cred: &mut auth::Credential,
) -> Result<String> {
    let lines: Vec<String> = active_project_tree(http, api_url, cred)?
        .iter()
        .flat_map(|(workspace, projects)| {
            projects.iter().map(move |project| {
                format!(
                    "  {}  {}  (workspace {})",
                    project.id, project.name, workspace.name
                )
            })
        })
        .collect();
    if lines.is_empty() {
        return Ok("  (none; create one in the web app first)".to_string());
    }
    Ok(lines.join("\n"))
}

/// The candidates of one pick, with how to read and how to show each.
struct Candidates<T, I, O>
where
    I: Fn(&T) -> (&String, &String),
    O: Fn(&T) -> String,
{
    /// The kind of thing, as the flag spells it: `workspace` or `project`.
    what: &'static str,
    /// The heading of the list the person selects from.
    title: String,
    items: Vec<T>,
    id_and_name: I,
    /// One candidate as a line of the list.
    option: O,
}

/// Picks one candidate, in the order of the module docs.
fn pick<T, I, O>(
    candidates: Candidates<T, I, O>,
    flag: Option<&str>,
    default_id: Option<&str>,
    ask: &mut Ask,
) -> Result<T>
where
    I: Fn(&T) -> (&String, &String),
    O: Fn(&T) -> String,
{
    let Candidates {
        what,
        title,
        items,
        id_and_name,
        option,
    } = candidates;
    let listing = items
        .iter()
        .map(|item| {
            let (id, name) = id_and_name(item);
            format!("  {id}  {name}")
        })
        .collect::<Vec<_>>()
        .join("\n");

    if let Some(flag) = flag {
        return items
            .into_iter()
            .find(|item| {
                let (id, name) = id_and_name(item);
                id == flag || name == flag
            })
            .ok_or_else(|| {
                Error::ExecutionFailed(format!(
                    "no {what} with id or name {flag:?}; the {what}s you can use are:\n{listing}"
                ))
            });
    }
    if let Some(default_id) = default_id {
        return items
            .into_iter()
            .find(|item| id_and_name(item).0 == default_id)
            .ok_or_else(|| {
                Error::ExecutionFailed(format!(
                    "the {what} of the context ({default_id}) is not one you can use; run \
                     `peppy platform configure` to select again. The {what}s you can use \
                     are:\n{listing}"
                ))
            });
    }
    match items.len() {
        0 => Err(Error::ExecutionFailed(format!(
            "no {what} is available to this account; create one in the web app first"
        ))),
        1 => Ok(items.into_iter().next().expect("one candidate")),
        _ => {
            let options: Vec<String> = items.iter().map(&option).collect();
            let index = match ask {
                Ask::Reader(reader) => Some(choose_prompt(
                    &title,
                    what,
                    &options,
                    Some(reader.as_mut()),
                )?),
                Ask::Terminal if std::io::stdin().is_terminal() => {
                    Some(choose_prompt(&title, what, &options, None)?)
                }
                Ask::Terminal | Ask::Never => None,
            };
            let Some(index) = index else {
                return Err(Error::ExecutionFailed(format!(
                    "more than one {what} is available; run `peppy platform configure` to \
                     select one, or pass --{what} <id|name>:\n{listing}"
                )));
            };
            Ok(items.into_iter().nth(index).expect("an index of the list"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use auth::ProblemKind;
    use auth::test_support::{enrollment_document as enrolled, platform_context as context};
    use std::io::Cursor;

    type Pair = (String, String);

    fn named(id: &str, name: &str) -> Pair {
        (id.to_string(), name.to_string())
    }

    fn candidates(
        items: Vec<Pair>,
    ) -> Candidates<Pair, impl Fn(&Pair) -> (&String, &String), impl Fn(&Pair) -> String> {
        Candidates {
            what: "project",
            title: "Projects".to_string(),
            items,
            id_and_name: |c: &Pair| (&c.0, &c.1),
            option: |c: &Pair| c.1.clone(),
        }
    }

    fn two() -> Vec<Pair> {
        vec![named("id-1", "Lab"), named("id-2", "Field")]
    }

    /// The rule that names the router: a flag always wins, then the context,
    /// then the enrollment, and with none of them the command has no target.
    #[test]
    fn the_flags_win_then_the_context_then_the_enrollment_then_nothing() {
        let (context, document) = (context(), enrolled());
        for (workspace, project) in [
            (Some("ws"), Some("p")),
            (None, Some("p")),
            (Some("ws"), None),
        ] {
            assert_eq!(
                decide_target(workspace, project, Some(&context), Some(&document)),
                TargetDecision::Flags,
                "flags win over a context and an enrollment"
            );
            assert_eq!(
                decide_target(workspace, project, None, None),
                TargetDecision::Flags
            );
        }
        assert_eq!(
            decide_target(None, None, Some(&context), Some(&document)),
            TargetDecision::Context(&context),
            "the context wins over the enrollment"
        );
        assert_eq!(
            decide_target(None, None, None, Some(&document)),
            TargetDecision::Enrolled(&document)
        );
        assert_eq!(
            decide_target(None, None, None, None),
            TargetDecision::Unnamed
        );
    }

    #[test]
    fn a_flag_picks_by_id_or_exact_name() {
        let by_id = pick(candidates(two()), Some("id-2"), None, &mut Ask::Never).unwrap();
        assert_eq!(by_id.1, "Field");
        let by_name = pick(
            candidates(two()),
            Some("Lab"),
            Some("id-2"),
            &mut Ask::Never,
        )
        .unwrap();
        assert_eq!(by_name.0, "id-1", "the flag wins over the default");
        let err = pick(candidates(two()), Some("lab"), None, &mut Ask::Never).unwrap_err();
        assert!(
            err.to_string().contains("id-1  Lab"),
            "lists the choices: {err}"
        );
    }

    #[test]
    fn the_default_picks_by_id_and_a_default_that_is_gone_names_configure() {
        let picked = pick(candidates(two()), None, Some("id-2"), &mut Ask::Never).unwrap();
        assert_eq!(picked.1, "Field");
        let err = pick(candidates(two()), None, Some("id-9"), &mut Ask::Never).unwrap_err();
        let message = err.to_string();
        assert!(message.contains("peppy platform configure"), "{message}");
        assert!(message.contains("id-1  Lab"), "{message}");
    }

    #[test]
    fn with_no_flag_and_no_default_only_a_single_candidate_is_picked() {
        let one = vec![named("id-1", "Lab")];
        assert_eq!(
            pick(candidates(one), None, None, &mut Ask::Never)
                .unwrap()
                .0,
            "id-1"
        );

        let err = pick(candidates(Vec::new()), None, None, &mut Ask::Never).unwrap_err();
        assert!(err.to_string().contains("no project"), "{err}");

        let err = pick(candidates(two()), None, None, &mut Ask::Never).unwrap_err();
        let message = err.to_string();
        assert!(message.contains("--project"), "{message}");
        assert!(message.contains("peppy platform configure"), "{message}");
    }

    #[test]
    fn with_more_than_one_candidate_the_person_selects() {
        let answers = |text: &str| Ask::Reader(Box::new(Cursor::new(text.as_bytes().to_vec())));
        let picked = pick(candidates(two()), None, None, &mut answers("2\n")).unwrap();
        assert_eq!(picked.1, "Field");
        assert!(
            pick(candidates(two()), None, None, &mut answers("")).is_err(),
            "end of input selects nothing"
        );
    }

    fn target(source: TargetSource) -> Target {
        Target {
            workspace_id: "ws-2".into(),
            project_id: "p-2".into(),
            label: "project Field".into(),
            source,
        }
    }

    fn problem(status: u16) -> AuthError {
        AuthError::Problem(auth::Problem {
            title: "Not Found".into(),
            ..auth::test_support::problem(ProblemKind::Other("about:blank".into()), status)
        })
    }

    /// Only a `403` or a `404` on a target from the context gets the hint.
    #[test]
    fn a_refusal_on_a_context_target_names_configure() {
        for status in [403, 404] {
            let message = refusal_on(&target(TargetSource::Context), problem(status)).to_string();
            assert_eq!(
                message,
                "Not Found. The context can be out of date. Run `peppy platform configure`."
            );
        }
        assert_eq!(
            refusal_on(&target(TargetSource::Context), problem(422)).to_string(),
            "Not Found"
        );
        for source in [TargetSource::Flags, TargetSource::Enrollment] {
            assert_eq!(
                refusal_on(&target(source), problem(404)).to_string(),
                "Not Found"
            );
        }
        assert_eq!(
            refusal_on(
                &target(TargetSource::Context),
                AuthError::Http("boom".into())
            )
            .to_string(),
            "boom"
        );
    }
}
