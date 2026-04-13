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
) -> Result<()> {
    let repo_source = parse_repo_source(source_str, git_ref)?;
    let label = repo_source_label(&repo_source);
    info!("Adding repository {label}");

    crate::commands::block_on(add_repo_async(ctx, repo_source, label))
}

async fn add_repo_async(
    ctx: &Arc<AppContext>,
    repo_source: RepoSource,
    label: String,
) -> Result<()> {
    let conn = ctx.connect_to_daemon().await?;

    let request = RepoAddRequest {
        source: repo_source,
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
        return Ok(RepoSource::Fs(source_str.into()));
    }

    if source::looks_like_git_url(source_str) {
        let (repo_url, _repo_path) = source::parse_git_repo_url_and_path(source_str)?;
        let repo_url_str = repo_url.to_bstring().to_string();
        return Ok(RepoSource::Git {
            repo_url: repo_url_str,
            repo_ref: git_ref,
        });
    }

    // Plain URL
    if git_ref.is_some() {
        return Err(Error::ExecutionFailed(
            "`--ref` is only supported for git sources".to_string(),
        ));
    }
    Ok(RepoSource::Url(source_str.to_string()))
}
