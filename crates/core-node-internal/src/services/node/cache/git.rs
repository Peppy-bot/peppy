//! Persistent Git checkout cache shared across `node add` batches.
//!
//! Keyed by `(repo_url, repo_ref)`, entries live under
//! [`PeppyDirs::git_checkouts_dir`]. A batch that pulls several nodes
//! from the same repo + ref reuses a single checkout; subsequent batches
//! that hit the same key skip the clone entirely (only fetching new
//! commits on the pinned ref).
//!
//! Concurrency is serialized with an in-process mutex map keyed by
//! `<slug>-<hash>` so two concurrent batches inside the same daemon
//! can't race on the same directory. Cross-process safety is not a
//! concern yet — the daemon is the only writer.

use super::super::checkout_repo_ref;
use super::key;
use config::consts::PeppyDirs;
use git2::build::RepoBuilder;
use parking_lot::Mutex;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

/// Path where the checkout for `(repo_url, repo_ref)` lives (whether or
/// not it has been populated yet). Exposed for tests and diagnostics.
pub fn checkout_dir_for(peppy_dirs: &PeppyDirs, repo_url: &str, repo_ref: Option<&str>) -> PathBuf {
    let slug = key::slug(repo_url, "repo");
    let hash = key::short_hash(repo_url, repo_ref);
    peppy_dirs
        .git_checkouts_dir()
        .join(format!("{slug}-{hash}"))
}

/// Returns `true` when the directory looks like a populated git working
/// tree (`.git` exists). A stale/partial directory (e.g. the clone
/// crashed mid-way) is wiped and re-cloned.
fn is_populated(dir: &Path) -> bool {
    dir.join(".git").exists()
}

fn locks_map() -> &'static Mutex<HashMap<String, Arc<Mutex<()>>>> {
    static MAP: OnceLock<Mutex<HashMap<String, Arc<Mutex<()>>>>> = OnceLock::new();
    MAP.get_or_init(|| Mutex::new(HashMap::new()))
}

fn lock_for(key: &str) -> Arc<Mutex<()>> {
    let mut map = locks_map().lock();
    // GC entries not currently held by any caller. `strong_count == 1`
    // means only the map still references the Arc, so no one can race on
    // rebuilding the slot (the map lock serializes all access).
    map.retain(|_, v| Arc::strong_count(v) > 1);
    map.entry(key.to_owned())
        .or_insert_with(|| Arc::new(Mutex::new(())))
        .clone()
}

/// Per-process set of cache keys that have already been clone-or-fetched
/// during this daemon's lifetime. Used to skip redundant network fetches
/// when the same `(url, ref)` is requested repeatedly (e.g. multiple
/// nodes in one batch, or sequential batches) — the cache key pins the
/// ref, so a once-refreshed checkout stays correct.
fn refreshed_set() -> &'static Mutex<HashSet<String>> {
    static SET: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    SET.get_or_init(|| Mutex::new(HashSet::new()))
}

/// Ensures a checkout exists for `(repo_url, repo_ref)` and returns the
/// working-tree directory. The checkout is populated on first call and
/// refreshed (fetch + checkout) on subsequent calls so pinned refs that
/// moved upstream stay current.
///
/// Blocking — callers inside tokio should run this via
/// [`tokio::task::spawn_blocking`].
pub fn ensure_checkout(
    peppy_dirs: &PeppyDirs,
    repo_url: &str,
    repo_ref: Option<&str>,
    on_feedback: &dyn Fn(&str),
) -> std::result::Result<PathBuf, String> {
    let dir = checkout_dir_for(peppy_dirs, repo_url, repo_ref);
    let lock_key = dir.to_string_lossy().into_owned();
    let lock = lock_for(&lock_key);
    let _guard = lock.lock();

    if let Some(parent) = dir.parent() {
        std::fs::create_dir_all(parent).map_err(|e| {
            format!(
                "Failed to create git_checkouts parent {}: {}",
                parent.display(),
                e
            )
        })?;
    }

    let result = if is_populated(&dir) {
        if refreshed_set().lock().contains(&lock_key) {
            on_feedback(&format!(
                "Reusing cached checkout at {} (already refreshed this session)",
                dir.display()
            ));
            return Ok(dir);
        }
        on_feedback(&format!("Reusing cached checkout at {}", dir.display()));
        refresh_existing(&dir, repo_url, repo_ref, on_feedback)
    } else {
        if dir.exists() {
            // Partial/stale checkout — wipe and re-clone to avoid git2
            // tripping on a populated but non-git directory.
            std::fs::remove_dir_all(&dir).ok();
        }
        on_feedback(&format!(
            "Cloning {} into cache at {}",
            repo_url,
            dir.display()
        ));
        fresh_clone(&dir, repo_url, repo_ref)
    };

    if result.is_ok() {
        refreshed_set().lock().insert(lock_key);
    }
    result
}

