//! Typed loader for `~/.peppy/cache/packages.json5`.
//!
//! The cache file is written by `repo_refresh` (see `write_cache` in
//! `refresh.rs`) and lists every node discovered across every configured
//! repository — FS, Git, or HTTP. This module gives the rest of the daemon
//! a typed view over those entries so callers don't have to dig through
//! `serde_json::Value` every time.

use crate::Result;
use crate::encoding::RepoSourceKind;
use crate::services::repo::refresh::read_or_create_repos;
use config::consts::PeppyDirs;
use serde_json::Value;
use std::path::PathBuf;
use tracing::warn;

/// One entry as it appears in `packages.json5`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackageEntry {
    pub node_name: String,
    pub node_tag: String,
    pub source_type: RepoSourceKind,
    /// Git repository URL or HTTP archive URL. `None` for FS entries.
    pub source_uri: Option<String>,
    /// Short ref name (branch/tag) actually checked out during the last
    /// refresh. `None` for FS and HTTP entries.
    pub resolved_ref: Option<String>,
    /// Absolute path for FS entries; path-within-repo for Git entries.
    pub path: String,
    pub variants: Vec<String>,
    /// True when another repository with a higher-priority (lower) id
    /// already provides this `(name, tag)` pair — duplicates are kept
    /// in the file for `repo list` but are skipped during lookup.
    pub duplicate: bool,
    /// The id of the repository entry this package was discovered
    /// under (as read from `repositories.json5`). Used to break ties
    /// — lower id wins.
    pub repo_id: u32,
}

/// Reads the cache file and returns every entry, tagged with its
/// originating `repo_id`. Missing or malformed files yield an empty
/// vector — the orchestrator layer decides whether "no cache" is a
/// user-facing error for a given call.
pub fn load(peppy_dirs: &PeppyDirs) -> Result<Vec<PackageEntry>> {
    let cache_path = peppy_dirs.cache_dir().join("packages.json5");
    if !cache_path.exists() {
        return Ok(Vec::new());
    }

    let content = std::fs::read_to_string(&cache_path)?;
    let raw: Vec<Value> = serde_json5::from_str(&content).map_err(|e| {
        crate::Error::Decoding(format!(
            "failed to parse packages cache at {}: {e}",
            cache_path.display()
        ))
    })?;

    // Build a URL/path → repo_id map so we can tag each package with its
    // originating repository's id. The repo id list comes from the same
    // `repositories.json5` used elsewhere.
    let repos = read_or_create_repos(peppy_dirs)?;
    let lookup_repo_id = |source_type: RepoSourceKind, uri: Option<&str>, path: &str| -> u32 {
        for entry in &repos {
            let Some(typ) = entry.get("type").and_then(|v| v.as_str()) else {
                continue;
            };
            let matches = match source_type {
                RepoSourceKind::Fs if typ == "fs" => entry
                    .get("path")
                    .and_then(|v| v.as_str())
                    .is_some_and(|p| path.starts_with(p)),
                RepoSourceKind::Git if typ == "git" => {
                    entry.get("url").and_then(|v| v.as_str()) == uri
                }
                RepoSourceKind::Url if typ == "url" => {
                    entry.get("url").and_then(|v| v.as_str()) == uri
                }
                _ => false,
            };
            if matches {
                let id = entry.get("id").and_then(|v| v.as_u64()).unwrap_or(0);
                return u32::try_from(id).unwrap_or(0);
            }
        }
        0
    };

    let mut out = Vec::with_capacity(raw.len());
    for value in raw {
        let Some(entry) = parse_entry(&value, &lookup_repo_id) else {
            warn!("Skipping malformed packages.json5 entry: {:?}", value);
            continue;
        };
        out.push(entry);
    }
    Ok(out)
}

fn parse_entry(
    value: &Value,
    lookup_repo_id: &dyn Fn(RepoSourceKind, Option<&str>, &str) -> u32,
) -> Option<PackageEntry> {
    let node_name = value.get("node_name")?.as_str()?.to_owned();
    let node_tag = value.get("node_tag")?.as_str()?.to_owned();
    let source_type = RepoSourceKind::parse(value.get("source_type")?.as_str()?)?;
    let source_uri = value
        .get("source_uri")
        .and_then(|v| v.as_str())
        .map(|s| s.to_owned());
    let resolved_ref = value
        .get("resolved_ref")
        .and_then(|v| v.as_str())
        .map(|s| s.to_owned());
    let path = value.get("path")?.as_str()?.to_owned();
    let variants = value
        .get("variants")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(|s| s.to_owned()))
                .collect()
        })
        .unwrap_or_default();
    let duplicate = value
        .get("duplicate")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let repo_id = lookup_repo_id(source_type, source_uri.as_deref(), &path);

    Some(PackageEntry {
        node_name,
        node_tag,
        source_type,
        source_uri,
        resolved_ref,
        path,
        variants,
        duplicate,
        repo_id,
    })
}

/// Returns the highest-priority (lowest `repo_id`) non-duplicate entry
/// for `(name, tag)`. Returns `None` when no entry matches.
pub fn lookup<'a>(entries: &'a [PackageEntry], name: &str, tag: &str) -> Option<&'a PackageEntry> {
    entries
        .iter()
        .filter(|e| !e.duplicate && e.node_name == name && e.node_tag == tag)
        .min_by_key(|e| e.repo_id)
}

/// Path to the cache file. Used for user-facing error messages.
pub fn cache_path(peppy_dirs: &PeppyDirs) -> PathBuf {
    peppy_dirs.cache_dir().join("packages.json5")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mk_entry(name: &str, tag: &str, repo_id: u32, duplicate: bool) -> PackageEntry {
        PackageEntry {
            node_name: name.to_owned(),
            node_tag: tag.to_owned(),
            source_type: RepoSourceKind::Fs,
            source_uri: None,
            resolved_ref: None,
            path: "/tmp/foo".to_owned(),
            variants: vec![],
            duplicate,
            repo_id,
        }
    }

    #[test]
    fn lookup_picks_lowest_repo_id() {
        let entries = vec![
            mk_entry("a", "0.1.0", 5, false),
            mk_entry("a", "0.1.0", 2, false),
            mk_entry("a", "0.1.0", 9, false),
        ];
        let hit = lookup(&entries, "a", "0.1.0").unwrap();
        assert_eq!(hit.repo_id, 2);
    }

    #[test]
    fn lookup_skips_duplicate_entries() {
        let entries = vec![
            mk_entry("a", "0.1.0", 1, true),
            mk_entry("a", "0.1.0", 3, false),
        ];
        let hit = lookup(&entries, "a", "0.1.0").unwrap();
        assert_eq!(hit.repo_id, 3);
    }

    #[test]
    fn lookup_returns_none_when_all_duplicates() {
        let entries = vec![mk_entry("a", "0.1.0", 1, true)];
        assert!(lookup(&entries, "a", "0.1.0").is_none());
    }

    #[test]
    fn load_returns_empty_when_file_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let peppy_dirs = PeppyDirs::new(tmp.path());
        let entries = load(&peppy_dirs).unwrap();
        assert!(entries.is_empty());
    }
}
