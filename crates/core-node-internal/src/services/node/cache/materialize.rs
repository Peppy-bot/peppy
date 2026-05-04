//! Materialize a `PackageEntry` from `~/.peppy/cache/packages.json5`
//! into an on-disk `(root_dir, parsed config)` pair.
//!
//! Filesystem entries are resolved directly. Git and HTTP entries route
//! through the persistent `git` / `bundle` caches under
//! [`config::consts::PeppyDirs`], so the same repo or archive is fetched
//! at most once per packages-cache generation.
//!
//! Shared by `add_batch` (`run_repo_node_add`) and `sync`
//! (`materialize_repo_deps`). Feedback flows through a callback so each
//! caller can fan it into its own channel / log sink.

use super::super::sanitize_repo_path;
use super::{ensure_bundle, ensure_checkout};
use crate::services::repo::cache::PackageEntry;
use config::consts::{NODE_CONFIG_FILE, PeppyDirs};
use config::node::{NodeConfigParser, ParsedNodeConfig};
use core_node_api::encoding::RepoSourceKind;
use std::path::PathBuf;
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

/// Materialize one packages-cache entry to a `(root_dir, parsed config)`
/// pair, reusing the persistent git / HTTP caches for non-FS entries.
///
/// `cache_generation` is the `mtime` of the `packages.json5` snapshot
/// the entry was resolved from. The git cache uses it to dedup
/// fetch/refresh work across calls within the same snapshot.
///
/// `on_feedback` receives human-readable progress lines from the
/// underlying clone / download. Pass [`silent_feedback`] when no
/// streaming is wanted.
pub(crate) async fn materialize_entry(
    entry: &PackageEntry,
    peppy_dirs: &PeppyDirs,
    cache_generation: Option<SystemTime>,
    on_feedback: MaterializeFeedback,
) -> Result<(PathBuf, ParsedNodeConfig), String> {
    let root_dir = match entry.source_type {
        RepoSourceKind::Fs => PathBuf::from(&entry.path),
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
            let repo_relative_path = sanitize_repo_path(&entry.path).map_err(|e| {
                format!(
                    "Git cache entry for {}:{} has unsafe path {:?}: {}",
                    entry.node_name, entry.node_tag, entry.path, e
                )
            })?;
            checkout.join(repo_relative_path)
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
