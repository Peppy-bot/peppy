//! Typed loader for `~/.peppy/cache/packages.json5`.
//!
//! The cache file is written by `repo_refresh` (see `write_cache`) and
//! lists every node discovered across every configured repository — FS,
//! Git, or HTTP. This module gives the rest of the daemon a typed view
//! over those entries so callers don't have to dig through
//! `serde_json::Value` every time.
//!
//! Reads are memoized by `(mtime-of-packages.json5)` per path so that a
//! daemon hit by many `node add` goals in a row doesn't re-read and
//! re-parse the cache file on every request.

use crate::Result;
use crate::encoding::RepoSourceKind;
use crate::services::repo::refresh::read_or_create_repos;
use config::consts::PeppyDirs;
use parking_lot::Mutex;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::SystemTime;
use tracing::warn;

/// One entry as it appears in `packages.json5`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PackageEntry {
    pub node_name: String,
    pub node_tag: String,
    pub source_type: RepoSourceKind,
    /// Git repository URL or HTTP archive URL. `None` for FS entries.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_uri: Option<String>,
    /// Short ref name (branch/tag) actually checked out during the last
    /// refresh. `None` for FS and HTTP entries.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolved_ref: Option<String>,
    /// Absolute path for FS entries; path-within-repo for Git entries.
    pub path: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub variants: Vec<String>,
    /// True when another repository with a higher-priority (lower) id
    /// already provides this `(name, tag)` pair — duplicates are kept
    /// in the file for `repo list` but are skipped during lookup.
    #[serde(default, skip_serializing_if = "is_false")]
    pub duplicate: bool,
    /// The id of the repository entry this package was discovered
    /// under (as read from `repositories.json5`). Derived at read time
    /// and never serialized back to disk.
    #[serde(skip)]
    pub repo_id: u32,
}

fn is_false(b: &bool) -> bool {
    !*b
}

/// Reads the cache file plus the `packages.json5` generation used for
/// the read.
pub fn load_with_generation(
    peppy_dirs: &PeppyDirs,
) -> Result<(Vec<PackageEntry>, Option<SystemTime>)> {
    let path = cache_path(peppy_dirs);
    let generation = std::fs::metadata(&path)
        .ok()
        .and_then(|m| m.modified().ok());

    if let Some(mtime) = generation
        && let Some(cached) = memo_get(&path, mtime)
    {
        return Ok(((*cached).clone(), Some(mtime)));
    }

    if !path.exists() {
        return Ok((Vec::new(), None));
    }

    let content = std::fs::read_to_string(&path)?;
    let raw: Vec<PackageEntry> = serde_json5::from_str(&content).map_err(|e| {
        crate::Error::Decoding(format!(
            "failed to parse packages cache at {}: {e}",
            path.display()
        ))
    })?;

    // Build a URL/path → repo_id map so we can tag each package with its
    // originating repository's id. Missing matches default to 0 (highest
    // priority) to preserve previous behavior for hand-written caches.
    let repos = read_or_create_repos(peppy_dirs)?;
    let mut entries: Vec<PackageEntry> = raw
        .into_iter()
        .map(|mut e| {
            e.repo_id = lookup_repo_id(&repos, e.source_type, e.source_uri.as_deref(), &e.path);
            e
        })
        .collect();
    entries.retain(|e| {
        let ok = !e.node_name.is_empty() && !e.node_tag.is_empty() && !e.path.is_empty();
        if !ok {
            warn!(
                "Skipping malformed packages.json5 entry: {:?}:{:?}",
                e.node_name, e.node_tag
            );
        }
        ok
    });

    if let Some(mtime) = generation {
        memo_put(&path, mtime, entries.clone());
    }
    Ok((entries, generation))
}

