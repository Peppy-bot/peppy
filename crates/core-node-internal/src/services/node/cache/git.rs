//! Persistent Git checkout cache shared across `node add` batches.
//!
//! Keyed by `(repo_url, commit)`, entries live under
//! [`PeppyDirs::git_checkouts_dir`]. A commit names one tree for as long
//! as the repository exists, so a populated checkout is already the right
//! bytes and is reused without touching the network. Several nodes from
//! one repository at one commit share a single checkout.
//!
//! `repo refresh` clones a repository to read what it publishes, which is
//! the same clone materializing any of those items needs, so it hands that
//! clone over through [`adopt_checkout`] rather than deleting it. The
//! caches are what keeps a checkout reachable, so [`prune_checkouts`]
//! drops the ones they no longer name.
//!
//! Concurrency is serialized with an in-process mutex map keyed by
//! `<slug>-<hash>` so two concurrent batches inside the same daemon
//! can't race on the same directory. Cross-process safety is not a
//! concern yet; the daemon is the only writer.

use super::super::checkout_repo_ref;
use super::super::git_utils::{clone_repo_shallow, fetch_with_progress, head_commit};
use super::key;
use super::keyed_lock::KeyedLocks;
use daemon_config::consts::PeppyDirs;
use daemon_config::repository::GitCommit;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};
use tracing::{debug, warn};

static LOCKS: KeyedLocks = KeyedLocks::new();

/// How long a checkout that no cache entry names any more is kept.
///
/// A caller is handed a path and reads from it after the lock is released
/// — `node add` copies the tree out — so a checkout that stops being
/// referenced mid-add must not vanish underneath it. The window runs from
/// the last hand-out, which is long enough to cover any single add and
/// short enough that a superseded pin does not linger for a day.
const PRUNE_GRACE: Duration = Duration::from_secs(60 * 60);

/// Path where the checkout for `(repo_url, commit)` lives (whether or not
/// it has been populated yet). Exposed for tests and diagnostics.
pub fn checkout_dir_for(peppy_dirs: &PeppyDirs, repo_url: &str, commit: &GitCommit) -> PathBuf {
    let slug = key::slug(repo_url);
    let hash = key::short_hash(repo_url, commit.as_str());
    peppy_dirs
        .git_checkouts_dir()
        .join(format!("{slug}-{hash}"))
}

/// The marker whose mtime records when the checkout at `dir` was last
/// handed to a caller.
///
/// Kept inside `.git`, which the node-add copy skips along with every
/// other VCS directory, so it never travels with a node and dies with the
/// checkout instead of outliving it as a stray file.
fn last_used_marker(dir: &Path) -> PathBuf {
    dir.join(".git").join("peppy-last-used")
}

/// Records that the checkout at `dir` has just been handed out, so a prune
/// running while the caller still holds the path leaves it alone.
///
/// Best-effort: a marker that cannot be written only costs that checkout
/// its grace period.
fn touch_last_used(dir: &Path) {
    let _ = std::fs::write(last_used_marker(dir), b"");
}

/// Whether the checkout at `dir` was handed out recently enough that a
/// caller may still be reading it.
fn recently_used(dir: &Path) -> bool {
    let Ok(used) = std::fs::metadata(last_used_marker(dir)).and_then(|m| m.modified()) else {
        return false;
    };
    // A marker dated in the future (a clock stepped back, a restored
    // backup) reads as in use rather than as ancient.
    SystemTime::now()
        .duration_since(used)
        .map_or(true, |since| since < PRUNE_GRACE)
}

/// Empties `dir` and makes sure its parent exists, so a clone into it or a
/// rename onto it starts from nothing.
///
/// Anything already at the key is a checkout that did not finish, since a
/// finished one at this key is the commit asked for.
fn clear_dir(dir: &Path) -> std::result::Result<(), String> {
    if let Some(parent) = dir.parent() {
        std::fs::create_dir_all(parent).map_err(|e| {
            format!(
                "Failed to create git_checkouts parent {}: {}",
                parent.display(),
                e
            )
        })?;
    }
    if dir.exists() {
        std::fs::remove_dir_all(dir).map_err(|e| {
            format!(
                "Failed to remove incomplete checkout at {}: {e}",
                dir.display()
            )
        })?;
    }
    Ok(())
}

