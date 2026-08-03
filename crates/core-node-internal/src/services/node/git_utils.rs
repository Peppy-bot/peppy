//! Git utilities shared by the node command handlers: repo-path
//! sanitization, ref checkout, reading the commit a working tree sits on,
//! and a clone that honors a deadline on the network transfer.

use daemon_config::repository::GitCommit;
use git2::Repository;
use git2::build::{CheckoutBuilder, RepoBuilder};
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, Instant};

/// Throttle window between consecutive `transfer_progress` reports surfaced
/// to the user: fast enough to feel live, slow enough to avoid flooding
/// the feedback channel.
const PROGRESS_REPORT_INTERVAL: Duration = Duration::from_millis(500);

/// Returns `true` for URLs that dispatch through libgit2's local transport,
/// which rejects `depth(1)` shallow fetches with
/// "shallow fetch is not supported by the local transport".
fn is_local_url(repo_url: &str) -> bool {
    repo_url.starts_with('/') || repo_url.starts_with("file://")
}

/// Renders byte counts in the compact form used by all clone/fetch progress
/// lines (`KB`/`MB`).
pub(crate) fn format_bytes(bytes: usize) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = 1024.0 * 1024.0;
    let b = bytes as f64;
    if b >= MB {
        format!("{:.1} MB", b / MB)
    } else if b >= KB {
        format!("{:.0} KB", b / KB)
    } else {
        format!("{} B", bytes)
    }
}

pub(crate) fn sanitize_repo_path(repo_path: &str) -> std::result::Result<PathBuf, String> {
    let trimmed = repo_path.trim().trim_start_matches(['/', '\\']);
    if trimmed.is_empty() {
        return Err("repo_path must not be empty".to_string());
    }
    let path = PathBuf::from(trimmed);
    for component in path.components() {
        match component {
            Component::ParentDir => {
                return Err("repo_path must not contain '..'".to_string());
            }
            Component::RootDir => {
                return Err("repo_path must be relative".to_string());
            }
            Component::Prefix(_) => {
                return Err("repo_path must not contain a drive or UNC prefix".to_string());
            }
            _ => {}
        }
    }
    Ok(path)
}

pub(crate) fn checkout_repo_ref(
    repo: &Repository,
    repo_ref: &str,
) -> std::result::Result<(), git2::Error> {
    let repo_ref = repo_ref.trim();
    if repo_ref.is_empty() {
        return Ok(());
    }
    let object = repo
        .revparse_single(repo_ref)
        .or_else(|_| repo.revparse_single(&format!("refs/tags/{repo_ref}")))
        .or_else(|_| repo.revparse_single(&format!("refs/heads/{repo_ref}")))
        .or_else(|_| repo.revparse_single(&format!("refs/remotes/origin/{repo_ref}")))?;
    let commit = object.peel_to_commit()?;
    repo.set_head_detached(commit.id())?;
    let mut checkout = CheckoutBuilder::new();
    checkout.force();
    repo.checkout_head(Some(&mut checkout))?;
    Ok(())
}

/// Clone `repo_url` into `dst` and check out `repo_ref` if set, emitting
/// throttled (~500ms) `transfer_progress` lines via `on_progress` so callers
/// can surface live byte/object counts instead of sitting silent for the
/// duration of the clone.
///
/// Each progress line is formatted as
/// `"Cloning {repo_url}: received {recv}/{total} objects ({bytes})"`.
///
/// `shallow` requests a `depth=1` fetch, but is silently downgraded to a
/// full clone when `repo_url` targets the local transport.
/// The commit `repo`'s working tree is sitting on.
///
/// The one place a `git2::Oid` becomes a [`GitCommit`], so a caller that
/// wants to know which bytes a checkout holds compares validated commits
/// rather than rendered strings.
pub(crate) fn head_commit(repo: &Repository) -> std::result::Result<GitCommit, String> {
    let head = repo
        .head()
        .map_err(|e| format!("the repository has no HEAD to read a commit from: {e}"))?;
    let commit = head
        .peel_to_commit()
        .map_err(|e| format!("the repository's HEAD does not name a commit: {e}"))?;
    GitCommit::parse(&commit.id().to_string())
        .map_err(|e| format!("the repository's HEAD is not a usable commit: {e}"))
}

