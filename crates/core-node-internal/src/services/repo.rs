mod add;
pub(crate) mod cache;
mod exclude;
mod init;
mod list;
mod refresh;
mod remove;

pub use add::listen_for_repo_add;
pub use exclude::listen_for_repo_exclude;
pub use init::{InitOutcome, ensure_default_repos};
pub use list::listen_for_repo_list;
pub use refresh::listen_for_repo_refresh;
pub use remove::listen_for_repo_remove;

use core_node_api::encoding::RepoSource;
use serde_json::Value;
use std::collections::HashSet;
use std::path::Path;

/// Guards read-modify-write cycles on repositories.json5 and
/// excluded_repositories.json5 to prevent concurrent corruption.
pub(crate) fn repos_file_lock() -> &'static parking_lot::Mutex<()> {
    static LOCK: std::sync::OnceLock<parking_lot::Mutex<()>> = std::sync::OnceLock::new();
    LOCK.get_or_init(|| parking_lot::Mutex::new(()))
}

/// Serializes `process_refresh` + `write_all_caches` so the user-facing repo_refresh
/// action and the post-remove refresh in repo_remove cannot race on
/// nodes.json5. The ActionState single-flight inside repo_refresh rejects
/// concurrent *user* refreshes with a friendly error; this mutex is the
/// correctness backstop that also covers the remove-triggered path.
pub(crate) fn refresh_lock() -> &'static parking_lot::Mutex<()> {
    static LOCK: std::sync::OnceLock<parking_lot::Mutex<()>> = std::sync::OnceLock::new();
    LOCK.get_or_init(|| parking_lot::Mutex::new(()))
}

/// Serialize a `RepoSource` with an assigned id into a JSON object for
/// persisting in repositories.json5 / excluded_repositories.json5.
pub(crate) fn repo_source_to_json(id: u64, source: &RepoSource) -> Value {
    let mut map = serde_json::Map::new();
    map.insert("id".to_string(), Value::Number(id.into()));
    match source {
        RepoSource::Fs(path) => {
            map.insert("type".to_string(), Value::String("fs".to_string()));
            map.insert(
                "path".to_string(),
                Value::String(path.to_string_lossy().into_owned()),
            );
        }
        RepoSource::Git { repo_url, repo_ref } => {
            map.insert("type".to_string(), Value::String("git".to_string()));
            map.insert("url".to_string(), Value::String(repo_url.clone()));
            if let Some(r) = repo_ref {
                map.insert("ref".to_string(), Value::String(r.to_string()));
            }
        }
        RepoSource::Url(url) => {
            map.insert("type".to_string(), Value::String("url".to_string()));
            map.insert("url".to_string(), Value::String(url.clone()));
        }
    }
    Value::Object(map)
}

/// The canonical identity string for a [`RepoSource`], used for duplicate
/// detection and exclusion matching. Lives here (the daemon) rather than in
/// `core-node-api`: the `Fs` arm canonicalizes against the real filesystem,
/// which a pure wire-codec crate should not do.
///
/// - `Fs`: canonicalized (absolute, symlink-resolved) when possible, so that
///   `./repo` and `/abs/path/to/repo` produce the same identity. Falls back to
///   the raw string when the path does not exist.
/// - `Git`: `repo_url@repo_ref` when a non-empty ref is present, otherwise just
///   the url, so the same repo pinned to different refs is not collapsed.
/// - `Url`: the url as-is.
///
/// Must stay in sync with [`json_entry_identity`], the JSON-entry equivalent.
pub(crate) fn source_identity(source: &RepoSource) -> String {
    match source {
        RepoSource::Fs(path) => std::fs::canonicalize(path)
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|_| path.to_string_lossy().into_owned()),
        RepoSource::Git { repo_url, repo_ref } => match repo_ref {
            Some(r) if !r.is_empty() => format!("{repo_url}@{r}"),
            _ => repo_url.clone(),
        },
        RepoSource::Url(url) => url.clone(),
    }
}

/// Returns the canonical identity for a persisted JSON repo entry.
///
/// Must stay in sync with [`source_identity`]:
/// - `fs`: canonicalized path when possible (falls back to raw string).
/// - `git`: `url@ref` when a non-empty `ref` field is present, otherwise `url`.
/// - other (`url`): the url as-is.
pub(crate) fn json_entry_identity(entry: &Value) -> Option<String> {
    let typ = entry.get("type")?.as_str()?;
    match typ {
        "fs" => {
            let path = entry.get("path")?.as_str()?;
            let canonical = std::fs::canonicalize(path)
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_else(|_| path.to_string());
            Some(canonical)
        }
        "git" => {
            let url = entry.get("url")?.as_str()?;
            match entry.get("ref").and_then(|v| v.as_str()) {
                Some(r) if !r.is_empty() => Some(format!("{url}@{r}")),
                _ => Some(url.to_string()),
            }
        }
        _ => entry.get("url")?.as_str().map(|s| s.to_string()),
    }
}

