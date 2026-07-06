use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use core_node_api::encoding::{NodeSyncRequest, RepoSourceKind};
use tracing::info;

use super::source::resolve_node_root_dir;
use crate::commands::CALLER_INSTANCE_ID;
use crate::context::AppContext;
use crate::error::{Error, Result};

use peppylib::core_node::transport::poll_node_sync;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

pub fn sync_node(
    ctx: &Arc<AppContext>,
    path: Option<PathBuf>,
    include_repositories: bool,
) -> Result<()> {
    crate::commands::block_on(sync_node_async(ctx, path, include_repositories))
}

pub(super) async fn sync_node_async(
    ctx: &Arc<AppContext>,
    path: Option<PathBuf>,
    include_repositories: bool,
) -> Result<()> {
    let base_dir = match path {
        Some(p) => ctx.root_dir.join(p),
        None => ctx.root_dir.clone(),
    };
    let node_root_dir = resolve_node_root_dir(&base_dir)?;
    sync_resolved_node(ctx, &node_root_dir, include_repositories).await
}

/// Runs the daemon-side `node_generate` service for a node directory that has
/// already been resolved to its canonical root.
async fn sync_resolved_node(
    ctx: &Arc<AppContext>,
    node_root_dir: &Path,
    include_repositories: bool,
) -> Result<()> {
    let conn = ctx.connect_to_daemon().await?;

    // The request embeds `node_root_dir`, a caller-local path the daemon
    // resolves on its own machine: a remote target would sync (or fail on)
    // the wrong machine's filesystem.
    crate::commands::reject_remote_target_for_local_path(&conn, "peppy node sync")?;

    info!(
        "Syncing node from {} via daemon '{}'...",
        node_root_dir.display(),
        conn.target_core_node
    );

    let request = NodeSyncRequest::new(
        node_root_dir.to_path_buf(),
        conn.git_hash,
        include_repositories,
    );
    let response = poll_node_sync(
        &request,
        conn.messenger,
        &conn.core_node_name,
        CALLER_INSTANCE_ID,
        &conn.target_core_node,
        REQUEST_TIMEOUT,
    )
    .await
    .map_err(|e| Error::ExecutionFailed(format!("Failed to call node_generate service: {}", e)))?;

    if !response.success {
        let msg = if response.error_message.trim().is_empty() {
            "node_generate failed with no error message".to_string()
        } else {
            response.error_message
        };
        return Err(Error::ExecutionFailed(msg));
    }

    info!("Synced node interfaces at {}", node_root_dir.display());

    if include_repositories {
        if !response.resolved_from_stack.is_empty() {
            info!("Synchronized from node stack:");
            for dep in &response.resolved_from_stack {
                info!("  - {}", dep);
            }
        }
        if !response.resolved_from_repositories.is_empty() {
            info!("Synchronized from repositories:");
            for entry in &response.resolved_from_repositories {
                let label = match entry.source_kind {
                    RepoSourceKind::Fs => "fs",
                    RepoSourceKind::Git => "git",
                    RepoSourceKind::Url => "http",
                };
                info!("  - {}:{} ({})", entry.name, entry.tag, label);
            }
        }
    }

    Ok(())
}