/// Whether `dir` is a git working tree already checked out at `commit`.
///
/// Both halves matter. A directory without `.git` is a partial or crashed
/// clone, and one at another commit belongs to a key it no longer matches,
/// which only a hand-edited cache dir can produce.
fn is_checked_out_at(dir: &Path, commit: &GitCommit) -> bool {
    if !dir.join(".git").exists() {
        return false;
    }
    let Ok(repo) = git2::Repository::open(dir) else {
        return false;
    };
    head_commit(&repo).is_ok_and(|head| head == *commit)
}

/// Ensures a checkout of `commit` exists and returns its working-tree
/// directory.
///
/// The clone brings every head the remote publishes at depth 1, so a
/// commit that is any of their tips — which is what a pin read from a
/// freshly refreshed repository is — is already here and the checkout is
/// the whole job. A commit the refs have since moved past is fetched by
/// its own hash, falling back to deepening `repo_ref`; one the remote no
/// longer holds is refused rather than silently answered with a tip.
///
/// Blocking; callers inside tokio should run this via
/// [`tokio::task::spawn_blocking`].
pub fn ensure_checkout_at_commit(
    peppy_dirs: &PeppyDirs,
    repo_url: &str,
    repo_ref: Option<&str>,
    commit: &GitCommit,
    on_feedback: &dyn Fn(&str),
) -> std::result::Result<PathBuf, String> {
    let dir = checkout_dir_for(peppy_dirs, repo_url, commit);
    let lock_key = dir.to_string_lossy().into_owned();
    let lock = LOCKS.lock_for(&lock_key);
    let _guard = lock.lock();

    if is_checked_out_at(&dir, commit) {
        on_feedback(&format!(
            "Reusing cached checkout of {commit} at {}",
            dir.display()
        ));
        touch_last_used(&dir);
        return Ok(dir);
    }

    clear_dir(&dir)?;

    on_feedback(&format!(
        "Cloning {repo_url} at {commit} into cache at {}",
        dir.display()
    ));
    let repo = clone_repo_shallow(repo_url, &dir, &mut |line| on_feedback(line))?;

    // Positioned on the pinned commit rather than on `repo_ref`: an entry
    // pins bytes, and the clone above is the same one `repo refresh` makes,
    // so the head the pin was read from is already here. `repo_ref` is what
    // the deepening fallback starts from when it is not.
    if checkout_repo_ref(&repo, commit.as_str()).is_err() {
        fetch_commit(&repo, repo_url, repo_ref, commit, on_feedback)?;
        checkout_repo_ref(&repo, commit.as_str()).map_err(|e| {
            format!("Failed to check out commit {commit} of {repo_url} after fetching it: {e}")
        })?;
    }
    touch_last_used(&dir);
    Ok(dir)
}

/// Takes over the working tree at `dir` as the cached checkout of
/// `(repo_url, commit)`, moving it under
/// [`PeppyDirs::git_checkouts_dir`].
///
/// `repo refresh` clones a repository to read what it publishes, and that
/// clone is exactly what materializing any of those items needs. Handing it
/// over is what stops the daemon downloading one commit of one repository
/// twice: without it every refresh that advances a pin pays for the whole
/// tree again the first time anything from it is added.
///
/// `dir` must be a working tree whose HEAD is `commit`, and must sit on the
/// same filesystem as the cache — both hold for a clone made under
/// [`PeppyDirs::tmp_dir`]. Ownership of `dir` passes here either way.
///
/// Best-effort, and infallible from the caller's side: whatever goes wrong,
/// the cache is left as it was and the checkout is cloned again on demand.
pub fn adopt_checkout(peppy_dirs: &PeppyDirs, repo_url: &str, commit: &GitCommit, dir: PathBuf) {
    let dst = checkout_dir_for(peppy_dirs, repo_url, commit);
    let lock = LOCKS.lock_for(&dst.to_string_lossy());
    let _guard = lock.lock();

    // A populated checkout at this key is the same commit, so it is already
    // the same bytes and the donation is simply not needed.
    if is_checked_out_at(&dst, commit) {
        touch_last_used(&dst);
        discard(&dir);
        return;
    }
    if let Err(e) = clear_dir(&dst) {
        debug!("Keeping the clone of {repo_url} out of the checkout cache: {e}");
        discard(&dir);
        return;
    }
    match std::fs::rename(&dir, &dst) {
        Ok(()) => {
            debug!(
                "Adopted the clone of {repo_url} at {commit} as {}",
                dst.display()
            );
            touch_last_used(&dst);
        }
        Err(e) => {
            debug!("Keeping the clone of {repo_url} out of the checkout cache: {e}");
            discard(&dir);
        }
    }
}

