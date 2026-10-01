//! Resolving which workspace and project a command acts on.
//!
//! A command that uses the selection as a default (`enroll`, `project list`,
//! a command whose flags name its project) picks one candidate in this order:
//! the flag (by id or exact name), the selected one, the only candidate there
//! is. A command that selects (`workspace use`, `project use`, `login`) picks
//! in this order: the flag, the only candidate, the answer of the person on a
//! menu that starts on the selected one. When more than one candidate is left
//! and there is nobody to ask, the command stops and lists the choices, so a
//! script never lands on a guessed project.
//!
//! A command on an existing router (`peers`, `router`) resolves a [`Target`]:
//! the flags, else the selected project, else the project this machine is
//! enrolled in, else nothing. One project has one router, so the project names
//! it.

use std::collections::VecDeque;

use auth::AuthError;
use auth::client::{PlatformApi, Project, Workspace};
use auth::enrollment::EnrollmentDocument;
use auth::selection::{Named, PlatformSelection, project_label};

use crate::commands::menu::{Menu, MenuAnswer, can_show_menu};
use crate::error::{Error, Result};

/// The workspace and project a command resolved.
pub(crate) struct ProjectInWorkspace {
    pub workspace: Workspace,
    pub project: Project,
}

/// Whether a pick may ask the person when more than one candidate is left,
/// and how.
pub enum Ask {
    /// Do not ask: stop with the list of the choices.
    Never,
    /// Ask on a menu on the terminal. With no terminal this is [`Ask::Never`].
    Terminal,
    /// Answer each menu with the next of these positions in its list, and
    /// cancel the menu when none is left. A test answers with this, with no
    /// terminal.
    Scripted(VecDeque<usize>),
}

impl Ask {
    /// Answers the menus with `positions`, in order. See [`Ask::Scripted`].
    pub fn scripted(positions: impl IntoIterator<Item = usize>) -> Self {
        Self::Scripted(positions.into_iter().collect())
    }

    /// The answer to `menu`, or `None` when there is nobody to ask.
    fn answer(&mut self, menu: &Menu) -> Result<Option<MenuAnswer>> {
        match self {
            Self::Never => Ok(None),
            Self::Terminal if can_show_menu() => menu.show().map(Some),
            Self::Terminal => Ok(None),
            Self::Scripted(positions) => match positions.pop_front() {
                None => Ok(Some(MenuAnswer::Cancelled)),
                Some(position) if position < menu.options.len() => {
                    Ok(Some(MenuAnswer::Selected(position)))
                }
                Some(position) => Err(Error::ExecutionFailed(format!(
                    "the scripted answer {position} is not a position of the menu {:?}",
                    menu.title
                ))),
            },
        }
    }
}

/// The answer to one question.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Picked<T> {
    One(T),
    /// The person declined the question: they selected the entry that
    /// declines it, or cancelled the menu.
    Declined,
}

impl<T> Picked<T> {
    /// The candidate, or the error of a question the person declined, for a
    /// command that cannot go on with no candidate.
    pub(crate) fn required(self, what: &str) -> Result<T> {
        match self {
            Self::One(candidate) => Ok(candidate),
            Self::Declined => Err(Error::ExecutionFailed(format!("no {what} was selected"))),
        }
    }
}

/// What a pick knows besides its candidates.
pub(crate) struct Question<'a> {
    /// The title of the menu: what the person selects, as a noun.
    pub title: &'a str,
    /// The id or the exact name the command was given.
    pub flag: Option<&'a str>,
    /// The id picked with no question when it is one of the candidates: the
    /// selected one, for a command that uses the selection as a default.
    pub default_id: Option<&'a str>,
    /// The id the menu starts on: the selected one, for a command that
    /// selects.
    pub current_id: Option<&'a str>,
    /// The label of a last menu entry that declines the question, or `None`
    /// for a question that has no such entry.
    pub decline: Option<&'a str>,
    /// What the person does when more than one candidate is left and there is
    /// nobody to ask, after "more than one workspace is available; ".
    pub remedy: &'a str,
}

/// The candidates of one pick, with how to read and how to show each.
struct Candidates<T> {
    /// The kind of thing, as the flag spells it: `workspace` or `project`.
    what: &'static str,
    items: Vec<T>,
    id_and_name: fn(&T) -> (&String, &String),
    /// One candidate as a line of the menu.
    option: fn(&T) -> String,
}

