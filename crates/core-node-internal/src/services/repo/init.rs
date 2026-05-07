use crate::Result;
use crate::services::repo::json_entry_identity;
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
/// - If the file exists, default entries whose identity (as defined by
///   [`json_entry_identity`]) is not already present are appended with a
///   non-conflicting `id`. Existing user entries are preserved unchanged.
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

    let existing_identities: HashSet<String> =
        existing.iter().filter_map(json_entry_identity).collect();
    let mut used_ids: HashSet<u64> = existing
        .iter()
        .filter_map(|e| e.get("id").and_then(|v| v.as_u64()))
        .collect();
    let mut max_id = used_ids.iter().copied().max().unwrap_or(0);

    let mut added = 0usize;
    for default_entry in defaults {
        let Some(identity) = json_entry_identity(&default_entry) else {
            continue;
        };
        if existing_identities.contains(&identity) {
            continue;
        }

        let mut new_entry = default_entry.clone();
        let default_id = new_entry.get("id").and_then(|v| v.as_u64());
        let id_to_use = match default_id {
            Some(id) if !used_ids.contains(&id) => id,
            _ => {
                max_id = max_id.checked_add(1).ok_or_else(|| {
                    core_node_api::Error::Encoding(
                        "cannot assign id for default repository: id space exhausted".to_string(),
                    )
                })?;
                max_id
            }
        };
        if id_to_use > max_id {
            max_id = id_to_use;
        }
        used_ids.insert(id_to_use);

        if let Some(obj) = new_entry.as_object_mut() {
            obj.insert("id".to_string(), Value::Number(id_to_use.into()));
        }
        existing.push(new_entry);
        added += 1;
    }

    if added > 0 {
        existing.sort_by_key(|e| e.get("id").and_then(|v| v.as_u64()).unwrap_or(0));
        let serialized = config::json5_pretty::to_string_pretty(&existing).map_err(|e| {
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
        let mtime_before = std::fs::metadata(&repos_path).unwrap().modified().unwrap();

        // Sleep briefly so a write would change mtime.
        std::thread::sleep(std::time::Duration::from_millis(10));
        ensure_default_repos(&peppy_dirs).unwrap();

        let mtime_after = std::fs::metadata(&repos_path).unwrap().modified().unwrap();
        assert_eq!(
            mtime_before, mtime_after,
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

    /// If a user's existing entry occupies an id used by a default repo, the
    /// missing default is still added but with a freshly assigned id at
    /// `max(existing) + 1`, leaving the user's entry alone.
    #[test]
    fn assigns_fresh_id_when_default_id_collides() {
        let tmp = tempfile::tempdir().unwrap();
        let peppy_dirs = PeppyDirs::new(tmp.path());
        let conf_dir = peppy_dirs.conf_dir();
        std::fs::create_dir_all(&conf_dir).unwrap();
        std::fs::write(
            conf_dir.join("repositories.json5"),
            r#"[
                { "id": 1000, "type": "fs", "path": "/squatting/on/1000" },
                { "id": 1001, "type": "fs", "path": "/squatting/on/1001" },
                { "id": 1002, "type": "fs", "path": "/squatting/on/1002" }
            ]"#,
        )
        .unwrap();

        ensure_default_repos(&peppy_dirs).unwrap();

        let repos = read_repos(&peppy_dirs);
        let nodes_hub = repos
            .iter()
            .find(|e| {
                e.get("type").and_then(|v| v.as_str()) == Some("git")
                    && e.get("url").and_then(|v| v.as_str())
                        == Some("https://github.com/Peppy-bot/nodes_hub")
            })
            .expect("nodes_hub default should be added");
        let nodes_hub_id = nodes_hub.get("id").and_then(|v| v.as_u64()).unwrap();
        assert!(
            nodes_hub_id > 1002,
            "default id 1000 was taken — nodes_hub should have been bumped past existing max, got {nodes_hub_id}"
        );

        // All three squatting fs entries must remain untouched.
        for path in [
            "/squatting/on/1000",
            "/squatting/on/1001",
            "/squatting/on/1002",
        ] {
            assert!(
                repos
                    .iter()
                    .any(|e| e.get("type").and_then(|v| v.as_str()) == Some("fs")
                        && e.get("path").and_then(|v| v.as_str()) == Some(path)),
                "user entry {path} must be preserved"
            );
        }
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
        let serialized = config::json5_pretty::to_string_pretty(&without_launchers).unwrap();
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