fn fresh_clone(
    dir: &Path,
    repo_url: &str,
    repo_ref: Option<&str>,
) -> std::result::Result<PathBuf, String> {
    let is_local = repo_url.starts_with('/') || repo_url.starts_with("file://");
    let mut builder = RepoBuilder::new();
    if !is_local {
        let mut fetch_opts = git2::FetchOptions::new();
        fetch_opts.depth(1);
        builder.fetch_options(fetch_opts);
    }

    let repo = builder
        .clone(repo_url, dir)
        .map_err(|e| format!("Failed to clone {}: {}", repo_url, e))?;
    if let Some(r) = repo_ref {
        checkout_repo_ref(&repo, r)
            .map_err(|e| format!("Failed to checkout ref '{}': {}", r, e))?;
    }
    Ok(dir.to_path_buf())
}

fn refresh_existing(
    dir: &Path,
    repo_url: &str,
    repo_ref: Option<&str>,
    on_feedback: &dyn Fn(&str),
) -> std::result::Result<PathBuf, String> {
    let repo = git2::Repository::open(dir)
        .map_err(|e| format!("Failed to open cached checkout at {}: {}", dir.display(), e))?;

    // Attempt a shallow fetch of the current ref. Some transports reject
    // depth(1) on refetch (local transport in particular); if that fails we
    // just proceed with what's on disk.
    let refspec = repo_ref.unwrap_or("HEAD");
    let fetch_result = repo.find_remote("origin").and_then(|mut remote| {
        let mut fetch_opts = git2::FetchOptions::new();
        let is_local = repo_url.starts_with('/') || repo_url.starts_with("file://");
        if !is_local {
            fetch_opts.depth(1);
        }
        remote.fetch(&[refspec], Some(&mut fetch_opts), None)
    });
    if let Err(e) = fetch_result {
        on_feedback(&format!(
            "Fetch for cached checkout failed ({e}); using existing working tree"
        ));
    }

    if let Some(r) = repo_ref {
        checkout_repo_ref(&repo, r)
            .map_err(|e| format!("Failed to checkout ref '{}': {}", r, e))?;
    }
    Ok(dir.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checkout_dir_for_distinct_refs_distinct_dirs() {
        let tmp = tempfile::tempdir().unwrap();
        let peppy_dirs = PeppyDirs::new(tmp.path());
        let a = checkout_dir_for(&peppy_dirs, "https://example.com/repo.git", Some("v1"));
        let b = checkout_dir_for(&peppy_dirs, "https://example.com/repo.git", Some("v2"));
        let c = checkout_dir_for(&peppy_dirs, "https://example.com/repo.git", None);
        assert_ne!(a, b);
        assert_ne!(a, c);
        assert_ne!(b, c);
    }

    #[test]
    fn checkout_dir_for_same_url_same_ref_same_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let peppy_dirs = PeppyDirs::new(tmp.path());
        let a = checkout_dir_for(&peppy_dirs, "https://example.com/repo.git", Some("main"));
        let b = checkout_dir_for(&peppy_dirs, "https://example.com/repo.git", Some("main"));
        assert_eq!(a, b);
    }
}
