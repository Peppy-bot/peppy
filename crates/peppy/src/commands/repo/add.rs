use std::sync::Arc;
use std::time::Duration;

use core_node::encoding::RepoAddRequest;
use tracing::info;

use crate::commands::CALLER_INSTANCE_ID;
use crate::commands::node::source;
use crate::context::AppContext;
use crate::error::{Error, Result};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

pub(super) fn add_repo(
    ctx: &Arc<AppContext>,
    source_str: &str,
    git_ref: Option<String>,
) -> Result<()> {
    if !source::is_probably_remote_source(source_str) {
        return Err(Error::ExecutionFailed(format!(
            "'{source_str}' is not a valid repository URL. Expected a git URL \
             (e.g. https://github.com/org/repo.git)."
        )));
    }

    // Plain HTTP archives / URLs are not repository sources.
    if let Ok(url) = url::Url::parse(source_str)
        && matches!(url.scheme(), "http" | "https")
        && !source::looks_like_git_url(source_str)
    {
        return Err(Error::ExecutionFailed(
            "URL repositories are not supported yet. \
             Please provide a git repository URL (e.g. https://github.com/org/repo.git)."
                .to_string(),
        ));
    }

    let (repo_url, _repo_path) = source::parse_git_repo_url_and_path(source_str)?;
    let repo_url_str = repo_url.to_bstring().to_string();
    info!("Adding repository {} (ref: {:?})", repo_url_str, git_ref);

    crate::commands::block_on(add_repo_async(ctx, repo_url_str, git_ref))
}

async fn add_repo_async(
    ctx: &Arc<AppContext>,
    repo_url: String,
    git_ref: Option<String>,
) -> Result<()> {
    let conn = ctx.connect_to_daemon().await?;

    let response = RepoAddRequest::new_git(repo_url.clone(), git_ref)
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
        info!("Repository '{}' added successfully", repo_url);
        Ok(())
    } else {
        Err(Error::ExecutionFailed(format!(
            "Failed to add repository: {}",
            response.error_message
        )))
    }
}
