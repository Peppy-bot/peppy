use std::sync::Arc;
use std::time::Duration;

use core_node::encoding::RepoListRequest;
use tracing::info;

use crate::commands::CALLER_INSTANCE_ID;
use crate::context::AppContext;
use crate::error::{Error, Result};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

pub(super) fn list_repos(ctx: &Arc<AppContext>) -> Result<()> {
    crate::commands::block_on(list_repos_async(ctx))
}

async fn list_repos_async(ctx: &Arc<AppContext>) -> Result<()> {
    let conn = ctx.connect_to_daemon().await?;

    let response = RepoListRequest
        .poll(
            conn.messenger,
            &conn.core_node_name,
            CALLER_INSTANCE_ID,
            &conn.core_node_name,
            REQUEST_TIMEOUT,
        )
        .await
        .map_err(|e| Error::ExecutionFailed(format!("Failed to list repositories: {}", e)))?;

    if !response.success {
        return Err(Error::ExecutionFailed(format!(
            "Failed to list repositories: {}",
            response.error_message.unwrap_or_default()
        )));
    }

    if response.nodes.is_empty() {
        info!("No nodes found. Run `peppy repo refresh` to discover nodes.");
        return Ok(());
    }

    for node in &response.nodes {
        info!(
            "{} ({}) [{}] {}",
            node.node_name, node.node_tag, node.source_type, node.path
        );
    }

    Ok(())
}
