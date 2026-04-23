use std::sync::Arc;
use std::time::Duration;

use core_node_api::encoding::{RepoExcludeRequest, RepoSource};
use tracing::info;

use crate::commands::CALLER_INSTANCE_ID;
use crate::commands::repo::add::parse_repo_source;
use crate::commands::repo::repo_source_label;
use crate::context::AppContext;
use crate::error::{Error, Result};
use peppylib::core_node::transport::poll_repo_exclude;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

pub(super) fn exclude_repo(
    ctx: &Arc<AppContext>,
    source_str: &str,
    git_ref: Option<String>,
) -> Result<()> {
    let repo_source = parse_repo_source(source_str, git_ref)?;
    let label = repo_source_label(&repo_source);
    info!("Excluding repository {label}");

    crate::commands::block_on(exclude_repo_async(ctx, repo_source, label))
}

async fn exclude_repo_async(
    ctx: &Arc<AppContext>,
    repo_source: RepoSource,
    label: String,
) -> Result<()> {
    let conn = ctx.connect_to_daemon().await?;

    let request = RepoExcludeRequest {
        source: repo_source,
    };
    let response = poll_repo_exclude(
        &request,
        conn.messenger,
        &conn.core_node_name,
        CALLER_INSTANCE_ID,
        &conn.core_node_name,
        REQUEST_TIMEOUT,
    )
    .await
    .map_err(|e| Error::ExecutionFailed(format!("Failed to exclude repository: {}", e)))?;

    if response.success {
        info!("Repository '{label}' excluded successfully");
        Ok(())
    } else {
        Err(Error::ExecutionFailed(format!(
            "Failed to exclude repository: {}",
            response.error_message
        )))
    }
}