/// Picks one workspace of the account.
pub(crate) fn pick_workspace(
    api: &mut PlatformApi,
    question: Question,
    ask: &mut Ask,
) -> Result<Picked<Workspace>> {
    let candidates = Candidates {
        what: "workspace",
        items: api.list_workspaces()?,
        id_and_name: |w: &Workspace| (&w.id, &w.name),
        option: |w: &Workspace| match w.tier.as_str() {
            "" => w.name.clone(),
            tier => format!("{}   ({tier})", w.name),
        },
    };
    pick(candidates, question, ask)
}

/// Picks one project of `workspace` that is not archived.
pub(crate) fn pick_project(
    api: &mut PlatformApi,
    workspace: &Workspace,
    question: Question,
    ask: &mut Ask,
) -> Result<Picked<Project>> {
    let candidates = Candidates {
        what: "project",
        items: active_projects(api, &workspace.id)?,
        id_and_name: |p: &Project| (&p.id, &p.name),
        option: |p: &Project| p.name.clone(),
    };
    pick(candidates, question, ask)
}

/// The workspace a command uses: the flag, else `default_workspace_id`, else
/// the only one, else the answer of the person when `ask` allows it.
pub(crate) fn resolve_workspace(
    api: &mut PlatformApi,
    flag: Option<&str>,
    default_workspace_id: Option<&str>,
    ask: &mut Ask,
) -> Result<Workspace> {
    let question = Question {
        title: "Workspace",
        flag,
        default_id: default_workspace_id,
        current_id: None,
        decline: None,
        remedy: "run `peppy platform workspace use` to select one, or pass --workspace <id|name>",
    };
    pick_workspace(api, question, ask)?.required("workspace")
}

/// The workspace and the project a command uses, the selection as the
/// default of each.
pub(crate) fn resolve_project(
    api: &mut PlatformApi,
    workspace_flag: Option<&str>,
    project_flag: Option<&str>,
    selection: Option<&PlatformSelection>,
    ask: &mut Ask,
) -> Result<ProjectInWorkspace> {
    let workspace = resolve_workspace(
        api,
        workspace_flag,
        selection.map(|selection| selection.workspace.id.as_str()),
        ask,
    )?;
    let title = format!("Project of {}", workspace.name);
    let question = Question {
        title: &title,
        flag: project_flag,
        default_id: selected_project_in(selection, &workspace).map(|project| project.id.as_str()),
        current_id: None,
        decline: None,
        remedy: "run `peppy platform project use` to select one, or pass --project <id|name>",
    };
    let project = pick_project(api, &workspace, question, ask)?.required("project")?;
    Ok(ProjectInWorkspace { workspace, project })
}

/// The selected project when it is a project of `workspace`. The selected
/// project of a different workspace names nothing in this one.
pub(crate) fn selected_project_in<'a>(
    selection: Option<&'a PlatformSelection>,
    workspace: &Workspace,
) -> Option<&'a Named> {
    selection
        .filter(|selection| selection.workspace.id == workspace.id)
        .and_then(|selection| selection.project.as_ref())
}

/// What named the project of a [`Target`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TargetSource {
    Flags,
    Selection,
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

/// Where a [`Target`] comes from, decided from the flags, the selection and
/// the enrollment alone, before any call to the platform.
#[derive(Debug, PartialEq, Eq)]
enum TargetDecision<'a> {
    /// At least one flag was given. The flags always win.
    Flags,
    /// No flag, and the person selected a project.
    Selected(&'a PlatformSelection, &'a Named),
    /// No flag and no selected project, and this machine is enrolled.
    Enrolled(&'a EnrollmentDocument),
    /// Nothing names a project: the command has no router to act on.
    Unnamed,
}

