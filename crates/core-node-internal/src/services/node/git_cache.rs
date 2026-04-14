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

use super::checkout_repo_ref;
use config::consts::PeppyDirs;
use git2::build::RepoBuilder;
use parking_lot::Mutex;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

/// Returns a short sanitized slug for a repo URL, useful for
/// human-readable checkout dir names. Keeps `[a-zA-Z0-9._-]`,
/// replaces everything else with `_`, and caps length.
fn url_slug(repo_url: &str) -> String {
    let cleaned: String = repo_url
        .chars()
        .map(|c| match c {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '.' | '-' | '_' => c,
            _ => '_',
        })
        .collect();
    let trimmed = cleaned.trim_matches('_');
    let truncated: String = trimmed.chars().take(40).collect();
    if truncated.is_empty() {
        "repo".to_string()
    } else {
        truncated
    }
}

/// 16-hex-char digest of `url || '\0' || ref`, used as the cache-key
/// suffix to prevent different refs from colliding on the same slug.
fn cache_hash(repo_url: &str, repo_ref: Option<&str>) -> String {
    let mut hasher = Sha256::new();
    hasher.update(repo_url.as_bytes());
    hasher.update([0u8]);
    hasher.update(repo_ref.unwrap_or("").as_bytes());
    let digest = hasher.finalize();
    hex_short(&digest[..8])
}

fn hex_short(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push_str(&format!("{:02x}", b));
    }
    out
}

/// Path where the checkout for `(repo_url, repo_ref)` lives (whether or
/// not it has been populated yet). Exposed for tests and diagnostics.
pub fn checkout_dir_for(peppy_dirs: &PeppyDirs, repo_url: &str, repo_ref: Option<&str>) -> PathBuf {
    let slug = url_slug(repo_url);
    let hash = cache_hash(repo_url, repo_ref);
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
    map.entry(key.to_owned())
        .or_insert_with(|| Arc::new(Mutex::new(())))
        .clone()
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

    if is_populated(&dir) {
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
    }
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

    #[test]
    fn url_slug_rejects_dangerous_chars() {
        assert_eq!(
            url_slug("https://github.com/foo/bar.git"),
            "https___github.com_foo_bar.git"
        );
        assert_eq!(url_slug(""), "repo");
    }
}
