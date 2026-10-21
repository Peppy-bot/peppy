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
//! can't race on the same directory. The locks do not span processes:
//! the daemon is the only writer.

use super::super::checkout_repo_ref;
use super::super::git_utils::{
    fetch_commit_shallow, fetch_with_progress, head_commit, init_empty_repository,
};
use super::key;
use super::keyed_lock::KeyedLocks;
use crate::services::node::Report;
use crate::source_url::SourceUrl;
use daemon_config::consts::PeppyDirs;
use daemon_config::repository::GitCommit;
use parking_lot::Mutex;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{Duration, SystemTime};
use tracing::{debug, warn};

static LOCKS: KeyedLocks = KeyedLocks::new();

/// Checkouts whose reuse has been announced through `on_feedback` while
/// this process has been running, keyed by checkout directory.
///
/// One launch resolves many items out of the same few repositories, and
/// every resolution re-enters [`ensure_checkout_at_commit`] on the same
/// warm checkout. The first reuse line tells the operator the cache is
/// doing its job; the dozens after it repeat it, so each checkout is
/// announced once per population. A checkout removed from the cache and
/// materialized again is a new population, announced afresh.
static ANNOUNCED_REUSES: OnceLock<Mutex<HashSet<PathBuf>>> = OnceLock::new();

/// Whether `dir` has not been announced as reused yet, marking it
/// announced. Under the checkout's own lock, so the first reuse after each
/// population is the one that reports.
fn mark_reuse_announced(dir: &Path) -> bool {
    ANNOUNCED_REUSES
        .get_or_init(|| Mutex::new(HashSet::new()))
        .lock()
        .insert(dir.to_path_buf())
}

/// Drops `dir`'s reuse announcement, for a checkout leaving the cache: a
/// checkout later materialized at the same key is a new population whose
/// first reuse is worth a line again.
fn forget_reuse_announced(dir: &Path) {
    if let Some(announced) = ANNOUNCED_REUSES.get() {
        announced.lock().remove(dir);
    }
}

/// How long a checkout that no cache entry names any more is kept.
///
/// A caller is handed a path and reads from it after the lock is released
/// (`node add` copies the tree out), so a checkout that stops being
/// referenced mid-add must not vanish underneath it. The window runs from
/// the last hand-out, which is long enough to cover any single add and
/// short enough that a superseded pin does not linger for a day.
const PRUNE_GRACE: Duration = Duration::from_secs(60 * 60);