/// Removes a working tree this module took ownership of but did not keep.
fn discard(dir: &Path) {
    if let Err(e) = std::fs::remove_dir_all(dir) {
        warn!("Failed to remove the clone at {}: {e}", dir.display());
    }
}

/// Removes cached checkouts that no cache entry points at any more.
///
/// A checkout is only ever reached through the `(repo_url, commit)` of some
/// entry, so once the caches are published anything they do not name can
/// never be resolved again. Without this, every refresh that advances a pin
/// would leave a full working tree behind for good.
///
/// `live` names the pairs the caches still point at. Returns how many
/// checkouts were removed; a checkout that cannot be removed is reported
/// and skipped, because it costs disk rather than correctness.
pub fn prune_checkouts<'a>(
    peppy_dirs: &PeppyDirs,
    live: impl IntoIterator<Item = (&'a str, &'a GitCommit)>,
) -> usize {
    let keep: HashSet<PathBuf> = live
        .into_iter()
        .map(|(repo_url, commit)| checkout_dir_for(peppy_dirs, repo_url, commit))
        .collect();
    // Nothing has been cached yet, which is the common case on a machine
    // whose repositories are all on its own filesystem.
    let Ok(cached) = std::fs::read_dir(peppy_dirs.git_checkouts_dir()) else {
        return 0;
    };

    let mut removed = 0;
    for entry in cached.flatten() {
        let dir = entry.path();
        if !entry.file_type().is_ok_and(|kind| kind.is_dir())
            || keep.contains(&dir)
            || recently_used(&dir)
        {
            continue;
        }
        // A held lock means someone is cloning into this directory or has
        // just been handed it; it gets another chance next refresh.
        let lock = LOCKS.lock_for(&dir.to_string_lossy());
        let Some(_guard) = lock.try_lock() else {
            continue;
        };
        match std::fs::remove_dir_all(&dir) {
            Ok(()) => removed += 1,
            Err(e) => warn!("Failed to remove stale checkout {}: {e}", dir.display()),
        }
    }
    removed
}

/// Brings `commit` into `repo`, having failed to find it in the shallow
/// clone of the ref.
///
/// Two attempts, because two different things are wrong in the two cases.
/// Asking for the commit by hash covers a branch that moved past it, and
/// works wherever the host allows it. Deepening the ref covers a host that
/// does not, at the cost of the history the shallow clone skipped.
fn fetch_commit(
    repo: &git2::Repository,
    repo_url: &str,
    repo_ref: Option<&str>,
    commit: &GitCommit,
    on_feedback: &dyn Fn(&str),
) -> std::result::Result<(), String> {
    on_feedback(&format!(
        "Commit {commit} is not in the shallow clone of {repo_url}; fetching it"
    ));

    let by_hash = repo.find_remote("origin").and_then(|mut remote| {
        fetch_with_progress(&mut remote, repo_url, commit.as_str(), true, &mut |line| {
            on_feedback(line)
        })
    });
    if by_hash.is_ok() && has_commit(repo, commit) {
        return Ok(());
    }

    let refspec = repo_ref.unwrap_or("HEAD");
    on_feedback(&format!(
        "{repo_url} does not serve commits by hash; fetching the full history of {refspec}"
    ));
    repo.find_remote("origin")
        .and_then(|mut remote| {
            fetch_with_progress(&mut remote, repo_url, refspec, false, &mut |line| {
                on_feedback(line)
            })
        })
        .map_err(|e| format!("Failed to fetch {refspec} from {repo_url}: {e}"))?;

    if has_commit(repo, commit) {
        return Ok(());
    }
    Err(format!(
        "commit {commit} is not reachable at {repo_url}. The pin names bytes this remote no \
         longer serves, which means the repository was rewritten or the commit was never pushed"
    ))
}

