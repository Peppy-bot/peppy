//! Typed loader for `~/.peppy/cache/nodes.json5` and
//! `~/.peppy/cache/launchers.json5`.
//!
//! The cache files are written by `repo_refresh` (see `write_cache` /
//! `write_launcher_cache`) and list every node and launcher discovered
//! across every configured repository — FS, Git, or HTTP. This module
//! gives the rest of the daemon a typed view over those entries so
//! callers don't have to dig through `serde_json::Value` every time.
//!
//! Reads are memoized by `(mtime-of-cache-file)` per path so that a
//! daemon hit by many `node add` / launch goals in a row doesn't
//! re-read and re-parse the cache file on every request.

use crate::Result;
use crate::services::repo::refresh::read_or_create_repos;
use config::consts::PeppyDirs;
use core_node_api::encoding::{NodeSource, RepoSourceKind};
use parking_lot::Mutex;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::SystemTime;
use tracing::warn;

/// One entry as it appears in `nodes.json5`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct NodeCacheEntry {
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
    /// Recorded SHA-256 for URL-kind entries, when the repository entry
    /// pinned one at registration time. `None` for FS and Git entries,
    /// and for URL entries whose repository did not declare a checksum.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checksum: Option<String>,
    /// Absolute path for FS entries; path-within-repo for Git entries.
    pub path: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub variants: Vec<String>,
    /// True when another repository with a higher-priority (lower) id
    /// already provides this `(name, tag)` pair — duplicates are kept
    /// in the file for `repo list` but are skipped during lookup.
    #[serde(default, skip_serializing_if = "is_false")]
    pub duplicate: bool,
    /// The id of the repository entry this node was discovered
    /// under (as read from `repositories.json5`). Derived at read time
    /// and never serialized back to disk.
    #[serde(skip)]
    pub repo_id: u32,
}

/// One entry as it appears in `launchers.json5`. Launchers live in the
/// same kind of repositories as nodes (FS or Git), but they don't carry
/// a tag, variants, or a checksum — they're just the location of a
/// `peppy_launcher.json5` file by name.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct LauncherCacheEntry {
    /// Name of the launcher (file stem of the `.json5` file).
    pub launcher_name: String,
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
    /// True when another repository with a higher-priority (lower) id
    /// already provides this name.
    #[serde(default, skip_serializing_if = "is_false")]
    pub duplicate: bool,
    /// The id of the repository entry this launcher was discovered
    /// under (as read from `repositories.json5`). Derived at read time
    /// and never serialized back to disk.
    #[serde(skip)]
    pub repo_id: u32,
}

fn is_false(b: &bool) -> bool {
    !*b
}

/// Reads the cache file plus the `nodes.json5` generation used for
/// the read.
pub fn load_with_generation(
    peppy_dirs: &PeppyDirs,
) -> Result<(Vec<NodeCacheEntry>, Option<SystemTime>)> {
    let path = nodes_repo_cache_path(peppy_dirs);
    let generation = std::fs::metadata(&path)
        .ok()
        .and_then(|m| m.modified().ok());
    // `repo_id` on each cached entry is derived from `repositories.json5`,
    // so the memo must invalidate when that file changes too (not just
    // when nodes.json5 is rewritten). Missing file → UNIX_EPOCH so any
    // future appearance counts as a change.
    let repos_mtime = repositories_mtime(peppy_dirs);

    if let Some(mtime) = generation
        && let Some(cached) = memo_get(&path, mtime, repos_mtime)
    {
        return Ok(((*cached).clone(), Some(mtime)));
    }

    if !path.exists() {
        return Ok((Vec::new(), None));
    }

    let content = std::fs::read_to_string(&path)?;
    let raw: Vec<NodeCacheEntry> = serde_json5::from_str(&content).map_err(|e| {
        core_node_api::Error::Decoding(format!(
            "failed to parse nodes cache at {}: {e}",
            path.display()
        ))
    })?;

    // Build a URL/path → repo_id map so we can tag each node with its
    // originating repository's id. Missing matches default to 0 (highest
    // priority) to preserve previous behavior for hand-written caches.
    let repos = read_or_create_repos(peppy_dirs)?;
    let mut entries: Vec<NodeCacheEntry> = raw
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
                "Skipping malformed nodes.json5 entry: {:?}:{:?}",
                e.node_name, e.node_tag
            );
        }
        ok
    });

    if let Some(mtime) = generation {
        memo_put(&path, mtime, repos_mtime, entries.clone());
    }
    Ok((entries, generation))
}