/// Path where the checkout for `(repo_url, commit)` lives (whether or not
/// it has been populated yet). Exposed for tests and diagnostics.
pub fn checkout_dir_for(peppy_dirs: &PeppyDirs, repo_url: &str, commit: &GitCommit) -> PathBuf {
    // The directory is named in the lines the daemon writes, so its readable
    // part comes from the URL as shown. The hash tells apart two URLs that
    // differ in their credentials alone.
    let slug = key::slug(&SourceUrl::new(repo_url).to_string());
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
        forget_reuse_announced(dir);
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

/// The working tree of `(repo_url, commit)` this machine already holds, or
/// `None` when nothing has materialized it yet.
///
/// Reads the cache and writes nothing: no clone, no fetch, no marker. It is
/// what a caller that must not reach the network uses to read a file out of
/// a tree the caches have already brought down, `repo refresh` handing every
/// clone it makes to [`adopt_checkout`]. A miss is an answer, not a failure:
/// the caller says what it could not read rather than fetching behind the
/// operator's back.
pub fn materialized_checkout(
    peppy_dirs: &PeppyDirs,
    repo_url: &str,
    commit: &GitCommit,
) -> Option<PathBuf> {
    let dir = checkout_dir_for(peppy_dirs, repo_url, commit);
    let lock = LOCKS.lock_for(&dir.to_string_lossy());
    let _guard = lock.lock();
    is_checked_out_at(&dir, commit).then_some(dir)
}

/// Ensures a checkout of `commit` exists and returns its working-tree
/// directory.
///
/// A missing checkout is fetched as [`fetch_commit`] tells: the commit alone
/// at depth 1, or the history of `read_ref`, the ref the entry was read at,
/// from a host that serves the commit neither under a ref nor by its hash.
/// A commit the remote no longer holds is refused rather than silently
/// answered with a tip.
///
/// Blocking; callers inside tokio should run this via
/// [`tokio::task::spawn_blocking`].
pub fn ensure_checkout_at_commit(
    peppy_dirs: &PeppyDirs,
    repo_url: &str,
    read_ref: Option<&str>,
    commit: &GitCommit,
    on_feedback: &dyn Fn(Report<'_>),
) -> std::result::Result<PathBuf, String> {
    let shown_url = SourceUrl::new(repo_url);
    let dir = checkout_dir_for(peppy_dirs, repo_url, commit);
    let lock_key = dir.to_string_lossy().into_owned();
    let lock = LOCKS.lock_for(&lock_key);
    let _guard = lock.lock();

    if is_checked_out_at(&dir, commit) {
        if mark_reuse_announced(&dir) {
            on_feedback(Report::Step(&format!(
                "Reusing cached checkout of {commit} at {}",
                dir.display()
            )));
        }
        touch_last_used(&dir);
        return Ok(dir);
    }

    clear_dir(&dir)?;

    on_feedback(Report::Step(&format!(
        "Fetching {shown_url} at {commit} into cache at {}",
        dir.display()
    )));
    let repo = init_empty_repository(&dir)?;
    fetch_commit(&repo, repo_url, read_ref, commit, on_feedback)?;
    checkout_repo_ref(&repo, commit.as_str()).map_err(|e| {
        format!("Failed to check out commit {commit} of {shown_url} after fetching it: {e}")
    })?;
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
/// same filesystem as the cache. Both hold for a clone made under
/// [`PeppyDirs::tmp_dir`]. Ownership of `dir` passes here either way.
///
/// Best-effort, and infallible from the caller's side: whatever goes wrong,
/// the cache is left as it was and the checkout is cloned again on demand.
pub fn adopt_checkout(peppy_dirs: &PeppyDirs, repo_url: &str, commit: &GitCommit, dir: PathBuf) {
    let shown_url = SourceUrl::new(repo_url);
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
        debug!("Keeping the clone of {shown_url} out of the checkout cache: {e}");
        discard(&dir);
        return;
    }
    match std::fs::rename(&dir, &dst) {
        Ok(()) => {
            debug!(
                "Adopted the clone of {shown_url} at {commit} as {}",
                dst.display()
            );
            touch_last_used(&dst);
        }
        Err(e) => {
            debug!("Keeping the clone of {shown_url} out of the checkout cache: {e}");
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
            Ok(()) => {
                forget_reuse_announced(&dir);
                removed += 1;
            }
            Err(e) => warn!("Failed to remove stale checkout {}: {e}", dir.display()),
        }
    }
    removed
}

/// Brings `commit` into `repo`, a repository that holds nothing yet.
///
/// Two attempts. The commit alone at depth 1 is its tree and nothing else:
/// every host serves it while a ref it advertises points at it, and a host
/// that serves commits by hash, as GitHub does, serves it after its refs
/// have moved past it (see [`fetch_commit_shallow`]). A host that does
/// neither gets `read_ref` fetched with its whole history instead.
fn fetch_commit(
    repo: &git2::Repository,
    repo_url: &str,
    read_ref: Option<&str>,
    commit: &GitCommit,
    on_feedback: &dyn Fn(Report<'_>),
) -> std::result::Result<(), String> {
    let shown_url = SourceUrl::new(repo_url);
    let shallow = fetch_commit_shallow(repo, repo_url, commit, &mut |line| {
        on_feedback(Report::Progress(line))
    });
    if shallow.is_ok() && has_commit(repo, commit) {
        return Ok(());
    }

    let refspec = read_ref.unwrap_or("HEAD");
    on_feedback(Report::Step(&format!(
        "{shown_url} does not serve commit {commit} on its own; fetching the full history of \
         {refspec}"
    )));
    repo.remote_anonymous(repo_url)
        .and_then(|mut remote| {
            fetch_with_progress(&mut remote, repo_url, refspec, false, &mut |line| {
                on_feedback(Report::Progress(line))
            })
        })
        .map_err(|e| {
            let error = shown_url.redact_in(&e.to_string());
            format!("Failed to fetch {refspec} from {shown_url}: {error}")
        })?;

    if has_commit(repo, commit) {
        return Ok(());
    }
    Err(format!(
        "commit {commit} is not reachable at {shown_url}. The pin names bytes this remote no \
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
    use crate::services::node::test_git_daemon::GitDaemon;

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

    /// A source repository with one commit per entry of its contents, and
    /// the Peppy home whose checkout cache reads it. The temp dirs live as
    /// long as the fixture.
    struct SourceRepo {
        _home: tempfile::TempDir,
        peppy_dirs: PeppyDirs,
        _source: tempfile::TempDir,
        source_dir: PathBuf,
        commits: Vec<GitCommit>,
        url: String,
        branch: String,
    }

    impl SourceRepo {
        fn new(contents: &[&str]) -> Self {
            let home = tempfile::tempdir().unwrap();
            let peppy_dirs = PeppyDirs::new(home.path());
            let source = tempfile::tempdir().unwrap();
            let source_dir = source.path().join("source-repo");
            let commits = repo_with_commits(&source_dir, contents);
            let url = source_dir.display().to_string();
            let branch = head_branch(&source_dir);
            Self {
                _home: home,
                peppy_dirs,
                _source: source,
                source_dir,
                commits,
                url,
                branch,
            }
        }

        /// The same source served over `git://` (see [`GitDaemon`]), whose
        /// checkouts go through the depth-1 fetch peppy makes of a real
        /// remote. `serves_hashes` is whether the host hands out a commit by
        /// its hash, as GitHub does.
        fn served(contents: &[&str], serves_hashes: bool) -> (Self, GitDaemon) {
            let mut repo = Self::new(contents);
            git2::Repository::open(&repo.source_dir)
                .expect("open source repo")
                .config()
                .expect("source repo config")
                .set_bool("uploadpack.allowReachableSHA1InWant", serves_hashes)
                .expect("configure fetches by hash");
            let served_root = repo.source_dir.parent().expect("the source has a parent");
            let daemon = GitDaemon::serve(served_root);
            repo.url = daemon.url("source-repo");
            (repo, daemon)
        }

        /// Ensures the checkout of `commit` from this source, read at
        /// `read_ref`.
        fn checkout(&self, read_ref: Option<&str>, commit: &GitCommit) -> Result<PathBuf, String> {
            ensure_checkout_at_commit(&self.peppy_dirs, &self.url, read_ref, commit, &|_| {})
        }

        /// The same, returning beside the checkout the feedback lines the
        /// call reported.
        fn checkout_recording_lines(
            &self,
            read_ref: Option<&str>,
            commit: &GitCommit,
        ) -> (Result<PathBuf, String>, Vec<String>) {
            let lines = std::cell::RefCell::new(Vec::new());
            let checkout =
                ensure_checkout_at_commit(&self.peppy_dirs, &self.url, read_ref, commit, &|line| {
                    lines.borrow_mut().push(line.text().to_owned())
                });
            (checkout, lines.into_inner())
        }
    }

    #[test]
    fn a_checkout_dir_is_named_after_the_url_as_shown() {
        let tmp = tempfile::tempdir().unwrap();
        let peppy_dirs = PeppyDirs::new(tmp.path());
        let commit = GitCommit::parse(&"a".repeat(40)).unwrap();
        let with_token = "https://oauth2:glpat-secret-token@gitlab.example/acme/nodes.git";
        let with_other_token = "https://oauth2:glpat-other-token@gitlab.example/acme/nodes.git";

        let dir = checkout_dir_for(&peppy_dirs, with_token, &commit);
        let name = dir.file_name().unwrap().to_string_lossy().into_owned();
        assert!(
            name.starts_with("https___gitlab.example_acme_nodes.git-"),
            "{name}"
        );
        assert!(!name.contains("secret"), "{name}");
        // Two URLs that differ in their credentials alone keep their own
        // checkouts.
        assert_ne!(
            dir,
            checkout_dir_for(&peppy_dirs, with_other_token, &commit)
        );
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
        let repo = SourceRepo::new(&["first"]);
        let commit = &repo.commits[0];

        let checkout = repo
            .checkout(Some(&repo.branch), commit)
            .expect("initial checkout");
        assert_eq!(
            std::fs::read_to_string(checkout.join("tracked.txt")).unwrap(),
            "first"
        );

        // Deleting the source proves the second call touched no network:
        // a clone or fetch against a missing path fails outright.
        std::fs::remove_dir_all(&repo.source_dir).expect("remove source repo");
        let (reused, lines) = repo.checkout_recording_lines(Some(&repo.branch), commit);
        let reused = reused.expect("a populated checkout needs no remote");
        assert_eq!(checkout, reused);
        assert_eq!(
            lines,
            vec![format!(
                "Reusing cached checkout of {} at {}",
                commit,
                reused.display()
            )]
        );

        // The reuse was announced the first time; a launch resolves dozens
        // of items out of one checkout, so a repeat would bury the lines
        // that matter.
        let (again, lines) = repo.checkout_recording_lines(Some(&repo.branch), commit);
        let again = again.expect("a populated checkout needs no remote");
        assert_eq!(reused, again);
        assert_eq!(
            lines,
            Vec::<String>::new(),
            "an already-announced reuse reports nothing"
        );
    }

    /// A checkout that leaves the cache and is cloned again is a new
    /// population: its first reuse is announced again, so the
    /// once-per-population rule cannot outlive the bytes it described.
    #[test]
    fn ensure_checkout_at_commit_reannounces_a_rematerialized_checkout() {
        let repo = SourceRepo::new(&["first"]);
        let commit = &repo.commits[0];

        let checkout = repo
            .checkout(Some(&repo.branch), commit)
            .expect("initial checkout");
        let (reused, lines) = repo.checkout_recording_lines(Some(&repo.branch), commit);
        reused.expect("first reuse");
        assert_eq!(lines.len(), 1);

        // Strip the git dir, the state a clone killed half-way through
        // leaves, so the next call cannot trust the directory and clones
        // afresh rather than reusing it.
        std::fs::remove_dir_all(checkout.join(".git")).expect("break the checkout");

        repo.checkout(Some(&repo.branch), commit)
            .expect("re-materialized checkout");

        let (reused, lines) = repo.checkout_recording_lines(Some(&repo.branch), commit);
        reused.expect("reuse of the new population");
        assert_eq!(
            lines.len(),
            1,
            "the first reuse of a re-materialized checkout is announced again"
        );
    }

    /// Prune removes a checkout the caches no longer name, and the reuse
    /// announcement goes with it: a checkout later cloned at the same key is
    /// a new population announced on its own first reuse.
    #[test]
    fn prune_checkouts_forgets_the_reuse_announcement() {
        let repo = SourceRepo::new(&["first"]);
        let commit = &repo.commits[0];

        let checkout = repo
            .checkout(Some(&repo.branch), commit)
            .expect("initial checkout");
        let (reused, lines) = repo.checkout_recording_lines(Some(&repo.branch), commit);
        reused.expect("first reuse");
        assert_eq!(lines.len(), 1);

        // The grace marker would keep a checkout just handed out; removing
        // it stands in for the grace period having elapsed.
        std::fs::remove_file(last_used_marker(&checkout)).expect("drop the grace marker");
        assert_eq!(prune_checkouts(&repo.peppy_dirs, std::iter::empty()), 1);

        repo.checkout(Some(&repo.branch), commit)
            .expect("checkout cloned again");

        let (reused, lines) = repo.checkout_recording_lines(Some(&repo.branch), commit);
        reused.expect("reuse of the new population");
        assert_eq!(
            lines.len(),
            1,
            "the first reuse of a pruned-and-recloned checkout is announced again"
        );
    }

    #[test]
    fn ensure_checkout_at_commit_resolves_a_commit_the_branch_moved_past() {
        let repo = SourceRepo::new(&["first", "second"]);

        // The branch now points at "second"; the pin names "first".
        let checkout = repo
            .checkout(Some(&repo.branch), &repo.commits[0])
            .expect("a commit behind the branch tip still resolves");
        assert_eq!(
            std::fs::read_to_string(checkout.join("tracked.txt")).unwrap(),
            "first",
            "the pinned commit is what gets checked out, not the branch tip"
        );

        let tip = repo
            .checkout(Some(&repo.branch), &repo.commits[1])
            .expect("the tip resolves too");
        assert_ne!(checkout, tip, "two commits are two checkouts");
        assert_eq!(
            std::fs::read_to_string(tip.join("tracked.txt")).unwrap(),
            "second"
        );
    }

    /// Whether the checkout at `dir` holds `commit` in its object database.
    fn holds(dir: &Path, commit: &GitCommit) -> bool {
        git2::Repository::open(dir)
            .expect("open the checkout")
            .find_commit(git2::Oid::from_str(commit.as_str()).unwrap())
            .is_ok()
    }

    /// Whether the checkout at `dir` is a depth-1 fetch.
    fn is_shallow(dir: &Path) -> bool {
        dir.join(".git").join("shallow").exists()
    }

    /// A host that serves commits by hash hands out a commit its branch has
    /// moved past on its own: the checkout holds neither the commit before it
    /// nor the tip.
    #[test]
    fn ensure_checkout_at_commit_fetches_a_commit_behind_the_tip_alone() {
        let (repo, _daemon) = SourceRepo::served(&["first", "second", "third"], true);
        let [first, second, third] = &repo.commits[..] else {
            panic!("three commits");
        };

        let (checkout, lines) = repo.checkout_recording_lines(Some(&repo.branch), second);
        let checkout = checkout.expect("the commit is fetched by its hash");

        assert_eq!(
            std::fs::read_to_string(checkout.join("tracked.txt")).unwrap(),
            "second"
        );
        assert!(is_shallow(&checkout));
        assert!(
            !holds(&checkout, first),
            "the commit before it is not fetched"
        );
        assert!(!holds(&checkout, third), "the tip is not fetched");
        assert!(
            !lines.iter().any(|line| line.contains("full history")),
            "{lines:?}"
        );
    }

    /// A host that does not serve commits by hash still hands out a commit a
    /// ref points at, through that ref: a branch tip, or the commit of an
    /// annotated tag.
    #[test]
    fn ensure_checkout_at_commit_fetches_a_commit_a_ref_points_at_alone_from_any_host() {
        let (repo, _daemon) = SourceRepo::served(&["first", "second", "third"], false);
        let [first, second, third] = &repo.commits[..] else {
            panic!("three commits");
        };
        let source = git2::Repository::open(&repo.source_dir).expect("open source repo");
        let signature = git2::Signature::now("Peppy", "peppy@example.com").unwrap();
        let tagged = source
            .find_commit(git2::Oid::from_str(first.as_str()).unwrap())
            .unwrap();
        source
            .tag("v1", tagged.as_object(), &signature, "v1", false)
            .expect("tag the first commit");

        let tip = repo
            .checkout(Some(&repo.branch), third)
            .expect("the tip is fetched through its branch");
        assert!(is_shallow(&tip));
        assert!(
            !holds(&tip, second),
            "the commit before the tip is not fetched"
        );

        let (at_tag, lines) = repo.checkout_recording_lines(Some("v1"), first);
        let at_tag = at_tag.expect("the tagged commit is fetched through its tag");
        assert_eq!(
            std::fs::read_to_string(at_tag.join("tracked.txt")).unwrap(),
            "first"
        );
        assert!(is_shallow(&at_tag));
        assert!(
            !lines.iter().any(|line| line.contains("full history")),
            "{lines:?}"
        );
    }

    /// A host that does not serve commits by hash gets the history of the
    /// ref the entry was read at fetched for a commit no ref points at.
    #[test]
    fn ensure_checkout_at_commit_fetches_the_history_of_the_read_ref_from_a_host_without_hashes() {
        let (repo, _daemon) = SourceRepo::served(&["first", "second", "third"], false);
        let second = &repo.commits[1];

        let (checkout, lines) = repo.checkout_recording_lines(Some(&repo.branch), second);
        let checkout = checkout.expect("the commit is in the history of the branch");

        assert_eq!(
            std::fs::read_to_string(checkout.join("tracked.txt")).unwrap(),
            "second"
        );
        assert!(
            lines.iter().any(|line| line.contains(&format!(
                "does not serve commit {second} on its own; fetching the full history of {}",
                repo.branch
            ))),
            "{lines:?}"
        );
    }

    #[test]
    fn ensure_checkout_at_commit_refuses_a_commit_the_remote_does_not_have() {
        let repo = SourceRepo::new(&["first"]);
        let absent = GitCommit::parse(&"0".repeat(40)).unwrap();

        let error = repo
            .checkout(Some(&repo.branch), &absent)
            .expect_err("a commit the remote never had cannot be checked out");
        assert!(
            error.contains(absent.as_str()) && error.contains("not reachable"),
            "the refusal names the commit it could not reach, got: {error}"
        );
    }

    /// The donation is the whole point of the cache being keyed by commit:
    /// a clone somebody else already paid for is the same bytes, so nothing
    /// is fetched afterwards. Proven by deleting the source repository
    /// before asking for the checkout: a clone or fetch would fail.
    #[test]
    fn adopt_checkout_takes_over_a_clone_made_elsewhere() {
        let repo = SourceRepo::new(&["first"]);
        let commit = &repo.commits[0];

        let tmp_root = repo.peppy_dirs.tmp_dir();
        std::fs::create_dir_all(&tmp_root).expect("create the tmp root");
        let donor = tempfile::tempdir_in(&tmp_root).unwrap();
        let clone = donor.path().join("clone");
        crate::services::node::shallow_checkout_of_read_ref(&repo.url, None, &clone, &mut |_| {})
            .expect("clone the source");
        adopt_checkout(&repo.peppy_dirs, &repo.url, commit, clone);

        std::fs::remove_dir_all(&repo.source_dir).expect("remove source repo");
        let (checkout, lines) = repo.checkout_recording_lines(None, commit);
        let checkout = checkout.expect("the donated clone needs no remote");

        assert_eq!(
            checkout,
            checkout_dir_for(&repo.peppy_dirs, &repo.url, commit)
        );
        assert_eq!(
            std::fs::read_to_string(checkout.join("tracked.txt")).unwrap(),
            "first"
        );
        assert!(
            lines[0].starts_with("Reusing cached checkout"),
            "the donated clone is reused, not re-cloned"
        );
    }

    /// A key that already holds the commit already holds the same bytes, so
    /// the donation is dropped rather than swapped in, and the donor never
    /// leaks either way.
    #[test]
    fn adopt_checkout_keeps_the_checkout_already_at_the_key() {
        let repo = SourceRepo::new(&["first"]);
        let commit = &repo.commits[0];

        let existing = repo.checkout(None, commit).expect("initial checkout");
        std::fs::write(existing.join("marker.txt"), "original").expect("mark the original");

        let tmp_root = repo.peppy_dirs.tmp_dir();
        std::fs::create_dir_all(&tmp_root).expect("create the tmp root");
        let donor = tempfile::tempdir_in(&tmp_root).unwrap();
        let clone = donor.path().join("clone");
        crate::services::node::shallow_checkout_of_read_ref(&repo.url, None, &clone, &mut |_| {})
            .expect("clone the source");
        adopt_checkout(&repo.peppy_dirs, &repo.url, commit, clone.clone());

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
    /// is released (`node add` copies the tree out), so a refresh that
    /// supersedes the pin mid-add must not pull the directory out from
    /// under it.
    #[test]
    fn prune_checkouts_keeps_a_checkout_handed_out_recently() {
        let repo = SourceRepo::new(&["first"]);

        let checkout = repo
            .checkout(None, &repo.commits[0])
            .expect("initial checkout");

        assert_eq!(prune_checkouts(&repo.peppy_dirs, std::iter::empty()), 0);
        assert!(
            checkout.exists(),
            "a checkout just handed out is still in use"
        );
    }

    #[test]
    fn ensure_checkout_at_commit_replaces_an_incomplete_checkout() {
        let repo = SourceRepo::new(&["first"]);
        let commit = &repo.commits[0];

        // A directory at the key with no `.git`: what a clone killed
        // half-way through leaves behind.
        let dir = checkout_dir_for(&repo.peppy_dirs, &repo.url, commit);
        std::fs::create_dir_all(&dir).expect("create partial checkout");
        std::fs::write(dir.join("tracked.txt"), "garbage").expect("write partial content");

        let checkout = repo
            .checkout(Some(&repo.branch), commit)
            .expect("an incomplete checkout is replaced rather than trusted");
        assert_eq!(checkout, dir);
        assert_eq!(
            std::fs::read_to_string(checkout.join("tracked.txt")).unwrap(),
            "first"
        );
    }
}
