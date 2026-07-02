//! Materialize a `NodeCacheEntry` from `~/.peppy/cache/nodes.json5`
//! into an on-disk `(root_dir, parsed config)` pair.
//!
//! Filesystem entries are resolved directly. Git and HTTP entries route
//! through the persistent `git` / `bundle` caches under
//! [`daemon_config::consts::PeppyDirs`], so the same repo or archive is fetched
//! at most once per nodes-cache generation.
//!
//! Shared by `add_batch` (`run_repo_node_add`) and `sync`
//! (`materialize_repo_deps`). Feedback flows through a callback so each
//! caller can fan it into its own channel / log sink.

use super::super::sanitize_repo_path;
use super::{ensure_bundle, ensure_checkout};
use crate::services::repo::cache::NodeCacheEntry;
use config::consts::NODE_CONFIG_FILE;
use config::node::{NodeConfig, NodeConfigParser};
use core_node_api::encoding::RepoSourceKind;
use daemon_config::consts::PeppyDirs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;
use url::Url;

/// Type alias for a feedback callback that's safe to share across the
/// blocking thread `ensure_checkout` runs on.
pub(crate) type MaterializeFeedback = Arc<dyn Fn(&str) + Send + Sync + 'static>;

/// Returns a no-op feedback sink for callers that don't need progress
/// streaming (e.g. `node sync`, where progress is reported as response
/// provenance instead).
pub(crate) fn silent_feedback() -> MaterializeFeedback {
    Arc::new(|_| {})
}

/// Materialize one nodes-cache entry to a `(root_dir, parsed config)`
/// pair, reusing the persistent git / HTTP caches for non-FS entries.
///
/// `cache_generation` is the `mtime` of the `nodes.json5` snapshot
/// the entry was resolved from. The git cache uses it to dedup
/// fetch/refresh work across calls within the same snapshot.
///
/// `on_feedback` receives human-readable progress lines from the
/// underlying clone / download. Pass [`silent_feedback`] when no
/// streaming is wanted.
pub(crate) async fn materialize_entry(
    entry: &NodeCacheEntry,
    peppy_dirs: &PeppyDirs,
    cache_generation: Option<SystemTime>,
    on_feedback: MaterializeFeedback,
) -> Result<(PathBuf, NodeConfig), String> {
    let root_dir = match entry.source_type {
        RepoSourceKind::Fs => parent_dir_of(&entry.path).map_err(|e| {
            format!(
                "Fs cache entry for {}:{} has no parent directory in path {:?}: {}",
                entry.node_name, entry.node_tag, entry.path, e
            )
        })?,
        RepoSourceKind::Git => {
            let url = entry
                .source_uri
                .as_deref()
                .ok_or_else(|| "Git cache entry missing source_uri".to_owned())?;
            let reference = entry.resolved_ref.as_deref();
            let peppy_dirs = peppy_dirs.clone();
            let url_owned = url.to_owned();
            let ref_owned = reference.map(|s| s.to_owned());
            let fb = Arc::clone(&on_feedback);
            let checkout = tokio::task::spawn_blocking(move || {
                ensure_checkout(
                    &peppy_dirs,
                    &url_owned,
                    ref_owned.as_deref(),
                    cache_generation,
                    &|line| fb(line),
                )
            })
            .await
            .map_err(|e| format!("git cache task failed: {}", e))??;
            // `entry.path` is the repo-relative path of `peppy.json5`;
            // the materialized root directory is its parent.
            let manifest_relative = sanitize_repo_path(&entry.path).map_err(|e| {
                format!(
                    "Git cache entry for {}:{} has unsafe path {:?}: {}",
                    entry.node_name, entry.node_tag, entry.path, e
                )
            })?;
            let dir_relative = manifest_relative.parent().ok_or_else(|| {
                format!(
                    "Git cache entry for {}:{} has no parent directory in path {:?}",
                    entry.node_name, entry.node_tag, entry.path
                )
            })?;
            checkout.join(dir_relative)
        }
        RepoSourceKind::Url => {
            let url_str = entry
                .source_uri
                .as_deref()
                .ok_or_else(|| "Http cache entry missing source_uri".to_owned())?;
            let url = Url::parse(url_str)
                .map_err(|e| format!("Http cache entry has invalid URL '{url_str}': {e}"))?;
            ensure_bundle(peppy_dirs, &url, entry.checksum.clone(), &|line| {
                on_feedback(line)
            })
            .await?
        }
    };

    let config_path = root_dir.join(NODE_CONFIG_FILE);
    let parsed = NodeConfigParser::from_path(&config_path).map_err(|e| {
        format!(
            "Failed to parse node config at {}: {}",
            config_path.display(),
            e
        )
    })?;
    Ok((root_dir, parsed))
}

/// Resolve the parent directory of `entry.path`. The cache stores the
/// `peppy.json5` file path; consumers that need the source root take
/// its parent.
fn parent_dir_of(path: &str) -> Result<PathBuf, String> {
    Path::new(path)
        .parent()
        .map(|p| p.to_path_buf())
        .ok_or_else(|| "path has no parent".to_owned())
}
