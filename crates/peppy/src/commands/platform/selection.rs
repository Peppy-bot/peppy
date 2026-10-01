//! The selected workspace and project of the CLI: how a command stores a new
//! selection and how it shows one. `peppy platform workspace`, `peppy platform
//! project` and `peppy platform login` select; `status` shows the selection
//! too.
//!
//! The selection is for the CLI only. A new selection does not move this
//! machine to a different project and does not restart the daemon: that is
//! `peppy platform enroll --replace`.

use auth::enrollment::EnrollmentDocument;
use auth::selection::{Named, PlatformSelection, SELECTION_VERSION};
use auth::{PlatformApi, storage};

use crate::commands::platform::PlatformSession;
use crate::error::Result;

/// Stores `workspace`, and `project` when there is one, as the selection of
/// the signed-in identity, and returns it.
pub(crate) fn save_selection(
    session: &PlatformSession,
    api: &mut PlatformApi,
    workspace: Named,
    project: Option<Named>,
) -> Result<PlatformSelection> {
    let subject = match session.subject() {
        Some(subject) => subject,
        None => api.get_me()?.sub,
    };
    let selection = PlatformSelection {
        version: SELECTION_VERSION,
        api_origin: session.api_origin()?,
        subject,
        workspace,
        project,
        selected_at: storage::now_unix(),
    };
    auth::selection::save(&session.dirs, &selection)?;
    Ok(selection)
}

/// What the person reads after a selection, and from `project show`: the
/// selected project, or the selected workspace and the command that selects a
/// project, and a note when this machine is enrolled in a different project.
pub(crate) fn selection_report(session: &PlatformSession, selection: &PlatformSelection) -> String {
    let Some(project) = selection.project_label() else {
        return format!(
            "Selected {}. No project is selected: `peppy platform project use` selects one.\n",
            selection.workspace_label()
        );
    };
    let mut out = format!("Selected {project}.\n");
    let enrollment = auth::enrollment::load(&session.dirs).ok().flatten();
    if let Some(note) = enrollment_note(selection, enrollment.as_ref().map(|e| &e.document)) {
        out.push_str(&note);
        out.push('\n');
    }
    out
}

/// The note for a machine that is enrolled in a different project than the
/// selected one, or `None` when there is nothing to say.
pub(crate) fn enrollment_note(
    selection: &PlatformSelection,
    enrollment: Option<&EnrollmentDocument>,
) -> Option<String> {
    let project = selection.project.as_ref()?;
    let enrollment = enrollment.filter(|document| document.project_id != project.id)?;
    Some(format!(
        "This machine is enrolled in project {}. The selection does not move it. To move it: \
         `peppy platform enroll --replace`",
        enrollment.project_id
    ))
}

/// A workspace or a project as JSON.
fn named_json(named: &Named) -> serde_json::Value {
    serde_json::json!({ "id": named.id, "name": named.name })
}

/// The whole selection as JSON, `null` when there is none.
pub(crate) fn selection_json(selection: Option<&PlatformSelection>) -> serde_json::Value {
    let Some(selection) = selection else {
        return serde_json::Value::Null;
    };
    serde_json::json!({
        "workspace": named_json(&selection.workspace),
        "project": selection.project.as_ref().map(named_json),
        "selected_at": selection.selected_at,
    })
}

/// The selected workspace as JSON, `null` when there is none.
pub(crate) fn workspace_json(selection: Option<&PlatformSelection>) -> serde_json::Value {
    selection
        .map(|selection| named_json(&selection.workspace))
        .unwrap_or(serde_json::Value::Null)
}

/// The selected project with its workspace as JSON, `null` when no project is
/// selected.
pub(crate) fn project_json(selection: Option<&PlatformSelection>) -> serde_json::Value {
    let Some((selection, project)) =
        selection.and_then(|selection| Some((selection, selection.project.as_ref()?)))
    else {
        return serde_json::Value::Null;
    };
    serde_json::json!({
        "id": project.id,
        "name": project.name,
        "workspace": named_json(&selection.workspace),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use auth::test_support::{enrollment_document, platform_selection as selection};

    /// The record of a machine enrolled in `project_id`.
    fn enrolled_in(project_id: &str) -> EnrollmentDocument {
        EnrollmentDocument {
            project_id: project_id.into(),
            namespace: config::namespace::Namespace::parse(project_id).unwrap(),
            ..enrollment_document()
        }
    }

    fn workspace_only() -> PlatformSelection {
        PlatformSelection {
            project: None,
            ..selection()
        }
    }

    #[test]
    fn the_note_is_only_for_a_machine_enrolled_in_a_different_project() {
        assert_eq!(enrollment_note(&selection(), None), None);
        assert_eq!(
            enrollment_note(&selection(), Some(&enrolled_in("p-2"))),
            None
        );
        let note = enrollment_note(&selection(), Some(&enrolled_in("p-1"))).expect("a note");
        assert!(note.contains("enrolled in project p-1"), "{note}");
        assert!(note.contains("peppy platform enroll --replace"), "{note}");
        assert_eq!(
            enrollment_note(&workspace_only(), Some(&enrolled_in("p-1"))),
            None,
            "with no selected project there is no project to differ from"
        );
    }

    #[test]
    fn the_json_forms_carry_the_selection() {
        assert_eq!(selection_json(None), serde_json::Value::Null);
        assert_eq!(workspace_json(None), serde_json::Value::Null);
        assert_eq!(project_json(None), serde_json::Value::Null);

        let doc = selection_json(Some(&selection()));
        assert_eq!(doc["workspace"]["id"], "ws-2");
        assert_eq!(doc["project"]["id"], "p-2");
        assert_eq!(workspace_json(Some(&selection()))["name"], "Robotics lab");
        let project = project_json(Some(&selection()));
        assert_eq!(project["id"], "p-2");
        assert_eq!(project["workspace"]["id"], "ws-2");

        assert_eq!(
            selection_json(Some(&workspace_only()))["project"],
            serde_json::Value::Null
        );
        assert_eq!(
            project_json(Some(&workspace_only())),
            serde_json::Value::Null
        );
        assert_eq!(workspace_json(Some(&workspace_only()))["id"], "ws-2");
    }
}
