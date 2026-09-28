//! The platform context: the workspace and the project the person selected,
//! stored at `<root>/conf/platform_context.json5`. It is the default target of
//! the `peppy platform` commands, so a person in more than one workspace, or
//! with more than one project, names the project one time and not on each
//! command.
//!
//! The context is state of the CLI only. It does not change the enrollment of
//! this machine ([`crate::enrollment`]) and the daemon does not read it.
//!
//! A context belongs to one backend and one signed-in identity. The ids in it
//! mean nothing to a different account, so a command uses the stored context
//! only when [`PlatformContext::belongs_to`] holds for its session.

use std::path::PathBuf;

use daemon_config::consts::PeppyDirs;
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::fs_perms::restrict_dir;

/// On-disk schema version of `platform_context.json5`. There is no reader for
/// another version: a file of another version is rejected by [`load`], and the
/// person selects again.
pub const CONTEXT_VERSION: u32 = 1;

/// A workspace or a project: the id the API takes, and the name the person
/// reads.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Named {
    pub id: String,
    pub name: String,
}

/// The persisted context.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlatformContext {
    /// Schema version (see [`CONTEXT_VERSION`]). Defaults to `0` when absent so
    /// an unversioned file is rejected.
    #[serde(default)]
    pub version: u32,
    /// The origin of the platform API the selection was made against, as
    /// [`crate::profile::normalize_api_origin`] spells it.
    pub api_origin: String,
    /// The OAuth subject of the identity that made the selection.
    pub subject: String,
    pub workspace: Named,
    pub project: Named,
    /// When the selection was made, unix seconds.
    pub selected_at: i64,
}

impl PlatformContext {
    /// Whether this context was selected against `api_origin` by `subject`. An
    /// empty subject belongs to no context: it means the identity of the
    /// session is not known.
    pub fn belongs_to(&self, api_origin: &str, subject: &str) -> bool {
        !subject.is_empty() && self.api_origin == api_origin && self.subject == subject
    }

    /// The context in words, for the person.
    pub fn label(&self) -> String {
        format!(
            "project {} ({}) in workspace {}",
            self.project.name, self.project.id, self.workspace.name
        )
    }
}

/// Context path under a given peppy root.
pub fn context_path(dirs: &PeppyDirs) -> PathBuf {
    dirs.conf_dir()
        .join(daemon_config::consts::PLATFORM_CONTEXT_FILE)
}

/// Loads the context. `Ok(None)` when the file is absent (no selection was
/// made). An error when it is present but does not parse or is of another
/// version; the message names `peppy platform configure` as the remedy.
pub fn load(dirs: &PeppyDirs) -> Result<Option<PlatformContext>> {
    let path = context_path(dirs);
    let content = match std::fs::read_to_string(&path) {
        Ok(content) => content,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(Error::Io(e)),
    };
    let context: PlatformContext = serde_json5::from_str(&content).map_err(|e| {
        Error::Auth(format!(
            "failed to parse {}: {e}; run `peppy platform configure` again",
            path.display()
        ))
    })?;
    if context.version != CONTEXT_VERSION {
        return Err(Error::Auth(format!(
            "context file {} is an unsupported format (v{}, expected v{}); \
             run `peppy platform configure` again",
            path.display(),
            context.version,
            CONTEXT_VERSION
        )));
    }
    Ok(Some(context))
}

/// Atomically writes the context.
pub fn save(dirs: &PeppyDirs, context: &PlatformContext) -> Result<()> {
    let content = json5_pretty::to_string_pretty(context)
        .map_err(|e| Error::Auth(format!("failed to serialize the context: {e}")))?;
    let conf = dirs.conf_dir();
    std::fs::create_dir_all(&conf)?;
    // `conf/` holds the credentials too; keep it owner-only as `storage` does.
    restrict_dir(&conf)?;
    daemon_config::atomic_write::publish_atomic(&context_path(dirs), |tmp| {
        std::fs::write(tmp, &content)
    })?;
    Ok(())
}

/// Removes the context. Absent is not an error: there is then no context.
pub fn remove(dirs: &PeppyDirs) -> Result<()> {
    match std::fs::remove_file(context_path(dirs)) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(Error::Io(e)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn context() -> PlatformContext {
        PlatformContext {
            version: CONTEXT_VERSION,
            api_origin: "https://api.example.test".into(),
            subject: "user-123".into(),
            workspace: Named {
                id: "ws-1".into(),
                name: "Robotics lab".into(),
            },
            project: Named {
                id: "p-1".into(),
                name: "Lab".into(),
            },
            selected_at: 1_700_000_000,
        }
    }

    #[test]
    fn save_then_load_round_trips_and_remove_clears() {
        let dir = tempfile::tempdir().unwrap();
        let dirs = PeppyDirs::new(dir.path());
        assert_eq!(load(&dirs).unwrap(), None, "absent is no context");

        save(&dirs, &context()).expect("save");
        assert_eq!(load(&dirs).unwrap(), Some(context()));

        remove(&dirs).expect("remove");
        assert_eq!(load(&dirs).unwrap(), None);
        remove(&dirs).expect("removing two times is fine");
    }

    #[test]
    fn a_file_that_cannot_be_used_fails_the_load_naming_configure() {
        let dir = tempfile::tempdir().unwrap();
        let dirs = PeppyDirs::new(dir.path());
        save(&dirs, &context()).unwrap();
        let good = std::fs::read_to_string(context_path(&dirs)).unwrap();

        for (label, content) in [
            ("version", good.replace("version: 1", "version: 2")),
            ("unversioned", good.replace("version: 1,", "")),
            ("syntax", "{ not json5".to_string()),
        ] {
            std::fs::write(context_path(&dirs), content).unwrap();
            let err = load(&dirs).expect_err(label);
            assert!(
                err.to_string().contains("peppy platform configure"),
                "{label}: {err}"
            );
        }
    }

    #[test]
    fn a_context_belongs_to_one_backend_and_one_identity() {
        let context = context();
        assert!(context.belongs_to("https://api.example.test", "user-123"));
        assert!(!context.belongs_to("https://api.example.test", "user-456"));
        assert!(!context.belongs_to("https://other.example.test", "user-123"));
        let mut unknown = context.clone();
        unknown.subject = String::new();
        assert!(
            !unknown.belongs_to("https://api.example.test", ""),
            "an identity that is not known owns no context"
        );
    }

    #[test]
    fn the_label_names_the_project_and_the_workspace() {
        assert_eq!(
            context().label(),
            "project Lab (p-1) in workspace Robotics lab"
        );
    }
}
