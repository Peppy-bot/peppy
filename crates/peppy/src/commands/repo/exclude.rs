use std::sync::Arc;

use core_node_api::encoding::{GitRepoRef, RepoExcludeRequest, RepoSource};
use tracing::{info, warn};

use crate::commands::caller_instance_id;
use crate::commands::repo::add::parse_repo_source;
use crate::commands::repo::{REQUEST_TIMEOUT, repo_source_label};
use crate::context::AppContext;
use crate::error::Result;
use peppylib::core_node::transport::poll;

pub(super) fn exclude_repo(
    ctx: &Arc<AppContext>,
    source_str: &str,
    git_ref: Option<GitRepoRef>,
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
    let response = poll(
        &request,
        conn.messenger,
        &conn.core_node_name,
        caller_instance_id(),
        &conn.target_core_node,
        REQUEST_TIMEOUT,
    )
    .await
    .map_err(|e| crate::commands::request_error("Failed to exclude repository", e))?;

    info!("Repository '{label}' excluded successfully");
    // The exclusion landed, but the re-index that makes it take effect may
    // not have. Saying so matters most here: excluding is what a blocked
    // user reaches for, and they need to know whether it unblocked them.
    if !response.refresh_report.is_empty() {
        warn!(
            "Re-indexing after the exclusion: {}",
            response.refresh_report
        );
    }
    Ok(())
}