fn repositories_mtime(peppy_dirs: &PeppyDirs) -> SystemTime {
    let path = peppy_dirs.conf_dir().join("repositories.json5");
    std::fs::metadata(&path)
        .ok()
        .and_then(|m| m.modified().ok())
        .unwrap_or(SystemTime::UNIX_EPOCH)
}

/// Write cached node information for git/url repositories.
///
/// The write is performed via a sibling temp file + atomic rename so that
/// concurrent readers (see [`load_with_generation`]) never observe a
/// truncated or partially-written `nodes.json5`.
pub(crate) fn write_cache(peppy_dirs: &PeppyDirs, nodes: &[NodeCacheEntry]) -> Result<()> {
    let cache_dir = peppy_dirs.cache_dir();
    std::fs::create_dir_all(&cache_dir)?;
    let content = serde_json::to_string_pretty(nodes)
        .map_err(|e| core_node_api::Error::Encoding(format!("failed to serialize cache: {e}")))?;
    let final_path = nodes_repo_cache_path(peppy_dirs);
    let tmp_path = cache_dir.join("nodes.json5.tmp");
    std::fs::write(&tmp_path, content)?;
    std::fs::rename(&tmp_path, &final_path)?;
    Ok(())
}

/// Write cached launcher information for git/url/fs repositories.
///
/// The write is performed via a sibling temp file + atomic rename so
/// that concurrent readers never observe a truncated or partially-written
/// file.
pub(crate) fn write_launcher_cache(
    peppy_dirs: &PeppyDirs,
    launchers: &[LauncherCacheEntry],
) -> Result<()> {
    let cache_dir = peppy_dirs.cache_dir();
    std::fs::create_dir_all(&cache_dir)?;
    let content = serde_json::to_string_pretty(launchers).map_err(|e| {
        core_node_api::Error::Encoding(format!("failed to serialize launcher cache: {e}"))
    })?;
    let final_path = launchers_repo_cache_path(peppy_dirs);
    let tmp_path = cache_dir.join("launchers.json5.tmp");
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
pub fn lookup<'a>(
    entries: &'a [NodeCacheEntry],
    name: &str,
    tag: &str,
) -> Option<&'a NodeCacheEntry> {
    entries
        .iter()
        .filter(|e| !e.duplicate && e.node_name == name && e.node_tag == tag)
        .min_by_key(|e| e.repo_id)
}

pub fn nodes_repo_cache_path(peppy_dirs: &PeppyDirs) -> PathBuf {
    peppy_dirs.cache_dir().join("nodes.json5")
}

pub fn launchers_repo_cache_path(peppy_dirs: &PeppyDirs) -> PathBuf {
    peppy_dirs.cache_dir().join("launchers.json5")
}

/// Looks up `(name, tag)` in the nodes cache and translates the matched
/// entry into a concrete `NodeSource` (Fs / Git / Http) that downstream
/// resolution can handle directly.
pub(crate) fn resolve_repo_node_source(
    name: &str,
    tag: &str,
    peppy_dirs: &PeppyDirs,
) -> std::result::Result<NodeSource, String> {
    let (entries, _) =
        load_with_generation(peppy_dirs).map_err(|e| format!("failed to load nodes cache: {e}"))?;
    let id = format!("{name}:{tag}");
    let entry = lookup(&entries, name, tag).ok_or_else(|| {
        format!(
            "repo-node `{id}` not found in {}",
            nodes_repo_cache_path(peppy_dirs).display()
        )
    })?;

    match entry.source_type {
        RepoSourceKind::Fs => Ok(NodeSource::Fs(PathBuf::from(&entry.path))),
        RepoSourceKind::Git => {
            let repo_url_str = entry
                .source_uri
                .as_deref()
                .ok_or_else(|| format!("cache entry for `{id}` is git but has no source_uri"))?;
            let repo_url = gix_url::Url::try_from(repo_url_str)
                .map_err(|e| format!("invalid git URL in cache entry for `{id}`: {e}"))?;
            let repo_ref = entry
                .resolved_ref
                .clone()
                .ok_or_else(|| format!("cache entry for `{id}` is git but has no resolved_ref"))?;
            Ok(NodeSource::Git {
                repo_url,
                repo_path: entry.path.clone(),
                repo_ref: Some(repo_ref),
            })
        }
        RepoSourceKind::Url => {
            let url_str = entry
                .source_uri
                .as_deref()
                .ok_or_else(|| format!("cache entry for `{id}` is url but has no source_uri"))?;
            let url = url::Url::parse(url_str)
                .map_err(|e| format!("invalid url in cache entry for `{id}`: {e}"))?;
            Ok(NodeSource::Http {
                url,
                sha256: entry.checksum.clone(),
            })
        }
    }
}