/// Whether the object database already holds `commit`.
fn has_commit(repo: &git2::Repository, commit: &GitCommit) -> bool {
    git2::Oid::from_str(commit.as_str())
        .and_then(|oid| repo.find_commit(oid))
        .is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A repository with one commit per entry in `contents`, returning the
    /// commit of each in order. Built with the local transport, so nothing
    /// here reaches the network.
    fn repo_with_commits(dir: &Path, contents: &[&str]) -> Vec<GitCommit> {
        std::fs::create_dir_all(dir).expect("create source repo dir");
        let repo = git2::Repository::init(dir).expect("init source repo");
        let signature =
            git2::Signature::now("Peppy", "peppy@example.com").expect("create signature");
        let file_name = Path::new("tracked.txt");

        contents
            .iter()
            .map(|content| {
                std::fs::write(dir.join(file_name), content).expect("write tracked file");
                let mut index = repo.index().expect("open index");
                index.add_path(file_name).expect("add tracked file");
                index.write().expect("write index");
                let tree_id = index.write_tree().expect("write tree");
                let tree = repo.find_tree(tree_id).expect("find tree");
                let parents: Vec<git2::Commit> = repo
                    .head()
                    .ok()
                    .and_then(|head| head.target())
                    .map(|oid| vec![repo.find_commit(oid).expect("find parent commit")])
                    .unwrap_or_default();
                let parent_refs: Vec<&git2::Commit> = parents.iter().collect();
                let oid = repo
                    .commit(
                        Some("HEAD"),
                        &signature,
                        &signature,
                        content,
                        &tree,
                        &parent_refs,
                    )
                    .expect("commit tracked file");
                GitCommit::parse(&oid.to_string()).expect("a real commit is a full hash")
            })
            .collect()
    }

    fn head_branch(repo_dir: &Path) -> String {
        git2::Repository::open(repo_dir)
            .expect("open source repo")
            .head()
            .expect("head")
            .shorthand()
            .expect("branch shorthand")
            .to_owned()
    }

    #[test]
    fn checkout_dir_for_distinct_commits_distinct_dirs() {
        let tmp = tempfile::tempdir().unwrap();
        let peppy_dirs = PeppyDirs::new(tmp.path());
        let a = GitCommit::parse(&"a".repeat(40)).unwrap();
        let b = GitCommit::parse(&"b".repeat(40)).unwrap();
        let url = "https://example.com/repo.git";
        assert_ne!(
            checkout_dir_for(&peppy_dirs, url, &a),
            checkout_dir_for(&peppy_dirs, url, &b)
        );
        assert_eq!(
            checkout_dir_for(&peppy_dirs, url, &a),
            checkout_dir_for(&peppy_dirs, url, &a)
        );
    }

    #[test]
    fn ensure_checkout_at_commit_reuses_a_populated_dir_without_fetching() {
        let tmp = tempfile::tempdir().unwrap();
        let peppy_dirs = PeppyDirs::new(tmp.path());
        let source = tempfile::tempdir().unwrap();
        let source_dir = source.path().join("source-repo");
        let commits = repo_with_commits(&source_dir, &["first"]);
        let url = source_dir.display().to_string();
        let branch = head_branch(&source_dir);

        let checkout =
            ensure_checkout_at_commit(&peppy_dirs, &url, Some(&branch), &commits[0], &|_| {})
                .expect("initial checkout");
        assert_eq!(
            std::fs::read_to_string(checkout.join("tracked.txt")).unwrap(),
            "first"
        );

        // Deleting the source proves the second call touched no network:
        // a clone or fetch against a missing path fails outright.
        std::fs::remove_dir_all(&source_dir).expect("remove source repo");
        let lines = std::cell::RefCell::new(Vec::new());
        let reused =
            ensure_checkout_at_commit(&peppy_dirs, &url, Some(&branch), &commits[0], &|l| {
                lines.borrow_mut().push(l.to_owned())
            })
            .expect("a populated checkout needs no remote");
        assert_eq!(checkout, reused);
        assert_eq!(
            lines.into_inner(),
            vec![format!(
                "Reusing cached checkout of {} at {}",
                commits[0],
                reused.display()
            )]
        );
    }

    #[test]
    fn ensure_checkout_at_commit_resolves_a_commit_the_branch_moved_past() {
        let tmp = tempfile::tempdir().unwrap();
        let peppy_dirs = PeppyDirs::new(tmp.path());
        let source = tempfile::tempdir().unwrap();
        let source_dir = source.path().join("source-repo");
        let commits = repo_with_commits(&source_dir, &["first", "second"]);
        let url = source_dir.display().to_string();
        let branch = head_branch(&source_dir);

        // The branch now points at "second"; the pin names "first".
        let checkout =
            ensure_checkout_at_commit(&peppy_dirs, &url, Some(&branch), &commits[0], &|_| {})
                .expect("a commit behind the branch tip still resolves");
        assert_eq!(
            std::fs::read_to_string(checkout.join("tracked.txt")).unwrap(),
            "first",
            "the pinned commit is what gets checked out, not the branch tip"
        );

        let tip = ensure_checkout_at_commit(&peppy_dirs, &url, Some(&branch), &commits[1], &|_| {})
            .expect("the tip resolves too");
        assert_ne!(checkout, tip, "two commits are two checkouts");
        assert_eq!(
            std::fs::read_to_string(tip.join("tracked.txt")).unwrap(),
            "second"
        );
    }

    #[test]
    fn ensure_checkout_at_commit_refuses_a_commit_the_remote_does_not_have() {
        let tmp = tempfile::tempdir().unwrap();
        let peppy_dirs = PeppyDirs::new(tmp.path());
        let source = tempfile::tempdir().unwrap();
        let source_dir = source.path().join("source-repo");
        repo_with_commits(&source_dir, &["first"]);
        let url = source_dir.display().to_string();
        let branch = head_branch(&source_dir);
        let absent = GitCommit::parse(&"0".repeat(40)).unwrap();

        let error = ensure_checkout_at_commit(&peppy_dirs, &url, Some(&branch), &absent, &|_| {})
            .expect_err("a commit the remote never had cannot be checked out");
        assert!(
            error.contains(absent.as_str()) && error.contains("not reachable"),
            "the refusal names the commit it could not reach, got: {error}"
        );
    }

    /// The donation is the whole point of the cache being keyed by commit:
    /// a clone somebody else already paid for is the same bytes, so nothing
    /// is fetched afterwards. Proven by deleting the source repository
    /// before asking for the checkout — a clone or fetch would fail.
    #[test]
    fn adopt_checkout_takes_over_a_clone_made_elsewhere() {
        let tmp = tempfile::tempdir().unwrap();
        let peppy_dirs = PeppyDirs::new(tmp.path());
        let source = tempfile::tempdir().unwrap();
        let source_dir = source.path().join("source-repo");
        let commits = repo_with_commits(&source_dir, &["first"]);
        let url = source_dir.display().to_string();

        let tmp_root = peppy_dirs.tmp_dir();
        std::fs::create_dir_all(&tmp_root).expect("create the tmp root");
        let donor = tempfile::tempdir_in(&tmp_root).unwrap();
        let clone = donor.path().join("clone");
        clone_repo_shallow(&url, &clone, &mut |_| {}).expect("clone the source");
        adopt_checkout(&peppy_dirs, &url, &commits[0], clone);

        std::fs::remove_dir_all(&source_dir).expect("remove source repo");
        let lines = std::cell::RefCell::new(Vec::new());
        let checkout = ensure_checkout_at_commit(&peppy_dirs, &url, None, &commits[0], &|l| {
            lines.borrow_mut().push(l.to_owned())
        })
        .expect("the donated clone needs no remote");

        assert_eq!(checkout, checkout_dir_for(&peppy_dirs, &url, &commits[0]));
        assert_eq!(
            std::fs::read_to_string(checkout.join("tracked.txt")).unwrap(),
            "first"
        );
        assert!(
            lines.into_inner()[0].starts_with("Reusing cached checkout"),
            "the donated clone is reused, not re-cloned"
        );
    }

    /// A key that already holds the commit already holds the same bytes, so
    /// the donation is dropped rather than swapped in — and the donor never
    /// leaks either way.
    #[test]
    fn adopt_checkout_keeps_the_checkout_already_at_the_key() {
        let tmp = tempfile::tempdir().unwrap();
        let peppy_dirs = PeppyDirs::new(tmp.path());
        let source = tempfile::tempdir().unwrap();
        let source_dir = source.path().join("source-repo");
        let commits = repo_with_commits(&source_dir, &["first"]);
        let url = source_dir.display().to_string();

        let existing = ensure_checkout_at_commit(&peppy_dirs, &url, None, &commits[0], &|_| {})
            .expect("initial checkout");
        std::fs::write(existing.join("marker.txt"), "original").expect("mark the original");

        let tmp_root = peppy_dirs.tmp_dir();
        std::fs::create_dir_all(&tmp_root).expect("create the tmp root");
        let donor = tempfile::tempdir_in(&tmp_root).unwrap();
        let clone = donor.path().join("clone");
        clone_repo_shallow(&url, &clone, &mut |_| {}).expect("clone the source");
        adopt_checkout(&peppy_dirs, &url, &commits[0], clone.clone());

        assert!(
            existing.join("marker.txt").exists(),
            "the populated checkout stays; the donation is what is dropped"
        );
        assert!(!clone.exists(), "the donated clone is not left behind");
    }

    /// A checkout is only ever reached through a cache entry's
    /// `(repo_url, commit)`, so one the caches stopped naming can never be
    /// resolved again and is disk the daemon owes back.
    #[test]
    fn prune_checkouts_removes_only_what_no_entry_names() {
        let tmp = tempfile::tempdir().unwrap();
        let peppy_dirs = PeppyDirs::new(tmp.path());
        let url = "https://example.com/hub.git";
        let live = GitCommit::parse(&"a".repeat(40)).unwrap();
        let superseded = GitCommit::parse(&"b".repeat(40)).unwrap();

        let live_dir = checkout_dir_for(&peppy_dirs, url, &live);
        let superseded_dir = checkout_dir_for(&peppy_dirs, url, &superseded);
        for dir in [&live_dir, &superseded_dir] {
            std::fs::create_dir_all(dir.join(".git")).expect("create checkout");
        }

        assert_eq!(prune_checkouts(&peppy_dirs, [(url, &live)]), 1);
        assert!(live_dir.exists(), "an entry still names this commit");
        assert!(!superseded_dir.exists(), "nothing names this commit");
    }

    /// A caller is handed a path and keeps reading from it after the lock
    /// is released — `node add` copies the tree out — so a refresh that
    /// supersedes the pin mid-add must not pull the directory out from
    /// under it.
    #[test]
    fn prune_checkouts_keeps_a_checkout_handed_out_recently() {
        let tmp = tempfile::tempdir().unwrap();
        let peppy_dirs = PeppyDirs::new(tmp.path());
        let source = tempfile::tempdir().unwrap();
        let source_dir = source.path().join("source-repo");
        let commits = repo_with_commits(&source_dir, &["first"]);
        let url = source_dir.display().to_string();

        let checkout = ensure_checkout_at_commit(&peppy_dirs, &url, None, &commits[0], &|_| {})
            .expect("initial checkout");

        assert_eq!(prune_checkouts(&peppy_dirs, std::iter::empty()), 0);
        assert!(
            checkout.exists(),
            "a checkout just handed out is still in use"
        );
    }

    #[test]
    fn ensure_checkout_at_commit_replaces_an_incomplete_checkout() {
        let tmp = tempfile::tempdir().unwrap();
        let peppy_dirs = PeppyDirs::new(tmp.path());
        let source = tempfile::tempdir().unwrap();
        let source_dir = source.path().join("source-repo");
        let commits = repo_with_commits(&source_dir, &["first"]);
        let url = source_dir.display().to_string();
        let branch = head_branch(&source_dir);

        // A directory at the key with no `.git`: what a clone killed
        // half-way through leaves behind.
        let dir = checkout_dir_for(&peppy_dirs, &url, &commits[0]);
        std::fs::create_dir_all(&dir).expect("create partial checkout");
        std::fs::write(dir.join("tracked.txt"), "garbage").expect("write partial content");

        let checkout =
            ensure_checkout_at_commit(&peppy_dirs, &url, Some(&branch), &commits[0], &|_| {})
                .expect("an incomplete checkout is replaced rather than trusted");
        assert_eq!(checkout, dir);
        assert_eq!(
            std::fs::read_to_string(checkout.join("tracked.txt")).unwrap(),
            "first"
        );
    }
}
