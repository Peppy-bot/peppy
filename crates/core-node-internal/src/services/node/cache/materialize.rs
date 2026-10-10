//! Materialize a `NodeCacheEntry` from `~/.peppy/cache/nodes.json5`
//! into an on-disk `(root_dir, parsed config)` pair.
//!
//! Filesystem entries are read where they lie. Git entries route through
//! the persistent commit-keyed checkout cache under
//! [`daemon_config::consts::PeppyDirs`], so one commit of one repository is
//! fetched at most once per machine.
//!
//! `materialize_entry` is used by `sync` (`materialize_repo_deps`), which
//! reports provenance in its response rather than streaming progress.
//! [`MaterializeFeedback`] is the thread-safe progress callback that the
//! pinned add, the built-in MCP servers and the launch resolution give to
//! their blocking checkouts.

use crate::services::node::Report;
use crate::services::node::common::parse_node_config_file;
use crate::services::repo::cache::{NodeCacheEntry, resolve_cached_artifact_path};
use config::node::NodeConfig;
use daemon_config::consts::PeppyDirs;
use std::path::PathBuf;
use std::sync::Arc;

/// Type alias for a feedback callback that's safe to share across the
/// blocking thread the checkout runs on.
pub(crate) type MaterializeFeedback = Arc<dyn Fn(Report<'_>) + Send + Sync + 'static>;

/// Materialize one nodes-cache entry to a `(root_dir, parsed config)`
/// pair, reusing the persistent checkout cache for git entries.
pub(crate) async fn materialize_entry(
    entry: &NodeCacheEntry,
    peppy_dirs: &PeppyDirs,
) -> Result<(PathBuf, NodeConfig), String> {
    let id = format!("{}:{}", entry.node_name, entry.node_tag);
    // Only an origin that may reach the network is worth a blocking-pool
    // round trip and the two clones it forces; a filesystem entry already
    // names a file on this machine, and this runs once per node in a
    // batch's transitive closure.
    let manifest_path = if entry.origin.resolution_may_block() {
        let dirs = peppy_dirs.clone();
        let origin = entry.origin.clone();
        tokio::task::spawn_blocking(move || resolve_cached_artifact_path(&dirs, &origin, &|_| {}))
            .await
            .map_err(|e| format!("materialization task for `{id}` failed: {e}"))?
    } else {
        resolve_cached_artifact_path(peppy_dirs, &entry.origin, &|_| {})
    }
    .map_err(|e| format!("node `{id}`: {e}"))?;

    // The cache records the file that declares the node; a node is built
    // from the directory holding it.
    let root_dir = manifest_path
        .parent()
        .ok_or_else(|| {
            format!(
                "node `{id}` resolved to {}, which has no parent directory",
                manifest_path.display()
            )
        })?
        .to_path_buf();

    let parsed = parse_node_config_file(&manifest_path)?;
    Ok((root_dir, parsed))
}