/// Shallow-clones `repo_url` into `dst`, leaving the working tree wherever
/// the remote's HEAD points.
///
/// The one way peppy fetches a repository it reads, shared by `repo refresh`
/// and the commit-keyed checkout cache so the two can never disagree about
/// what cloning a repository brings. It brings every head the remote
/// publishes at depth 1: libgit2 clones with the standard
/// `refs/heads/*:refs/remotes/origin/*` refspec whatever branch is asked
/// for, so which ref a caller cares about changes nothing about the fetch.
///
/// Callers therefore position the working tree themselves, and the
/// positioning is the only thing that differs between them: refresh onto
/// the ref a repository is configured to follow, so the commit it pins is
/// that ref's tip; the checkout cache onto the commit an entry pins, which
/// is already here whenever it is a head's tip.
pub(crate) fn clone_repo_shallow(
    repo_url: &str,
    dst: &Path,
    on_progress: &mut dyn FnMut(&str),
) -> std::result::Result<Repository, String> {
    clone_with_progress(repo_url, None, dst, true, on_progress)
}

pub(crate) fn clone_with_progress(
    repo_url: &str,
    repo_ref: Option<&str>,
    dst: &Path,
    shallow: bool,
    on_progress: &mut dyn FnMut(&str),
) -> std::result::Result<Repository, String> {
    let mut builder = RepoBuilder::new();
    let mut fetch_opts = git2::FetchOptions::new();
    if shallow && !is_local_url(repo_url) {
        fetch_opts.depth(1);
    }
    fetch_opts.remote_callbacks(progress_callbacks(repo_url, "Cloning", on_progress));
    builder.fetch_options(fetch_opts);

    let repo = builder
        .clone(repo_url, dst)
        .map_err(|e| format!("Failed to clone {}: {}", repo_url, e))?;

    if let Some(r) = repo_ref {
        checkout_repo_ref(&repo, r)
            .map_err(|e| format!("Failed to checkout ref '{}': {}", r, e))?;
    }
    Ok(repo)
}

/// Fetch `refspec` on `remote`, emitting throttled (~500ms) progress lines
/// via `on_progress` formatted as
/// `"Fetching {repo_url}: received {recv}/{total} objects ({bytes})"`.
///
/// `shallow` is downgraded to non-shallow for local transports, mirroring
/// [`clone_with_progress`].
pub(crate) fn fetch_with_progress(
    remote: &mut git2::Remote<'_>,
    repo_url: &str,
    refspec: &str,
    shallow: bool,
    on_progress: &mut dyn FnMut(&str),
) -> std::result::Result<(), git2::Error> {
    let mut fetch_opts = git2::FetchOptions::new();
    if shallow && !is_local_url(repo_url) {
        fetch_opts.depth(1);
    }
    fetch_opts.remote_callbacks(progress_callbacks(repo_url, "Fetching", on_progress));
    remote.fetch(&[refspec], Some(&mut fetch_opts), None)
}

fn progress_callbacks<'cb>(
    repo_url: &str,
    verb: &'static str,
    on_progress: &'cb mut dyn FnMut(&str),
) -> git2::RemoteCallbacks<'cb> {
    let repo_url_owned = repo_url.to_string();
    let mut last_report = Instant::now()
        .checked_sub(PROGRESS_REPORT_INTERVAL)
        .unwrap_or_else(Instant::now);
    let mut callbacks = git2::RemoteCallbacks::new();
    callbacks.transfer_progress(move |progress| {
        if last_report.elapsed() >= PROGRESS_REPORT_INTERVAL {
            last_report = Instant::now();
            on_progress(&format!(
                "{verb} {repo_url}: received {recv}/{total} objects ({bytes})",
                repo_url = repo_url_owned,
                recv = progress.received_objects(),
                total = progress.total_objects(),
                bytes = format_bytes(progress.received_bytes()),
            ));
        }
        true
    });
    callbacks
}
