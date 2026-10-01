//! The platform selection: the workspace, and the project of that workspace,
//! that the person selected, stored at `<root>/conf/platform_selection.json5`.
//! It is the default target of the `peppy platform` commands, so a person in
//! more than one workspace, or with more than one project, names the project
//! one time and not on each command. A selection names a workspace always, and
//! a project when the person selected one.
//!
//! The selection is state of the CLI only. It does not change the enrollment
//! of this machine ([`crate::enrollment`]) and the daemon does not read it.
//!
//! A selection belongs to one backend and one signed-in identity. The ids in
//! it mean nothing to a different account, so a command uses the stored
//! selection only when [`PlatformSelection::belongs_to`] holds for its session.

use std::path::PathBuf;

use daemon_config::consts::PeppyDirs;
use serde::{Deserialize, Serialize};

use crate::client::{Project, Workspace};
use crate::document::{self, Versioned};
use crate::error::Result;
use crate::fs_perms::restrict_dir;

/// On-disk schema version of `platform_selection.json5`. There is no reader
/// for another version: a file of another version is rejected by [`load`], and
/// the person selects again.
pub const SELECTION_VERSION: u32 = 1;

/// A workspace or a project: the id the API takes, and the name the person
/// reads.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Named {
    pub id: String,
    pub name: String,
}

impl From<&Workspace> for Named {
    fn from(workspace: &Workspace) -> Self {
        Self {
            id: workspace.id.clone(),
            name: workspace.name.clone(),
        }
    }
}

impl From<&Project> for Named {
    fn from(project: &Project) -> Self {
        Self {
            id: project.id.clone(),
            name: project.name.clone(),
        }
    }
}

/// The persisted selection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlatformSelection {
    /// Schema version (see [`SELECTION_VERSION`]). Defaults to `0` when absent
    /// so an unversioned file is rejected.
    #[serde(default)]
    pub version: u32,
    /// The origin of the platform API the selection was made against, as
    /// [`crate::profile::normalize_api_origin`] spells it.
    pub api_origin: String,
    /// The OAuth subject of the identity that made the selection.
    pub subject: String,
    pub workspace: Named,
    /// The selected project of [`Self::workspace`], `None` when the person
    /// selected the workspace alone.
    #[serde(default)]
    pub project: Option<Named>,
    /// When the selection was made, unix seconds.
    pub selected_at: i64,
}

impl PlatformSelection {
    /// Whether this selection was made against `api_origin` by `subject`. An
    /// empty subject owns no selection: it means the identity of the session
    /// is not known.
    pub fn belongs_to(&self, api_origin: &str, subject: &str) -> bool {
        !subject.is_empty() && self.api_origin == api_origin && self.subject == subject
    }

    /// The selected workspace in words, for the person.
    pub fn workspace_label(&self) -> String {
        workspace_label(&self.workspace.name, &self.workspace.id)
    }

    /// The selected project in words, for the person, or `None` when no
    /// project is selected.
    pub fn project_label(&self) -> Option<String> {
        self.project
            .as_ref()
            .map(|project| project_label(&project.name, &project.id, &self.workspace.name))
    }
}

/// A workspace in words, for the person: its name and its id.
pub fn workspace_label(workspace_name: &str, workspace_id: &str) -> String {
    format!("workspace {workspace_name} ({workspace_id})")
}

/// A project in words, for the person: its name, its id, and the name of its
/// workspace.
pub fn project_label(project_name: &str, project_id: &str, workspace_name: &str) -> String {
    format!("project {project_name} ({project_id}) in workspace {workspace_name}")
}

impl Versioned for PlatformSelection {
    const VERSION: u32 = SELECTION_VERSION;
    const WHAT: &'static str = "selection";
    const REMEDY: &'static str = "peppy platform project use";

    fn version(&self) -> u32 {
        self.version
    }
}

/// Selection path under a given peppy root.
pub fn selection_path(dirs: &PeppyDirs) -> PathBuf {
    dirs.conf_dir()
        .join(daemon_config::consts::PLATFORM_SELECTION_FILE)
}