/// Normalize a list of repo JSON entries: auto-assign missing `id` fields,
/// detect duplicate ids, sort by id, and write back if any ids were assigned.
pub(crate) fn normalize_repo_entries(
    repos: &mut Vec<Value>,
    file_path: &Path,
    desc: &str,
) -> crate::Result<()> {
    let mut max_id: u64 = repos
        .iter()
        .filter_map(|e| e.get("id").and_then(|v| v.as_u64()))
        .max()
        .unwrap_or(0);

    let mut needs_write = false;
    for entry in repos.iter_mut() {
        if entry.get("id").and_then(|v| v.as_u64()).is_none() {
            max_id += 1;
            if let Some(obj) = entry.as_object_mut() {
                obj.insert("id".to_string(), Value::Number(max_id.into()));
                needs_write = true;
            }
        }
    }

    // Detect duplicate ids; a user may manually edit the file and introduce collisions.
    let mut seen_ids = HashSet::new();
    for entry in repos.iter() {
        if let Some(id) = entry.get("id").and_then(|v| v.as_u64())
            && !seen_ids.insert(id)
        {
            let file = file_path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| file_path.to_string_lossy().into_owned());
            return Err(crate::Error::DuplicateRepoId { id, file });
        }
    }

    let ids_before: Vec<u64> = repos
        .iter()
        .map(|e| e.get("id").and_then(|v| v.as_u64()).unwrap_or(0))
        .collect();
    repos.sort_by_key(|e| e.get("id").and_then(|v| v.as_u64()).unwrap_or(0));
    let ids_after: Vec<u64> = repos
        .iter()
        .map(|e| e.get("id").and_then(|v| v.as_u64()).unwrap_or(0))
        .collect();
    if ids_before != ids_after {
        needs_write = true;
    }

    if needs_write {
        let content = json5_pretty::to_string_pretty(repos).map_err(|e| {
            core_node_api::Error::Encoding(format!("failed to serialize {desc}: {e}"))
        })?;
        std::fs::write(file_path, content)?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    // Coverage for `source_identity` (relocated here from `core-node-api`
    // alongside the function, which moved out of the pure wire-codec crate
    // because its `Fs` arm canonicalizes against the real filesystem).
    use super::source_identity;
    use core_node_api::encoding::RepoSource;

    #[test]
    fn identity_git_distinguishes_refs() {
        let a = RepoSource::Git {
            repo_url: "https://github.com/org/repo".to_string(),
            repo_ref: Some("main".to_string()),
        };
        let b = RepoSource::Git {
            repo_url: "https://github.com/org/repo".to_string(),
            repo_ref: Some("dev".to_string()),
        };
        assert_ne!(source_identity(&a), source_identity(&b));
        assert!(source_identity(&a).contains("main"));
        assert!(source_identity(&b).contains("dev"));
    }

    #[test]
    fn identity_git_without_ref_matches_url() {
        let src = RepoSource::Git {
            repo_url: "https://github.com/org/repo".to_string(),
            repo_ref: None,
        };
        assert_eq!(source_identity(&src), "https://github.com/org/repo");
    }

    #[test]
    fn identity_git_empty_ref_matches_url() {
        // Treat empty ref as "no ref" so it matches legacy entries without a ref.
        let src = RepoSource::Git {
            repo_url: "https://github.com/org/repo".to_string(),
            repo_ref: Some(String::new()),
        };
        assert_eq!(source_identity(&src), "https://github.com/org/repo");
    }

    #[test]
    fn identity_fs_canonicalizes_existing_path() {
        let tmp = tempfile::tempdir().unwrap();
        let canonical = std::fs::canonicalize(tmp.path()).unwrap();

        // Build a non-canonical spelling: `<canonical>/../<basename>`.
        let parent = canonical.parent().unwrap();
        let name = canonical.file_name().unwrap();
        let roundabout = parent
            .join("..")
            .join(parent.file_name().unwrap())
            .join(name);

        let raw = RepoSource::Fs(roundabout);
        let canon = RepoSource::Fs(canonical);
        assert_eq!(
            source_identity(&raw),
            source_identity(&canon),
            "canonicalization must collapse equivalent paths"
        );
    }

    #[test]
    fn identity_fs_nonexistent_falls_back_to_raw() {
        let tmp = tempfile::tempdir().unwrap();
        let missing = tmp
            .path()
            .join("definitely")
            .join("does-not-exist")
            .join("xyz");
        let src = RepoSource::Fs(missing.clone());
        assert_eq!(
            source_identity(&src),
            missing.to_string_lossy().into_owned()
        );
    }

    #[test]
    fn identity_url_is_unchanged() {
        let src = RepoSource::Url("https://example.com/packages".to_string());
        assert_eq!(source_identity(&src), "https://example.com/packages");
    }
}