fn decide_target<'a>(
    workspace_flag: Option<&str>,
    project_flag: Option<&str>,
    selection: Option<&'a PlatformSelection>,
    enrollment: Option<&'a EnrollmentDocument>,
) -> TargetDecision<'a> {
    if workspace_flag.is_some() || project_flag.is_some() {
        return TargetDecision::Flags;
    }
    if let Some(selection) = selection
        && let Some(project) = &selection.project
    {
        return TargetDecision::Selected(selection, project);
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
    api: &mut PlatformApi,
    workspace_flag: Option<&str>,
    project_flag: Option<&str>,
    selection: Option<&PlatformSelection>,
    enrollment: Option<&EnrollmentDocument>,
) -> Result<Target> {
    match decide_target(workspace_flag, project_flag, selection, enrollment) {
        TargetDecision::Flags => {
            let resolved = resolve_project(
                api,
                workspace_flag,
                project_flag,
                selection,
                &mut Ask::Never,
            )?;
            Ok(Target {
                label: project_label(
                    &resolved.project.name,
                    &resolved.project.id,
                    &resolved.workspace.name,
                ),
                workspace_id: resolved.workspace.id,
                project_id: resolved.project.id,
                source: TargetSource::Flags,
            })
        }
        TargetDecision::Selected(selection, project) => Ok(Target {
            workspace_id: selection.workspace.id.clone(),
            project_id: project.id.clone(),
            label: format!(
                "{} (the selected project)",
                project_label(&project.name, &project.id, &selection.workspace.name)
            ),
            source: TargetSource::Selection,
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
            let listing = project_listing(api)
                .unwrap_or_else(|error| format!("  (the list could not be read: {error})"));
            Err(Error::ExecutionFailed(format!(
                "no project is selected: run `peppy platform project use` to select one, or pass \
                 --project <id|name> (and --workspace <id|name>). The projects you can use \
                 are:\n{listing}"
            )))
        }
    }
}

/// The error for a call the platform refused on `target`. A selection holds
/// ids from the time it was made, so a `403` or a `404` on a target that came
/// from the selection can mean that the project was archived or that the
/// person lost access: the refusal then names the command that selects again.
pub(crate) fn refusal_on(target: &Target, error: AuthError) -> Error {
    match &error {
        AuthError::Problem(problem)
            if target.source == TargetSource::Selection && matches!(problem.status, 403 | 404) =>
        {
            Error::Auth(format!(
                "{problem}. The selected project can be out of date. Run `peppy platform project \
                 use`."
            ))
        }
        _ => Error::AuthEngine(error),
    }
}

/// The projects of `workspace_id` that are not archived. An archived project
/// owns no router, so no command acts on it.
fn active_projects(api: &mut PlatformApi, workspace_id: &str) -> Result<Vec<Project>> {
    Ok(api
        .list_projects(workspace_id)?
        .into_iter()
        .filter(|project| !project.is_archived())
        .collect())
}

/// Every project of every workspace of the account that is not archived, one
/// per line, for an error that asks the person to name one.
fn project_listing(api: &mut PlatformApi) -> Result<String> {
    let mut lines = Vec::new();
    for workspace in api.list_workspaces()? {
        for project in active_projects(api, &workspace.id)? {
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

/// Picks one candidate, in the order of the module docs.
fn pick<T>(candidates: Candidates<T>, question: Question, ask: &mut Ask) -> Result<Picked<T>> {
    let Candidates {
        what,
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
    let position_of = |wanted: &str| {
        items.iter().position(|item| {
            let (id, name) = id_and_name(item);
            id == wanted || name == wanted
        })
    };

    if let Some(flag) = question.flag {
        let position = position_of(flag).ok_or_else(|| {
            Error::ExecutionFailed(format!(
                "no {what} with id or name {flag:?}; the {what}s you can use are:\n{listing}"
            ))
        })?;
        return Ok(Picked::One(take(items, position)));
    }
    if let Some(default_id) = question.default_id {
        let position = items
            .iter()
            .position(|item| id_and_name(item).0 == default_id)
            .ok_or_else(|| {
                Error::ExecutionFailed(format!(
                    "the selected {what} ({default_id}) is not one you can use; run `peppy \
                     platform {what} use` to select again. The {what}s you can use are:\n{listing}"
                ))
            })?;
        return Ok(Picked::One(take(items, position)));
    }
    match items.len() {
        0 => {
            return Err(Error::ExecutionFailed(format!(
                "no {what} is available to this account; create one in the web app first"
            )));
        }
        1 => return Ok(Picked::One(take(items, 0))),
        _ => {}
    }

    let mut options: Vec<String> = items.iter().map(option).collect();
    options.extend(question.decline.map(str::to_string));
    let menu = Menu {
        title: question.title,
        options: &options,
        start: menu_start(&items, id_and_name, question.current_id),
    };
    match ask.answer(&menu)? {
        None => Err(Error::ExecutionFailed(format!(
            "more than one {what} is available; {}:\n{listing}",
            question.remedy
        ))),
        Some(MenuAnswer::Selected(position)) if position < items.len() => {
            Ok(Picked::One(take(items, position)))
        }
        // The entry that declines the question, or a cancelled menu.
        Some(MenuAnswer::Selected(_) | MenuAnswer::Cancelled) => Ok(Picked::Declined),
    }
}

/// Where the menu starts: on the candidate of `current_id`, else on the first
/// one.
fn menu_start<T>(
    items: &[T],
    id_and_name: fn(&T) -> (&String, &String),
    current_id: Option<&str>,
) -> usize {
    current_id
        .and_then(|current_id| {
            items
                .iter()
                .position(|item| id_and_name(item).0 == current_id)
        })
        .unwrap_or(0)
}

/// The item at `position`, which the caller found in `items`.
fn take<T>(items: Vec<T>, position: usize) -> T {
    items
        .into_iter()
        .nth(position)
        .expect("a position of the list")
}

#[cfg(test)]
mod tests {
    use super::*;
    use auth::ProblemKind;
    use auth::test_support::{enrollment_document as enrolled, platform_selection as selection};

    type Pair = (String, String);

    fn named(id: &str, name: &str) -> Pair {
        (id.to_string(), name.to_string())
    }

    fn candidates(items: Vec<Pair>) -> Candidates<Pair> {
        Candidates {
            what: "project",
            items,
            id_and_name: |c: &Pair| (&c.0, &c.1),
            option: |c: &Pair| c.1.clone(),
        }
    }

    fn two() -> Vec<Pair> {
        vec![named("id-1", "Lab"), named("id-2", "Field")]
    }

    /// A question with nothing but a title and a remedy.
    fn question() -> Question<'static> {
        Question {
            title: "Project",
            flag: None,
            default_id: None,
            current_id: None,
            decline: None,
            remedy: "pass --project <id|name>",
        }
    }

    fn picked(pick: Result<Picked<Pair>>) -> Pair {
        match pick.expect("a pick") {
            Picked::One(pair) => pair,
            Picked::Declined => panic!("the question was declined"),
        }
    }

    /// The rule that names the router: a flag always wins, then the selected
    /// project, then the enrollment, and with none of them the command has no
    /// target.
    #[test]
    fn the_flags_win_then_the_selected_project_then_the_enrollment_then_nothing() {
        let (selection, document) = (selection(), enrolled());
        let project = selection.project.clone().expect("a selected project");
        for (workspace, project) in [
            (Some("ws"), Some("p")),
            (None, Some("p")),
            (Some("ws"), None),
        ] {
            assert_eq!(
                decide_target(workspace, project, Some(&selection), Some(&document)),
                TargetDecision::Flags,
                "flags win over a selection and an enrollment"
            );
            assert_eq!(
                decide_target(workspace, project, None, None),
                TargetDecision::Flags
            );
        }
        assert_eq!(
            decide_target(None, None, Some(&selection), Some(&document)),
            TargetDecision::Selected(&selection, &project),
            "the selected project wins over the enrollment"
        );
        let workspace_only = PlatformSelection {
            project: None,
            ..selection.clone()
        };
        assert_eq!(
            decide_target(None, None, Some(&workspace_only), Some(&document)),
            TargetDecision::Enrolled(&document),
            "a selected workspace alone names no router"
        );
        assert_eq!(
            decide_target(None, None, None, Some(&document)),
            TargetDecision::Enrolled(&document)
        );
        assert_eq!(
            decide_target(None, None, Some(&workspace_only), None),
            TargetDecision::Unnamed
        );
    }

    #[test]
    fn a_flag_picks_by_id_or_exact_name() {
        let by_id = Question {
            flag: Some("id-2"),
            ..question()
        };
        assert_eq!(
            picked(pick(candidates(two()), by_id, &mut Ask::Never)).1,
            "Field"
        );
        let by_name = Question {
            flag: Some("Lab"),
            default_id: Some("id-2"),
            ..question()
        };
        assert_eq!(
            picked(pick(candidates(two()), by_name, &mut Ask::Never)).0,
            "id-1",
            "the flag wins over the default"
        );
        let wrong_case = Question {
            flag: Some("lab"),
            ..question()
        };
        let err = pick(candidates(two()), wrong_case, &mut Ask::Never).unwrap_err();
        assert!(
            err.to_string().contains("id-1  Lab"),
            "lists the choices: {err}"
        );
    }

    #[test]
    fn the_default_picks_by_id_and_a_default_that_is_gone_names_use() {
        let default = |id| Question {
            default_id: Some(id),
            ..question()
        };
        assert_eq!(
            picked(pick(candidates(two()), default("id-2"), &mut Ask::Never)).1,
            "Field"
        );
        let err = pick(candidates(two()), default("id-9"), &mut Ask::Never).unwrap_err();
        let message = err.to_string();
        assert!(message.contains("peppy platform project use"), "{message}");
        assert!(message.contains("id-1  Lab"), "{message}");
    }

    #[test]
    fn with_no_flag_and_no_default_only_a_single_candidate_is_picked() {
        let one = vec![named("id-1", "Lab")];
        assert_eq!(
            picked(pick(candidates(one), question(), &mut Ask::Never)).0,
            "id-1"
        );

        let err = pick(candidates(Vec::new()), question(), &mut Ask::Never).unwrap_err();
        assert!(err.to_string().contains("no project"), "{err}");

        let err = pick(candidates(two()), question(), &mut Ask::Never).unwrap_err();
        assert_eq!(
            err.to_string(),
            "more than one project is available; pass --project <id|name>:\n  id-1  Lab\n  \
             id-2  Field",
            "the remedy of the question, then the choices"
        );
    }

    #[test]
    fn with_more_than_one_candidate_the_person_selects() {
        assert_eq!(
            picked(pick(candidates(two()), question(), &mut Ask::scripted([1]))).1,
            "Field"
        );
        assert_eq!(
            pick(candidates(two()), question(), &mut Ask::scripted([])).unwrap(),
            Picked::Declined,
            "a cancelled menu declines the question"
        );
        assert!(
            pick(candidates(two()), question(), &mut Ask::scripted([2])).is_err(),
            "with no entry to decline, the menu has two positions"
        );
    }

    /// The entry that declines comes last, after every candidate.
    #[test]
    fn the_entry_that_declines_is_the_last_of_the_menu() {
        let declinable = || Question {
            decline: Some("Do not enroll this machine"),
            ..question()
        };
        assert_eq!(
            pick(candidates(two()), declinable(), &mut Ask::scripted([2])).unwrap(),
            Picked::Declined
        );
        assert_eq!(
            picked(pick(
                candidates(two()),
                declinable(),
                &mut Ask::scripted([0])
            ))
            .1,
            "Lab"
        );
        let one = vec![named("id-1", "Lab")];
        assert_eq!(
            picked(pick(candidates(one), declinable(), &mut Ask::scripted([]))).1,
            "Lab",
            "the only candidate is picked with no menu, so nothing declines it"
        );
    }

    /// The menu starts on the selected candidate, so Enter keeps it.
    #[test]
    fn the_menu_starts_on_the_selected_candidate() {
        let id_and_name = candidates(Vec::new()).id_and_name;
        assert_eq!(menu_start(&two(), id_and_name, Some("id-2")), 1);
        assert_eq!(menu_start(&two(), id_and_name, None), 0);
        assert_eq!(
            menu_start(&two(), id_and_name, Some("id-9")),
            0,
            "a selected candidate that is gone starts the menu on the first one"
        );
    }

    #[test]
    fn a_declined_question_is_an_error_where_a_candidate_is_required() {
        assert_eq!(Picked::One(7).required("project").unwrap(), 7);
        let err = Picked::<u8>::Declined.required("project").unwrap_err();
        assert_eq!(err.to_string(), "no project was selected");
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

    /// Only a `403` or a `404` on a target from the selection gets the hint.
    #[test]
    fn a_refusal_on_a_selected_target_names_project_use() {
        for status in [403, 404] {
            let message = refusal_on(&target(TargetSource::Selection), problem(status)).to_string();
            assert_eq!(
                message,
                "Not Found. The selected project can be out of date. Run `peppy platform project \
                 use`."
            );
        }
        assert_eq!(
            refusal_on(&target(TargetSource::Selection), problem(422)).to_string(),
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
                &target(TargetSource::Selection),
                AuthError::Http("boom".into())
            )
            .to_string(),
            "boom"
        );
    }
}
