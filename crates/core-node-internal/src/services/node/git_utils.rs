//! Git utilities shared by the node command handlers: repo-path
//! sanitization, ref checkout, and a clone that honors a deadline on the
//! network transfer.

use git2::Repository;
use git2::build::{CheckoutBuilder, RepoBuilder};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
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

/// Clones a git repository, aborting the network transfer if `deadline` is
/// exceeded.  When `deadline` is `None` the clone runs without any time limit.
pub(crate) fn clone_repo_with_deadline(
    repo_url: &str,
    dest: &Path,
    deadline: Option<Instant>,
) -> std::result::Result<Repository, String> {
    let deadline_triggered = Arc::new(AtomicBool::new(false));

    let mut callbacks = git2::RemoteCallbacks::new();
    if let Some(deadline) = deadline {
        let flag = Arc::clone(&deadline_triggered);
        callbacks.transfer_progress(move |_progress| {
            if Instant::now() >= deadline {
                flag.store(true, Ordering::SeqCst);
                return false;
            }
            true
        });
    }

    let mut fetch_opts = git2::FetchOptions::new();
    fetch_opts.remote_callbacks(callbacks);

    RepoBuilder::new()
        .fetch_options(fetch_opts)
        .clone(repo_url, dest)
        .map_err(|e| {
            if deadline_triggered.load(Ordering::SeqCst) {
                format!("Git clone timed out for {}", repo_url)
            } else {
                format!("Failed to clone repository: {}", e)
            }
        })
}