/// Loads the selection. `Ok(None)` when the file is absent (nothing is
/// selected). An error when it is present but does not parse or is of another
/// version; the message names `peppy platform project use` as the remedy.
pub fn load(dirs: &PeppyDirs) -> Result<Option<PlatformSelection>> {
    document::load(&selection_path(dirs))
}

/// Atomically writes the selection.
pub fn save(dirs: &PeppyDirs, selection: &PlatformSelection) -> Result<()> {
    let conf = dirs.conf_dir();
    std::fs::create_dir_all(&conf)?;
    // `conf/` holds the credentials too; keep it owner-only as `storage` does.
    restrict_dir(&conf)?;
    document::save(&selection_path(dirs), selection, false)
}

/// Removes the selection. Absent is not an error: nothing is then selected.
pub fn remove(dirs: &PeppyDirs) -> Result<()> {
    document::remove_if_present(&selection_path(dirs))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::platform_selection as selection;

    #[test]
    fn save_then_load_round_trips_and_remove_clears() {
        let dir = tempfile::tempdir().unwrap();
        let dirs = PeppyDirs::new(dir.path());
        assert_eq!(load(&dirs).unwrap(), None, "absent is no selection");

        save(&dirs, &selection()).expect("save");
        assert_eq!(load(&dirs).unwrap(), Some(selection()));

        remove(&dirs).expect("remove");
        assert_eq!(load(&dirs).unwrap(), None);
        remove(&dirs).expect("removing two times is fine");
    }

    /// A workspace with no project is a selection too, and the file of one
    /// with no `project` member reads as one.
    #[test]
    fn a_selection_of_a_workspace_alone_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let dirs = PeppyDirs::new(dir.path());
        let workspace_only = PlatformSelection {
            project: None,
            ..selection()
        };

        save(&dirs, &workspace_only).expect("save");
        assert_eq!(load(&dirs).unwrap(), Some(workspace_only.clone()));

        let written = std::fs::read_to_string(selection_path(&dirs)).unwrap();
        let without_member = written.replace("project: null,", "");
        assert_ne!(written, without_member, "the member was written: {written}");
        std::fs::write(selection_path(&dirs), without_member).unwrap();
        assert_eq!(load(&dirs).unwrap(), Some(workspace_only));
    }

    #[test]
    fn a_file_that_cannot_be_used_fails_the_load_naming_project_use() {
        let dir = tempfile::tempdir().unwrap();
        let dirs = PeppyDirs::new(dir.path());
        save(&dirs, &selection()).unwrap();
        let good = std::fs::read_to_string(selection_path(&dirs)).unwrap();

        for (label, content) in [
            ("version", good.replace("version: 1", "version: 2")),
            ("unversioned", good.replace("version: 1,", "")),
            ("syntax", "{ not json5".to_string()),
        ] {
            std::fs::write(selection_path(&dirs), content).unwrap();
            let err = load(&dirs).expect_err(label);
            assert!(
                err.to_string().contains("peppy platform project use"),
                "{label}: {err}"
            );
        }
    }

    #[test]
    fn a_selection_belongs_to_one_backend_and_one_identity() {
        let selection = selection();
        assert!(selection.belongs_to("https://api.example", "user-123"));
        assert!(!selection.belongs_to("https://api.example", "user-456"));
        assert!(!selection.belongs_to("https://other.example", "user-123"));
        let mut unknown = selection.clone();
        unknown.subject = String::new();
        assert!(
            !unknown.belongs_to("https://api.example", ""),
            "an identity that is not known owns no selection"
        );
    }

    #[test]
    fn the_labels_name_the_project_and_the_workspace() {
        assert_eq!(
            selection().workspace_label(),
            "workspace Robotics lab (ws-2)"
        );
        assert_eq!(
            selection().project_label().as_deref(),
            Some("project Field (p-2) in workspace Robotics lab")
        );
        let workspace_only = PlatformSelection {
            project: None,
            ..selection()
        };
        assert_eq!(workspace_only.project_label(), None);
    }
}