/// Write cached node information for git/url repositories.
///
/// The write is performed via a sibling temp file + atomic rename so that
/// concurrent readers (see [`load_with_generation`]) never observe a
/// truncated or partially-written `packages.json5`.
pub(crate) fn write_cache(peppy_dirs: &PeppyDirs, nodes: &[PackageEntry]) -> Result<()> {
    let cache_dir = peppy_dirs.cache_dir();
    std::fs::create_dir_all(&cache_dir)?;
    let content = serde_json::to_string_pretty(nodes)
        .map_err(|e| crate::Error::Encoding(format!("failed to serialize cache: {e}")))?;
    let final_path = cache_dir.join("packages.json5");
    let tmp_path = cache_dir.join("packages.json5.tmp");
    std::fs::write(&tmp_path, content)?;
    std::fs::rename(&tmp_path, &final_path)?;
    Ok(())
}

fn lookup_repo_id(
    repos: &[serde_json::Value],
    source_type: RepoSourceKind,
    uri: Option<&str>,
    path: &str,
) -> u32 {
    for entry in repos {
        let Some(typ) = entry.get("type").and_then(|v| v.as_str()) else {
            continue;
        };
        let matches = match source_type {
            RepoSourceKind::Fs if typ == "fs" => entry
                .get("path")
                .and_then(|v| v.as_str())
                .is_some_and(|p| Path::new(path).starts_with(Path::new(p))),
            RepoSourceKind::Git if typ == "git" => entry.get("url").and_then(|v| v.as_str()) == uri,
            RepoSourceKind::Url if typ == "url" => entry.get("url").and_then(|v| v.as_str()) == uri,
            _ => false,
        };
        if matches {
            let id = entry.get("id").and_then(|v| v.as_u64()).unwrap_or(0);
            return u32::try_from(id).unwrap_or(0);
        }
    }
    0
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

struct MemoEntry {
    mtime: SystemTime,
    entries: Arc<Vec<PackageEntry>>,
}

fn memo_map() -> &'static Mutex<HashMap<PathBuf, MemoEntry>> {
    static MAP: OnceLock<Mutex<HashMap<PathBuf, MemoEntry>>> = OnceLock::new();
    MAP.get_or_init(|| Mutex::new(HashMap::new()))
}

fn memo_get(path: &Path, mtime: SystemTime) -> Option<Arc<Vec<PackageEntry>>> {
    let map = memo_map().lock();
    map.get(path)
        .filter(|e| e.mtime == mtime)
        .map(|e| Arc::clone(&e.entries))
}

fn memo_put(path: &Path, mtime: SystemTime, entries: Vec<PackageEntry>) {
    memo_map().lock().insert(
        path.to_path_buf(),
        MemoEntry {
            mtime,
            entries: Arc::new(entries),
        },
    );
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
        let (entries, generation) = load_with_generation(&peppy_dirs).unwrap();
        assert!(entries.is_empty());
        assert!(generation.is_none());
    }

    #[test]
    fn write_then_load_roundtrips_entries() {
        let tmp = tempfile::tempdir().unwrap();
        let peppy_dirs = PeppyDirs::new(tmp.path());
        let input = vec![
            PackageEntry {
                node_name: "a".to_owned(),
                node_tag: "0.1.0".to_owned(),
                source_type: RepoSourceKind::Git,
                source_uri: Some("https://example.com/repo.git".to_owned()),
                resolved_ref: Some("main".to_owned()),
                path: "nodes/a".to_owned(),
                variants: vec!["sim".to_owned()],
                duplicate: false,
                repo_id: 0,
            },
            PackageEntry {
                node_name: "b".to_owned(),
                node_tag: "0.2.0".to_owned(),
                source_type: RepoSourceKind::Fs,
                source_uri: None,
                resolved_ref: None,
                path: "/tmp/b".to_owned(),
                variants: vec![],
                duplicate: true,
                repo_id: 0,
            },
        ];
        write_cache(&peppy_dirs, &input).unwrap();
        let (loaded, generation) = load_with_generation(&peppy_dirs).unwrap();
        assert!(generation.is_some());
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[0].node_name, "a");
        assert_eq!(loaded[0].variants, vec!["sim".to_owned()]);
        assert_eq!(loaded[0].resolved_ref.as_deref(), Some("main"));
        assert_eq!(loaded[1].node_name, "b");
        assert!(loaded[1].duplicate);
    }
}
