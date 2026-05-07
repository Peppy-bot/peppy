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
use super::super::git_utils::{clone_with_progress, fetch_with_progress};
use super::key;
use config::consts::PeppyDirs;
use parking_lot::Mutex;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::SystemTime;

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

/// Per-process map of cache keys to the repo-cache generation they were
/// last refreshed against. This keeps repeated materializations within a
/// single `nodes.json5` snapshot fast without pinning moving refs for
/// the full daemon lifetime.
fn refreshed_generations() -> &'static Mutex<HashMap<String, SystemTime>> {
    static MAP: OnceLock<Mutex<HashMap<String, SystemTime>>> = OnceLock::new();
    MAP.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Ensures a checkout exists for `(repo_url, repo_ref)` and returns the
/// working-tree directory. The checkout is populated on first call and
/// refreshed (fetch + checkout) on subsequent calls so pinned refs that
/// moved upstream stay current. Callers should pass the same
/// `nodes.json5` generation they resolved the repo entry from so the
/// checkout stays consistent with that snapshot.
///
/// Blocking — callers inside tokio should run this via
/// [`tokio::task::spawn_blocking`].
pub fn ensure_checkout(
    peppy_dirs: &PeppyDirs,
    repo_url: &str,
    repo_ref: Option<&str>,
    cache_generation: Option<SystemTime>,
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
        if let Some(generation) = cache_generation
            && refreshed_generations().lock().get(&lock_key) == Some(&generation)
        {
            on_feedback(&format!(
                "Reusing cached checkout at {} (already refreshed for this packages cache generation)",
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
        fresh_clone(&dir, repo_url, repo_ref, on_feedback)
    };

    if result.is_ok()
        && let Some(generation) = cache_generation
    {
        refreshed_generations().lock().insert(lock_key, generation);
    }
    result
}

fn fresh_clone(
    dir: &Path,
    repo_url: &str,
    repo_ref: Option<&str>,
    on_feedback: &dyn Fn(&str),
) -> std::result::Result<PathBuf, String> {
    clone_with_progress(repo_url, repo_ref, dir, true, &mut |line| on_feedback(line))?;
    Ok(dir.to_path_buf())
}

fn refresh_existing(
    dir: &Path,
    repo_url: &str,
    repo_ref: Option<&str>,
    on_feedback: &dyn Fn(&str),
) -> std::result::Result<PathBuf, String> {
    let wipe_and_reclone = |reason: &str| -> std::result::Result<PathBuf, String> {
        on_feedback(reason);
        std::fs::remove_dir_all(dir).map_err(|remove_err| {
            format!(
                "Failed to remove stale cached checkout at {}: {}",
                dir.display(),
                remove_err
            )
        })?;
        on_feedback(&format!(
            "Recloning {} into cache at {}",
            repo_url,
            dir.display()
        ));
        fresh_clone(dir, repo_url, repo_ref, on_feedback)
    };

    let repo = match git2::Repository::open(dir) {
        Ok(repo) => repo,
        Err(e) => {
            return wipe_and_reclone(&format!(
                "Failed to open cached checkout at {} ({e}); removing stale checkout and recloning",
                dir.display()
            ));
        }
    };

    // Attempt a shallow fetch of the current ref. Some transports reject
    // depth(1) on refetch (local transport in particular); the helper
    // downgrades to a non-shallow fetch in that case.
    let refspec = repo_ref.unwrap_or("HEAD");
    let fetch_result = repo.find_remote("origin").and_then(|mut remote| {
        fetch_with_progress(&mut remote, repo_url, refspec, true, &mut |line| {
            on_feedback(line)
        })
    });

    if let Err(e) = fetch_result {
        drop(repo);
        return wipe_and_reclone(&format!(
            "Fetch for cached checkout failed ({e}); removing stale checkout and recloning"
        ));
    }

    checkout_repo_ref(&repo, "FETCH_HEAD")
        .map_err(|e| format!("Failed to checkout fetched ref for '{}': {}", refspec, e))?;
    Ok(dir.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::time::{Duration, SystemTime};

    fn commit_file(repo: &git2::Repository, repo_dir: &Path, contents: &str, message: &str) {
        let file_name = Path::new("tracked.txt");
        std::fs::write(repo_dir.join(file_name), contents).expect("write tracked file");

        let mut index = repo.index().expect("open index");
        index.add_path(file_name).expect("add tracked file");
        index.write().expect("write index");
        let tree_id = index.write_tree().expect("write tree");
        let tree = repo.find_tree(tree_id).expect("find tree");

        let parent_commits: Vec<git2::Commit> = repo
            .head()
            .ok()
            .and_then(|head| head.target())
            .map(|oid| vec![repo.find_commit(oid).expect("find parent commit")])
            .unwrap_or_default();
        let parent_refs: Vec<&git2::Commit> = parent_commits.iter().collect();
        let signature =
            git2::Signature::now("Peppy", "peppy@example.com").expect("create signature");

        repo.commit(
            Some("HEAD"),
            &signature,
            &signature,
            message,
            &tree,
            &parent_refs,
        )
        .expect("commit tracked file");
    }

    fn write_packages_cache_generation(
        peppy_dirs: &PeppyDirs,
        marker: &str,
        previous: Option<SystemTime>,
    ) -> SystemTime {
        let cache_path = crate::services::repo::cache::nodes_repo_cache_path(peppy_dirs);
        std::fs::create_dir_all(peppy_dirs.cache_dir()).expect("create cache dir");

        for attempt in 0..20 {
            let contents = format!(r#"[{{"marker":"{marker}-{attempt}"}}]"#);
            std::fs::write(&cache_path, contents).expect("write packages cache");

            let modified = std::fs::metadata(&cache_path)
                .expect("read packages cache metadata")
                .modified()
                .expect("read packages cache mtime");
            match previous {
                Some(prev) if modified <= prev => std::thread::sleep(Duration::from_millis(100)),
                _ => return modified,
            }
        }

        panic!("failed to advance packages cache generation");
    }

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
    fn ensure_checkout_refreshes_when_packages_cache_generation_changes() {
        let tmp = tempfile::tempdir().unwrap();
        let peppy_dirs = PeppyDirs::new(tmp.path());
        let first_generation = write_packages_cache_generation(&peppy_dirs, "initial", None);

        let source_parent = tempfile::tempdir().unwrap();
        let source_repo_dir = source_parent.path().join("source-repo");
        std::fs::create_dir_all(&source_repo_dir).expect("create source repo dir");
        let repo = git2::Repository::init(&source_repo_dir).expect("init source repo");
        commit_file(&repo, &source_repo_dir, "first", "first commit");

        let repo_url = source_repo_dir.display().to_string();
        let repo_ref = repo
            .head()
            .expect("head")
            .shorthand()
            .expect("branch shorthand")
            .to_owned();

        let checkout = ensure_checkout(
            &peppy_dirs,
            &repo_url,
            Some(&repo_ref),
            Some(first_generation),
            &|_| {},
        )
        .expect("initial checkout");
        assert_eq!(
            std::fs::read_to_string(checkout.join("tracked.txt")).unwrap(),
            "first"
        );

        commit_file(&repo, &source_repo_dir, "second", "second commit");

        let same_generation_feedback = RefCell::new(Vec::new());
        let reused_checkout = ensure_checkout(
            &peppy_dirs,
            &repo_url,
            Some(&repo_ref),
            Some(first_generation),
            &|line| same_generation_feedback.borrow_mut().push(line.to_owned()),
        )
        .expect("reuse checkout within same generation");
        assert_eq!(checkout, reused_checkout);
        assert_eq!(
            std::fs::read_to_string(reused_checkout.join("tracked.txt")).unwrap(),
            "first",
            "same packages cache generation should keep using the previously refreshed checkout"
        );
        assert_eq!(
            same_generation_feedback.into_inner(),
            vec![format!(
                "Reusing cached checkout at {} (already refreshed for this packages cache generation)",
                reused_checkout.display()
            )],
            "same packages cache generation should short-circuit before any refetch"
        );

        let second_generation =
            write_packages_cache_generation(&peppy_dirs, "refreshed", Some(first_generation));
        let refreshed_feedback = RefCell::new(Vec::new());
        let refreshed_checkout = ensure_checkout(
            &peppy_dirs,
            &repo_url,
            Some(&repo_ref),
            Some(second_generation),
            &|line| refreshed_feedback.borrow_mut().push(line.to_owned()),
        )
        .expect("refresh checkout after packages cache update");
        let refreshed_feedback = refreshed_feedback.into_inner();
        assert_eq!(checkout, refreshed_checkout);
        assert_eq!(
            std::fs::read_to_string(refreshed_checkout.join("tracked.txt")).unwrap(),
            "second",
            "new packages cache generation should refresh moving refs"
        );
        assert!(
            refreshed_feedback.iter().any(|line| line
                == &format!(
                    "Reusing cached checkout at {}",
                    refreshed_checkout.display()
                )),
            "new packages cache generation should refresh the cached checkout"
        );
    }

    #[test]
    fn ensure_checkout_reclones_when_cached_repo_is_corrupted() {
        let tmp = tempfile::tempdir().unwrap();
        let peppy_dirs = PeppyDirs::new(tmp.path());
        let first_generation = write_packages_cache_generation(&peppy_dirs, "initial", None);

        let source_parent = tempfile::tempdir().unwrap();
        let source_repo_dir = source_parent.path().join("source-repo");
        std::fs::create_dir_all(&source_repo_dir).expect("create source repo dir");
        let repo = git2::Repository::init(&source_repo_dir).expect("init source repo");
        commit_file(&repo, &source_repo_dir, "first", "first commit");

        let repo_url = source_repo_dir.display().to_string();
        let repo_ref = repo
            .head()
            .expect("head")
            .shorthand()
            .expect("branch shorthand")
            .to_owned();

        let checkout = ensure_checkout(
            &peppy_dirs,
            &repo_url,
            Some(&repo_ref),
            Some(first_generation),
            &|_| {},
        )
        .expect("initial checkout");

        commit_file(&repo, &source_repo_dir, "second", "second commit");

        // Corrupt the cached checkout so `Repository::open` fails but
        // `is_populated` still returns true: replace the `.git` directory
        // with a regular file containing an invalid gitlink, which libgit2
        // rejects at open time.
        std::fs::remove_dir_all(checkout.join(".git")).expect("remove .git dir");
        std::fs::write(checkout.join(".git"), "not a valid gitlink")
            .expect("write corrupt .git file");

        let second_generation =
            write_packages_cache_generation(&peppy_dirs, "refreshed", Some(first_generation));
        let refreshed_feedback = RefCell::new(Vec::new());
        let refreshed_checkout = ensure_checkout(
            &peppy_dirs,
            &repo_url,
            Some(&repo_ref),
            Some(second_generation),
            &|line| refreshed_feedback.borrow_mut().push(line.to_owned()),
        )
        .expect("corrupted cached checkout should be wiped and recloned");
        let refreshed_feedback = refreshed_feedback.into_inner();

        assert_eq!(checkout, refreshed_checkout);
        assert_eq!(
            std::fs::read_to_string(refreshed_checkout.join("tracked.txt")).unwrap(),
            "second",
            "reclone after corruption must produce a working tree at the latest ref"
        );
        assert!(
            refreshed_feedback
                .iter()
                .any(|line| line.contains("Failed to open cached checkout")),
            "expected feedback about the failed open, got: {refreshed_feedback:?}"
        );
        assert!(
            refreshed_feedback.iter().any(|line| {
                line == &format!(
                    "Recloning {} into cache at {}",
                    repo_url,
                    refreshed_checkout.display()
                )
            }),
            "expected fallback reclone feedback, got: {refreshed_feedback:?}"
        );
    }

    #[test]
    fn ensure_checkout_reclones_after_failed_refetch() {
        let tmp = tempfile::tempdir().unwrap();
        let peppy_dirs = PeppyDirs::new(tmp.path());
        let first_generation = write_packages_cache_generation(&peppy_dirs, "initial", None);

        let source_parent = tempfile::tempdir().unwrap();
        let source_repo_dir = source_parent.path().join("source-repo");
        std::fs::create_dir_all(&source_repo_dir).expect("create source repo dir");
        let repo = git2::Repository::init(&source_repo_dir).expect("init source repo");
        commit_file(&repo, &source_repo_dir, "first", "first commit");

        let repo_url = source_repo_dir.display().to_string();
        let repo_ref = repo
            .head()
            .expect("head")
            .shorthand()
            .expect("branch shorthand")
            .to_owned();

        let checkout = ensure_checkout(
            &peppy_dirs,
            &repo_url,
            Some(&repo_ref),
            Some(first_generation),
            &|_| {},
        )
        .expect("initial checkout");
        assert_eq!(
            std::fs::read_to_string(checkout.join("tracked.txt")).unwrap(),
            "first"
        );

        commit_file(&repo, &source_repo_dir, "second", "second commit");

        let checkout_repo = git2::Repository::open(&checkout).expect("open cached checkout");
        checkout_repo
            .remote_set_url("origin", "/definitely/missing/remote")
            .expect("corrupt cached origin URL");
        drop(checkout_repo);

        let second_generation =
            write_packages_cache_generation(&peppy_dirs, "refreshed", Some(first_generation));
        let refreshed_feedback = RefCell::new(Vec::new());
        let refreshed_checkout = ensure_checkout(
            &peppy_dirs,
            &repo_url,
            Some(&repo_ref),
            Some(second_generation),
            &|line| refreshed_feedback.borrow_mut().push(line.to_owned()),
        )
        .expect("refresh after failed fetch should recover via reclone");
        let refreshed_feedback = refreshed_feedback.into_inner();

        assert_eq!(checkout, refreshed_checkout);
        assert_eq!(
            std::fs::read_to_string(refreshed_checkout.join("tracked.txt")).unwrap(),
            "second",
            "failed refetch must not reuse the stale checkout contents"
        );
        assert!(
            refreshed_feedback
                .iter()
                .any(|line| line.contains("Fetch for cached checkout failed")),
            "expected feedback about the failed refetch"
        );
        assert!(
            refreshed_feedback.iter().any(|line| {
                line == &format!(
                    "Recloning {} into cache at {}",
                    repo_url,
                    refreshed_checkout.display()
                )
            }),
            "expected fallback reclone feedback, got: {refreshed_feedback:?}"
        );
    }
}
