//! Git utilities shared by the node command handlers: repo-path
//! sanitization, ref checkout, and a clone that honors a deadline on the
//! network transfer.

use git2::Repository;
use git2::build::{CheckoutBuilder, RepoBuilder};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

pub(crate) fn sanitize_repo_path(repo_path: &str) -> std::result::Result<PathBuf, String> {
    let trimmed = repo_path.trim_start_matches(['/', '\\']);
    let path = PathBuf::from(trimmed);
    if path.components().any(|c| matches!(c, Component::ParentDir)) {
        return Err("repo_path must not contain '..'".to_string());
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