struct MemoEntry {
    mtime: SystemTime,
    repos_mtime: SystemTime,
    entries: Arc<Vec<NodeCacheEntry>>,
}

fn memo_map() -> &'static Mutex<HashMap<PathBuf, MemoEntry>> {
    static MAP: OnceLock<Mutex<HashMap<PathBuf, MemoEntry>>> = OnceLock::new();
    MAP.get_or_init(|| Mutex::new(HashMap::new()))
}

fn memo_get(
    path: &Path,
    mtime: SystemTime,
    repos_mtime: SystemTime,
) -> Option<Arc<Vec<NodeCacheEntry>>> {
    let map = memo_map().lock();
    map.get(path)
        .filter(|e| e.mtime == mtime && e.repos_mtime == repos_mtime)
        .map(|e| Arc::clone(&e.entries))
}

fn memo_put(path: &Path, mtime: SystemTime, repos_mtime: SystemTime, entries: Vec<NodeCacheEntry>) {
    memo_map().lock().insert(
        path.to_path_buf(),
        MemoEntry {
            mtime,
            repos_mtime,
            entries: Arc::new(entries),
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mk_entry(name: &str, tag: &str, repo_id: u32, duplicate: bool) -> NodeCacheEntry {
        NodeCacheEntry {
            node_name: name.to_owned(),
            node_tag: tag.to_owned(),
            source_type: RepoSourceKind::Fs,
            source_uri: None,
            resolved_ref: None,
            checksum: None,
            path: "/tmp/foo".to_owned(),
            variants: vec![],
            duplicate,
            repo_id,
        }
    }

    fn mk_fs_entry(name: &str, tag: &str, path: &str) -> NodeCacheEntry {
        NodeCacheEntry {
            node_name: name.to_owned(),
            node_tag: tag.to_owned(),
            source_type: RepoSourceKind::Fs,
            source_uri: None,
            resolved_ref: None,
            checksum: None,
            path: path.to_owned(),
            variants: vec![],
            duplicate: false,
            repo_id: 0,
        }
    }

    fn mk_git_entry(
        name: &str,
        tag: &str,
        uri: Option<&str>,
        resolved_ref: Option<&str>,
    ) -> NodeCacheEntry {
        NodeCacheEntry {
            node_name: name.to_owned(),
            node_tag: tag.to_owned(),
            source_type: RepoSourceKind::Git,
            source_uri: uri.map(str::to_owned),
            resolved_ref: resolved_ref.map(str::to_owned),
            checksum: None,
            path: "nodes/example".to_owned(),
            variants: vec![],
            duplicate: false,
            repo_id: 0,
        }
    }

    fn mk_url_entry(
        name: &str,
        tag: &str,
        uri: Option<&str>,
        checksum: Option<&str>,
    ) -> NodeCacheEntry {
        NodeCacheEntry {
            node_name: name.to_owned(),
            node_tag: tag.to_owned(),
            source_type: RepoSourceKind::Url,
            source_uri: uri.map(str::to_owned),
            resolved_ref: None,
            checksum: checksum.map(str::to_owned),
            path: "nodes/example".to_owned(),
            variants: vec![],
            duplicate: false,
            repo_id: 0,
        }
    }

    fn mk_launcher_entry(
        name: &str,
        source_type: RepoSourceKind,
        uri: Option<&str>,
        resolved_ref: Option<&str>,
        path: &str,
        repo_id: u32,
        duplicate: bool,
    ) -> LauncherCacheEntry {
        LauncherCacheEntry {
            launcher_name: name.to_owned(),
            source_type,
            source_uri: uri.map(str::to_owned),
            resolved_ref: resolved_ref.map(str::to_owned),
            path: path.to_owned(),
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
            NodeCacheEntry {
                node_name: "a".to_owned(),
                node_tag: "0.1.0".to_owned(),
                source_type: RepoSourceKind::Git,
                source_uri: Some("https://example.com/repo.git".to_owned()),
                resolved_ref: Some("main".to_owned()),
                checksum: None,
                path: "nodes/a".to_owned(),
                variants: vec!["sim".to_owned()],
                duplicate: false,
                repo_id: 0,
            },
            NodeCacheEntry {
                node_name: "b".to_owned(),
                node_tag: "0.2.0".to_owned(),
                source_type: RepoSourceKind::Fs,
                source_uri: None,
                resolved_ref: None,
                checksum: None,
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

    // -- resolve_repo_node_source tests --

    #[test]
    fn resolve_fs_success() {
        let tmp = tempfile::tempdir().unwrap();
        let peppy_dirs = PeppyDirs::new(tmp.path());
        let entries = vec![mk_fs_entry("mynode", "1.0", "/tmp/mynode")];
        write_cache(&peppy_dirs, &entries).unwrap();

        let src = resolve_repo_node_source("mynode", "1.0", &peppy_dirs).unwrap();
        assert_eq!(src, NodeSource::Fs(PathBuf::from("/tmp/mynode")));
    }

    #[test]
    fn resolve_git_success() {
        let tmp = tempfile::tempdir().unwrap();
        let peppy_dirs = PeppyDirs::new(tmp.path());
        let entries = vec![mk_git_entry(
            "g",
            "1.0",
            Some("https://example.com/repo.git"),
            Some("main"),
        )];
        write_cache(&peppy_dirs, &entries).unwrap();

        let src = resolve_repo_node_source("g", "1.0", &peppy_dirs).unwrap();
        match src {
            NodeSource::Git {
                repo_url,
                repo_path,
                repo_ref,
            } => {
                assert!(
                    repo_url
                        .to_bstring()
                        .to_string()
                        .contains("example.com/repo.git"),
                    "unexpected repo_url: {repo_url:?}",
                );
                assert_eq!(repo_path, "nodes/example");
                assert_eq!(repo_ref.as_deref(), Some("main"));
            }
            other => panic!("expected NodeSource::Git, got {other:?}"),
        }
    }

    #[test]
    fn resolve_url_success() {
        let tmp = tempfile::tempdir().unwrap();
        let peppy_dirs = PeppyDirs::new(tmp.path());
        let entries = vec![mk_url_entry(
            "u",
            "2.0",
            Some("https://example.com/archive.tzst"),
            Some("abc123"),
        )];
        write_cache(&peppy_dirs, &entries).unwrap();

        let src = resolve_repo_node_source("u", "2.0", &peppy_dirs).unwrap();
        match src {
            NodeSource::Http { url, sha256 } => {
                assert_eq!(url.as_str(), "https://example.com/archive.tzst");
                assert_eq!(sha256.as_deref(), Some("abc123"));
            }
            other => panic!("expected NodeSource::Http, got {other:?}"),
        }
    }

    #[test]
    fn resolve_git_missing_source_uri() {
        let tmp = tempfile::tempdir().unwrap();
        let peppy_dirs = PeppyDirs::new(tmp.path());
        let entries = vec![mk_git_entry("g", "1.0", None, Some("main"))];
        write_cache(&peppy_dirs, &entries).unwrap();

        let err = resolve_repo_node_source("g", "1.0", &peppy_dirs).unwrap_err();
        assert!(err.contains("no source_uri"), "unexpected error: {err}");
    }

    #[test]
    fn resolve_git_invalid_source_uri() {
        let tmp = tempfile::tempdir().unwrap();
        let peppy_dirs = PeppyDirs::new(tmp.path());
        let entries = vec![mk_git_entry("g", "1.0", Some(""), Some("main"))];
        write_cache(&peppy_dirs, &entries).unwrap();

        let err = resolve_repo_node_source("g", "1.0", &peppy_dirs).unwrap_err();
        assert!(err.contains("invalid git URL"), "unexpected error: {err}",);
    }

    #[test]
    fn resolve_git_missing_resolved_ref() {
        let tmp = tempfile::tempdir().unwrap();
        let peppy_dirs = PeppyDirs::new(tmp.path());
        let entries = vec![mk_git_entry(
            "g",
            "1.0",
            Some("https://example.com/repo.git"),
            None,
        )];
        write_cache(&peppy_dirs, &entries).unwrap();

        let err = resolve_repo_node_source("g", "1.0", &peppy_dirs).unwrap_err();
        assert!(err.contains("no resolved_ref"), "unexpected error: {err}");
    }

    #[test]
    fn resolve_url_missing_source_uri() {
        let tmp = tempfile::tempdir().unwrap();
        let peppy_dirs = PeppyDirs::new(tmp.path());
        let entries = vec![mk_url_entry("u", "1.0", None, None)];
        write_cache(&peppy_dirs, &entries).unwrap();

        let err = resolve_repo_node_source("u", "1.0", &peppy_dirs).unwrap_err();
        assert!(err.contains("no source_uri"), "unexpected error: {err}");
    }

    #[test]
    fn resolve_url_invalid_source_uri() {
        let tmp = tempfile::tempdir().unwrap();
        let peppy_dirs = PeppyDirs::new(tmp.path());
        let entries = vec![mk_url_entry("u", "1.0", Some("not://[invalid"), None)];
        write_cache(&peppy_dirs, &entries).unwrap();

        let err = resolve_repo_node_source("u", "1.0", &peppy_dirs).unwrap_err();
        assert!(err.contains("invalid url"), "unexpected error: {err}");
    }

    #[test]
    fn resolve_not_found() {
        let tmp = tempfile::tempdir().unwrap();
        let peppy_dirs = PeppyDirs::new(tmp.path());
        write_cache(&peppy_dirs, &[]).unwrap();

        let err = resolve_repo_node_source("missing", "0.0", &peppy_dirs).unwrap_err();
        assert!(err.contains("not found"), "unexpected error: {err}");
    }

    // -- launcher cache tests --

    /// `write_launcher_cache` writes to the path returned by
    /// `launchers_repo_cache_path` with the on-disk launcher schema.
    #[test]
    fn write_launcher_cache_serializes_entries() {
        let tmp = tempfile::tempdir().unwrap();
        let peppy_dirs = PeppyDirs::new(tmp.path());
        let entries = vec![
            mk_launcher_entry(
                "openarm01_sim_teleop",
                RepoSourceKind::Git,
                Some("https://github.com/Peppy-bot/launchers_hub"),
                Some("main"),
                "openarm01_sim_teleop.json5",
                0,
                false,
            ),
            mk_launcher_entry(
                "local_demo",
                RepoSourceKind::Fs,
                None,
                None,
                "/tmp/local_demo.json5",
                0,
                false,
            ),
        ];
        write_launcher_cache(&peppy_dirs, &entries).unwrap();

        let path = launchers_repo_cache_path(&peppy_dirs);
        assert!(
            path.exists(),
            "launcher cache file should exist at {}",
            path.display()
        );

        let raw = std::fs::read_to_string(&path).expect("read launcher cache");
        let parsed: serde_json::Value =
            serde_json::from_str(&raw).expect("launcher cache should be valid JSON");
        let arr = parsed.as_array().expect("expected array");
        assert_eq!(arr.len(), 2);
        assert_eq!(arr[0]["launcher_name"], "openarm01_sim_teleop");
        assert_eq!(
            arr[0]["source_uri"],
            "https://github.com/Peppy-bot/launchers_hub"
        );
        assert_eq!(arr[0]["resolved_ref"], "main");
        assert_eq!(arr[1]["launcher_name"], "local_demo");
        assert_eq!(arr[1]["source_type"], "fs");
    }

    /// The write is atomic: the final file is created via a tmp + rename
    /// dance so concurrent readers can't observe a half-written cache.
    /// We can't reliably observe the rename, but we can at least confirm
    /// no `.tmp` file is left behind on the happy path.
    #[test]
    fn write_launcher_cache_does_not_leak_tmp_file() {
        let tmp = tempfile::tempdir().unwrap();
        let peppy_dirs = PeppyDirs::new(tmp.path());
        write_launcher_cache(
            &peppy_dirs,
            &[mk_launcher_entry(
                "demo",
                RepoSourceKind::Fs,
                None,
                None,
                "/tmp/demo.json5",
                0,
                false,
            )],
        )
        .unwrap();

        let tmp_path = peppy_dirs.cache_dir().join("launchers.json5.tmp");
        assert!(
            !tmp_path.exists(),
            "tmp file should be renamed away, not left behind"
        );
    }
}
