//! Persistent Git checkout cache shared across `node add` batches.
//!
//! Keyed by `(repo_url, commit)`, entries live under
//! [`PeppyDirs::git_checkouts_dir`]. A commit names one tree for as long
//! as the repository exists, so a populated checkout is already the right
//! bytes and is reused without touching the network. Several nodes from
//! one repository at one commit share a single checkout.
//!
//! Concurrency is serialized with an in-process mutex map keyed by
//! `<slug>-<hash>` so two concurrent batches inside the same daemon
//! can't race on the same directory. Cross-process safety is not a
//! concern yet; the daemon is the only writer.

use super::super::checkout_repo_ref;
use super::super::git_utils::{clone_with_progress, fetch_with_progress, head_commit};
use super::key;
use super::keyed_lock::KeyedLocks;
use daemon_config::consts::PeppyDirs;
use daemon_config::repository::GitCommit;
use std::path::{Path, PathBuf};

static LOCKS: KeyedLocks = KeyedLocks::new();

/// Path where the checkout for `(repo_url, commit)` lives (whether or not
/// it has been populated yet). Exposed for tests and diagnostics.
pub fn checkout_dir_for(peppy_dirs: &PeppyDirs, repo_url: &str, commit: &GitCommit) -> PathBuf {
    let slug = key::slug(repo_url, "repo");
    let hash = key::short_hash(repo_url, commit.as_str());
    peppy_dirs
        .git_checkouts_dir()
        .join(format!("{slug}-{hash}"))
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
/// The shallow clone of the remote's default branch is the cheap way to
/// reach a commit at or near its tip, which is the common case. A commit
/// that clone does not contain is fetched by its own hash, falling back to
/// deepening `repo_ref`; one the remote no longer holds is refused rather
/// than silently answered with a branch tip.
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
        return Ok(dir);
    }

    if let Some(parent) = dir.parent() {
        std::fs::create_dir_all(parent).map_err(|e| {
            format!(
                "Failed to create git_checkouts parent {}: {}",
                parent.display(),
                e
            )
        })?;
    }

    // Anything already here is a checkout that did not finish, since a
    // finished one at this key is the commit asked for.
    if dir.exists() {
        std::fs::remove_dir_all(&dir).map_err(|e| {
            format!(
                "Failed to remove incomplete checkout at {}: {e}",
                dir.display()
            )
        })?;
    }

    on_feedback(&format!(
        "Cloning {repo_url} at {commit} into cache at {}",
        dir.display()
    ));
    // Cloned shallow at the remote's default branch first: when the commit
    // is that branch's tip, which is what a freshly refreshed cache of a
    // repository followed at its default branch pins, this is the whole job.
    let repo = clone_with_progress(repo_url, None, &dir, true, &mut |line| on_feedback(line))?;

    if checkout_repo_ref(&repo, commit.as_str()).is_ok() {
        return Ok(dir);
    }

    fetch_commit(&repo, repo_url, repo_ref, commit, on_feedback)?;
    checkout_repo_ref(&repo, commit.as_str()).map_err(|e| {
        format!("Failed to check out commit {commit} of {repo_url} after fetching it: {e}")
    })?;
    Ok(dir)
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
