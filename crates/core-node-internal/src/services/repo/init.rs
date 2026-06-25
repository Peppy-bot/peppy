use crate::Result;
use config::consts::PeppyDirs;
use serde_json::Value;
use std::collections::HashSet;
use tracing::info;

const DEFAULT_REPOS_TEMPLATE: &str = include_str!("../../../assets/default_repositories.json5");

/// Outcome of [`ensure_default_repos`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InitOutcome {
    /// File did not exist; default template was written verbatim.
    Created,
    /// File already existed; the given number of missing default entries were appended.
    Updated { added: usize },
}

/// Ensures `repositories.json5` exists and contains every entry from the
/// bundled default template.
///
/// - If the file does not exist, the default template is written verbatim
///   so its comments and formatting are preserved.
/// - If the file exists, default entries whose `id` is not already present
///   are appended verbatim. An existing entry with the same `id` claims that
///   slot regardless of its `type`, `url`, or `ref` — so users who change a
///   default's branch (or repoint it entirely) never get the default re-added
///   alongside their edit.
///
/// Runs at daemon startup and is also exposed via `peppy repo init` so
/// users can resync after upgrading peppy without restarting the daemon.
pub fn ensure_default_repos(peppy_dirs: &PeppyDirs) -> Result<InitOutcome> {
    let conf_dir = peppy_dirs.conf_dir();
    std::fs::create_dir_all(&conf_dir)?;
    let repos_path = conf_dir.join("repositories.json5");

    let _guard = crate::services::repo::repos_file_lock().lock();

    if !repos_path.exists() {
        std::fs::write(&repos_path, DEFAULT_REPOS_TEMPLATE)?;
        return Ok(InitOutcome::Created);
    }

    let content = std::fs::read_to_string(&repos_path)?;
    let mut existing: Vec<Value> = serde_json5::from_str(&content).map_err(|e| {
        core_node_api::Error::Decoding(format!("failed to parse repositories.json5: {e}"))
    })?;
    let defaults: Vec<Value> = serde_json5::from_str(DEFAULT_REPOS_TEMPLATE).map_err(|e| {
        core_node_api::Error::Decoding(format!("failed to parse default repositories: {e}"))
    })?;

    let existing_ids: HashSet<u64> = existing
        .iter()
        .filter_map(|e| e.get("id").and_then(|v| v.as_u64()))
        .collect();

    let mut added = 0usize;
    for default_entry in defaults {
        let Some(default_id) = default_entry.get("id").and_then(|v| v.as_u64()) else {
            continue;
        };
        if existing_ids.contains(&default_id) {
            continue;
        }
        existing.push(default_entry);
        added += 1;
    }

    if added > 0 {
        existing.sort_by_key(|e| e.get("id").and_then(|v| v.as_u64()).unwrap_or(0));
        let serialized = json5_pretty::to_string_pretty(&existing).map_err(|e| {
            core_node_api::Error::Encoding(format!("failed to serialize repositories: {e}"))
        })?;
        std::fs::write(&repos_path, serialized)?;
        info!(
            "Added {} missing default repositor{} to repositories.json5",
            added,
            if added == 1 { "y" } else { "ies" }
        );
    }

    Ok(InitOutcome::Updated { added })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::repo::cache::repositories_list_path;

    /// Helper: read repositories.json5 as a Vec<Value>.
    fn read_repos(peppy_dirs: &PeppyDirs) -> Vec<Value> {
        let path = repositories_list_path(peppy_dirs);
        let content = std::fs::read_to_string(&path).unwrap();
        serde_json5::from_str(&content).unwrap()
    }

    /// Helper: returns true if any entry has the given git url.
    fn has_git_url(repos: &[Value], url: &str) -> bool {
        repos.iter().any(|e| {
            e.get("type").and_then(|v| v.as_str()) == Some("git")
                && e.get("url").and_then(|v| v.as_str()) == Some(url)
        })
    }

    #[test]
    fn creating_new_file_preserves_template_comments_and_formatting_byte_for_byte() {
        let tmp = tempfile::tempdir().unwrap();
        let peppy_dirs = PeppyDirs::new(tmp.path());
        let repos_path = repositories_list_path(&peppy_dirs);
        assert!(!repos_path.exists());

        ensure_default_repos(&peppy_dirs).unwrap();

        assert!(repos_path.exists());
        // Verbatim write preserves comments from the json5 template.
        let written = std::fs::read_to_string(&repos_path).unwrap();
        assert_eq!(written, DEFAULT_REPOS_TEMPLATE);
    }

    /// A user upgrades peppy and a new entry (`launchers_hub`) is added
    /// to the bundled defaults, but their pre-existing `repositories.json5`
    /// only contains the older entries.
    /// `ensure_default_repos` must add the missing default(s) without
    /// disturbing what is already there.
    #[test]
    fn adds_missing_default_when_file_already_has_some_defaults() {
        let tmp = tempfile::tempdir().unwrap();
        let peppy_dirs = PeppyDirs::new(tmp.path());
        let conf_dir = peppy_dirs.conf_dir();
        std::fs::create_dir_all(&conf_dir).unwrap();
        let repos_path = conf_dir.join("repositories.json5");
        std::fs::write(
            &repos_path,
            r#"[
                { "id": 1000, "type": "git", "url": "https://github.com/Peppy-bot/nodes_hub", "ref": "main" }
            ]"#,
        )
        .unwrap();

        ensure_default_repos(&peppy_dirs).unwrap();

        let repos = read_repos(&peppy_dirs);
        assert!(
            has_git_url(&repos, "https://github.com/Peppy-bot/nodes_hub"),
            "pre-existing nodes_hub entry must be preserved, got: {repos:?}"
        );
        assert!(
            has_git_url(&repos, "https://github.com/Peppy-bot/launchers_hub.git"),
            "missing launchers_hub default must be appended, got: {repos:?}"
        );
    }

    #[test]
    fn preserves_user_repos_when_adding_defaults() {
        let tmp = tempfile::tempdir().unwrap();
        let peppy_dirs = PeppyDirs::new(tmp.path());
        let conf_dir = peppy_dirs.conf_dir();
        std::fs::create_dir_all(&conf_dir).unwrap();
        std::fs::write(
            conf_dir.join("repositories.json5"),
            r#"[
                { "id": 1, "type": "fs", "path": "/home/me/my_nodes" }
            ]"#,
        )
        .unwrap();

        ensure_default_repos(&peppy_dirs).unwrap();

        let repos = read_repos(&peppy_dirs);
        assert!(
            repos
                .iter()
                .any(|e| e.get("type").and_then(|v| v.as_str()) == Some("fs")
                    && e.get("path").and_then(|v| v.as_str()) == Some("/home/me/my_nodes")),
            "user fs repo must be preserved"
        );
        assert!(has_git_url(
            &repos,
            "https://github.com/Peppy-bot/nodes_hub"
        ));
        assert!(has_git_url(
            &repos,
            "https://github.com/Peppy-bot/launchers_hub.git"
        ));
    }

    #[test]
    fn no_changes_when_all_defaults_already_present() {
        let tmp = tempfile::tempdir().unwrap();
        let peppy_dirs = PeppyDirs::new(tmp.path());
        let conf_dir = peppy_dirs.conf_dir();
        std::fs::create_dir_all(&conf_dir).unwrap();
        let repos_path = conf_dir.join("repositories.json5");
        std::fs::write(&repos_path, DEFAULT_REPOS_TEMPLATE).unwrap();
        let content_before = std::fs::read_to_string(&repos_path).unwrap();

        let outcome = ensure_default_repos(&peppy_dirs).unwrap();

        // Nothing needs adding when the file already holds every default, and
        // the file is left byte-for-byte unchanged. Asserted directly on the
        // outcome + content (deterministic) rather than via filesystem mtime +
        // a sleep, which depended on wall-clock granularity.
        assert_eq!(outcome, InitOutcome::Updated { added: 0 });
        let content_after = std::fs::read_to_string(&repos_path).unwrap();
        assert_eq!(
            content_before, content_after,
            "file should not be rewritten when nothing changes"
        );
    }

    #[test]
    fn is_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let peppy_dirs = PeppyDirs::new(tmp.path());
        let conf_dir = peppy_dirs.conf_dir();
        std::fs::create_dir_all(&conf_dir).unwrap();
        std::fs::write(
            conf_dir.join("repositories.json5"),
            r#"[
                { "id": 1000, "type": "git", "url": "https://github.com/Peppy-bot/nodes_hub", "ref": "main" }
            ]"#,
        )
        .unwrap();

        ensure_default_repos(&peppy_dirs).unwrap();
        let after_first = read_repos(&peppy_dirs);
        ensure_default_repos(&peppy_dirs).unwrap();
        let after_second = read_repos(&peppy_dirs);

        assert_eq!(
            after_first, after_second,
            "second invocation must not introduce duplicates"
        );
    }

    /// If an existing entry occupies a default's id, that id is considered
    /// taken and the default is NOT re-added — regardless of url/ref/type.
    /// This is what stops the daemon from duplicating defaults when a user
    /// edits a default entry's ref (e.g. main → feature/x).
    #[test]
    fn skips_default_when_id_is_already_taken() {
        let tmp = tempfile::tempdir().unwrap();
        let peppy_dirs = PeppyDirs::new(tmp.path());
        let conf_dir = peppy_dirs.conf_dir();
        std::fs::create_dir_all(&conf_dir).unwrap();
        std::fs::write(
            conf_dir.join("repositories.json5"),
            r#"[
                { "id": 1000, "type": "git", "url": "https://github.com/Peppy-bot/nodes_hub", "ref": "feature/v0.10.0" },
                { "id": 1001, "type": "fs", "path": "/some/where" }
            ]"#,
        )
        .unwrap();

        ensure_default_repos(&peppy_dirs).unwrap();

        let repos = read_repos(&peppy_dirs);
        // Exactly one entry per id 1000 and 1001 — the user's entries, not the defaults.
        let id_1000: Vec<_> = repos
            .iter()
            .filter(|e| e.get("id").and_then(|v| v.as_u64()) == Some(1000))
            .collect();
        assert_eq!(
            id_1000.len(),
            1,
            "id 1000 must not be duplicated: {repos:?}"
        );
        assert_eq!(
            id_1000[0].get("ref").and_then(|v| v.as_str()),
            Some("feature/v0.10.0"),
            "user's edited ref must be preserved"
        );

        let id_1001: Vec<_> = repos
            .iter()
            .filter(|e| e.get("id").and_then(|v| v.as_u64()) == Some(1001))
            .collect();
        assert_eq!(
            id_1001.len(),
            1,
            "id 1001 must not be duplicated: {repos:?}"
        );
        assert_eq!(
            id_1001[0].get("type").and_then(|v| v.as_str()),
            Some("fs"),
            "user's fs entry at id 1001 must be preserved"
        );

        // launchers_hub default is not present — id 1001 is taken, so it is skipped.
        assert!(
            !has_git_url(&repos, "https://github.com/Peppy-bot/launchers_hub.git"),
            "launchers_hub default must not be added when its id 1001 is taken"
        );
    }

    /// `repo remove` deletes an entry by id. Subsequent init runs would
    /// re-add it (this is a deliberate trade-off — the user can use
    /// `repo exclude` for permanent suppression). Cover the read-back to
    /// document the behaviour.
    #[test]
    fn re_adds_default_after_repo_remove() {
        let tmp = tempfile::tempdir().unwrap();
        let peppy_dirs = PeppyDirs::new(tmp.path());

        ensure_default_repos(&peppy_dirs).unwrap();
        let initial = read_repos(&peppy_dirs);
        assert!(has_git_url(
            &initial,
            "https://github.com/Peppy-bot/launchers_hub.git"
        ));

        // Simulate `repo remove` deleting the launchers_hub entry.
        let mut without_launchers: Vec<Value> = initial
            .into_iter()
            .filter(|e| {
                e.get("url").and_then(|v| v.as_str())
                    != Some("https://github.com/Peppy-bot/launchers_hub.git")
            })
            .collect();
        let serialized = json5_pretty::to_string_pretty(&without_launchers).unwrap();
        std::fs::write(repositories_list_path(&peppy_dirs), &serialized).unwrap();
        without_launchers.clear(); // dropped, only used to write

        ensure_default_repos(&peppy_dirs).unwrap();

        let after = read_repos(&peppy_dirs);
        assert!(
            has_git_url(&after, "https://github.com/Peppy-bot/launchers_hub.git"),
            "init should re-add a removed default"
        );
    }
}
