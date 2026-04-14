use std::sync::Arc;
use std::time::Duration;

use core_node::encoding::{RepoAddRequest, RepoSource};
use tracing::info;

use crate::commands::CALLER_INSTANCE_ID;
use crate::commands::node::source;
use crate::commands::repo::repo_source_label;
use crate::context::AppContext;
use crate::error::{Error, Result};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

pub(super) fn add_repo(
    ctx: &Arc<AppContext>,
    source_str: &str,
    git_ref: Option<String>,
    top: bool,
) -> Result<()> {
    let repo_source = parse_repo_source(source_str, git_ref)?;
    let label = repo_source_label(&repo_source);
    info!("Adding repository {label}");

    crate::commands::block_on(add_repo_async(ctx, repo_source, label, top))
}

async fn add_repo_async(
    ctx: &Arc<AppContext>,
    repo_source: RepoSource,
    label: String,
    top: bool,
) -> Result<()> {
    let conn = ctx.connect_to_daemon().await?;

    let request = RepoAddRequest {
        source: repo_source,
        top,
    };
    let response = request
        .poll(
            conn.messenger,
            &conn.core_node_name,
            CALLER_INSTANCE_ID,
            &conn.core_node_name,
            REQUEST_TIMEOUT,
        )
        .await
        .map_err(|e| Error::ExecutionFailed(format!("Failed to add repository: {}", e)))?;

    if response.success {
        info!("Repository '{label}' added successfully");
        Ok(())
    } else {
        Err(Error::ExecutionFailed(format!(
            "Failed to add repository: {}",
            response.error_message
        )))
    }
}

/// Parse a user-supplied source string into a [`RepoSource`].
///
/// Accepts:
/// - Local filesystem paths (absolute or relative)
/// - Git URLs (contains `.git` or starts with `git@`/`ssh://`)
/// - Plain HTTP/HTTPS URLs
pub(crate) fn parse_repo_source(source_str: &str, git_ref: Option<String>) -> Result<RepoSource> {
    if !source::is_probably_remote_source(source_str) {
        // Local filesystem path
        if git_ref.is_some() {
            return Err(Error::ExecutionFailed(
                "`--ref` is only supported for git sources".to_string(),
            ));
        }
        let path = std::path::Path::new(source_str);
        let resolved = std::fs::canonicalize(path).unwrap_or_else(|_| {
            std::env::current_dir()
                .map(|cwd| cwd.join(path))
                .unwrap_or_else(|_| path.to_path_buf())
        });
        return Ok(RepoSource::Fs(resolved));
    }

    if source::looks_like_git_url(source_str) {
        let (repo_url, _repo_path) = source::parse_git_repo_url_and_path(source_str)?;
        let repo_url_str = repo_url.to_bstring().to_string();
        return Ok(RepoSource::Git {
            repo_url: repo_url_str,
            repo_ref: git_ref,
        });
    }

    // Plain URL — but if the user passed `--ref`, try to treat it as a git
    // clone URL first (e.g. `https://github.com/org/repo` without `.git`).
    if git_ref.is_some() {
        if let Ok((repo_url, _repo_path)) = source::parse_git_repo_url_and_path(source_str) {
            return Ok(RepoSource::Git {
                repo_url: repo_url.to_bstring().to_string(),
                repo_ref: git_ref,
            });
        }
        return Err(Error::ExecutionFailed(
            "`--ref` is only supported for git sources".to_string(),
        ));
    }
    Ok(RepoSource::Url(source_str.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_repo_source_fs_canonicalizes_existing_path() {
        let tmp = tempfile::tempdir().unwrap();
        let canonical = std::fs::canonicalize(tmp.path()).unwrap();
        let input = tmp.path().to_str().unwrap();

        let parsed = parse_repo_source(input, None).unwrap();
        let RepoSource::Fs(p) = parsed else {
            panic!("expected Fs variant");
        };
        assert_eq!(p, canonical);
    }

    #[test]
    fn parse_repo_source_fs_falls_back_on_nonexistent() {
        // Must not error even if the path does not exist yet.
        let parsed = parse_repo_source("/definitely/does/not/exist/xyz", None).unwrap();
        let RepoSource::Fs(p) = parsed else {
            panic!("expected Fs variant");
        };
        assert_eq!(p, std::path::Path::new("/definitely/does/not/exist/xyz"));
    }

    #[test]
    fn parse_repo_source_https_without_git_suffix_with_ref_returns_git() {
        let parsed =
            parse_repo_source("https://github.com/org/repo", Some("main".to_string())).unwrap();
        match parsed {
            RepoSource::Git { repo_url, repo_ref } => {
                assert!(repo_url.contains("github.com/org/repo"));
                assert_eq!(repo_ref.as_deref(), Some("main"));
            }
            other => panic!("expected Git variant, got {:?}", other),
        }
    }

    #[test]
    fn parse_repo_source_fs_path_with_ref_fails() {
        // --ref on a local path is still rejected.
        let tmp = tempfile::tempdir().unwrap();
        let input = tmp.path().to_str().unwrap();
        let result = parse_repo_source(input, Some("main".to_string()));
        assert!(result.is_err(), "expected error for fs path with --ref");
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("--ref"),
            "error should mention --ref, got: {msg}"
        );
    }
}
