use std::sync::Arc;
use std::time::Duration;

use core_node::encoding::RepoRemoveRequest;
use tracing::info;

use crate::commands::CALLER_INSTANCE_ID;
use crate::context::AppContext;
use crate::error::{Error, Result};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

pub(super) fn remove_repo(ctx: &Arc<AppContext>, id: u64) -> Result<()> {
    info!("Removing repository with id {id}");

    crate::commands::block_on(remove_repo_async(ctx, id))
}

async fn remove_repo_async(ctx: &Arc<AppContext>, id: u64) -> Result<()> {
    let conn = ctx.connect_to_daemon().await?;

    let request = RepoRemoveRequest::new(id);
    let response = request
        .poll(
            conn.messenger,
            &conn.core_node_name,
            CALLER_INSTANCE_ID,
            &conn.core_node_name,
            REQUEST_TIMEOUT,
        )
        .await
        .map_err(|e| Error::ExecutionFailed(format!("Failed to remove repository: {}", e)))?;

    if response.success {
        info!("Repository with id {id} removed successfully");
        Ok(())
    } else {
        Err(Error::ExecutionFailed(format!(
            "Failed to remove repository: {}",
            response.error_message
        )))
    }
}
