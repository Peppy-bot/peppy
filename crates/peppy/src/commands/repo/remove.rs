use std::sync::Arc;

use core_node_api::encoding::RepoRemoveRequest;
use tracing::{info, warn};

use crate::commands::CALLER_INSTANCE_ID;
use crate::commands::repo::REQUEST_TIMEOUT;
use crate::context::AppContext;
use crate::error::Result;
use peppylib::core_node::transport::poll;

pub(super) fn remove_repo(ctx: &Arc<AppContext>, id: u64) -> Result<()> {
    info!("Removing repository with id {id}");

    crate::commands::block_on(remove_repo_async(ctx, id))
}

async fn remove_repo_async(ctx: &Arc<AppContext>, id: u64) -> Result<()> {
    let conn = ctx.connect_to_daemon().await?;

    let request = RepoRemoveRequest::new(id);
    let response = poll(
        &request,
        conn.messenger,
        &conn.core_node_name,
        CALLER_INSTANCE_ID,
        &conn.target_core_node,
        REQUEST_TIMEOUT,
    )
    .await
    .map_err(|e| crate::commands::request_error("Failed to remove repository", e))?;

    info!("Repository with id {id} removed successfully");
    // The removal landed, but the re-index that makes it take effect may not
    // have.
    if !response.refresh_report.is_empty() {
        warn!("Re-indexing after the removal: {}", response.refresh_report);
    }
    Ok(())
}
