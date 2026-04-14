//! Persistent HTTP bundle cache shared across `node add` batches.
//!
//! Mirrors [`super::git_cache`] but keyed by `(url, sha256)` — a given
//! URL + checksum can only ever refer to one archive, so once we've
//! downloaded and extracted it we reuse the extraction indefinitely.
//!
//! Each cache entry is a directory `<slug>-<hash>/` under
//! [`PeppyDirs::http_bundles_dir`]. Its contents are the extracted node
//! root (what [`locate_node_root_dir`] returns); a sibling `.sha256`
//! file records the recorded checksum so stale entries can be detected
//! when the checksum changes for the same URL.

use super::add::download_and_extract_http_source;
use super::cache_key;
use super::locate_node_root_dir;
use config::consts::PeppyDirs;
use parking_lot::Mutex;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use url::Url;

/// Returns the deterministic cache directory for `(url, sha256)`.
pub fn extract_dir_for(peppy_dirs: &PeppyDirs, url: &Url, sha256: Option<&str>) -> PathBuf {
    let slug = cache_key::slug(url.as_str(), "bundle");
    let hash = cache_key::short_hash(url.as_str(), sha256);
    peppy_dirs.http_bundles_dir().join(format!("{slug}-{hash}"))
}

fn locks_map() -> &'static Mutex<HashMap<String, Arc<Mutex<()>>>> {
    static MAP: OnceLock<Mutex<HashMap<String, Arc<Mutex<()>>>>> = OnceLock::new();
    MAP.get_or_init(|| Mutex::new(HashMap::new()))
}

fn lock_for(key: &str) -> Arc<Mutex<()>> {
    let mut map = locks_map().lock();
    // GC entries not currently held by any caller. `strong_count == 1`
    // means only the map still references the Arc, so no one can race on
    // rebuilding the slot (the map lock serializes all access).
    map.retain(|_, v| Arc::strong_count(v) > 1);
    map.entry(key.to_owned())
        .or_insert_with(|| Arc::new(Mutex::new(())))
        .clone()
}

fn marker_path(dir: &Path) -> PathBuf {
    dir.with_extension("sha256")
}

fn recorded_sha(dir: &Path) -> Option<String> {
    std::fs::read_to_string(marker_path(dir))
        .ok()
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
}

/// Ensures an extracted copy of the bundle at `url` exists on disk,
/// returning the *node root* directory (ready to feed to `process_node_add`).
///
/// When a cached entry already matches the recorded sha256 (when one is
/// supplied), the download is skipped. Entries whose recorded sha256
/// differs from the one on record are treated as stale and wiped.
pub async fn ensure_bundle(
    peppy_dirs: &PeppyDirs,
    url: &Url,
    sha256: Option<String>,
    on_feedback: &(dyn Fn(&str) + Send + Sync),
) -> std::result::Result<PathBuf, String> {
    let target = extract_dir_for(peppy_dirs, url, sha256.as_deref());
    let lock_key = target.to_string_lossy().into_owned();

    // Serialize concurrent ensure_bundle calls for the same key.
    let blocking_check = {
        let lock = lock_for(&lock_key);
        let target_clone = target.clone();
        let sha256_clone = sha256.clone();
        tokio::task::spawn_blocking(move || {
            let _guard = lock.lock();
            cached_node_root(&target_clone, sha256_clone.as_deref())
        })
        .await
        .map_err(|e| format!("Bundle cache join error: {}", e))?
    };

    if let Some(root) = blocking_check {
        on_feedback(&format!("Reusing cached bundle at {}", root.display()));
        return Ok(root);
    }

    // Cache miss — download into a temp dir, then atomically rename.
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent).map_err(|e| {
            format!(
                "Failed to create http_bundles parent {}: {}",
                parent.display(),
                e
            )
        })?;
    }
    let extracted =
        download_and_extract_http_source(url, peppy_dirs.clone(), sha256.clone()).await?;

    let lock = lock_for(&lock_key);
    let target_final = target.clone();
    let sha_final = sha256.clone();
    tokio::task::spawn_blocking(move || {
        let _guard = lock.lock();
        let _ = std::fs::remove_dir_all(&target_final);
        if let Some(parent) = target_final.parent() {
            std::fs::create_dir_all(parent).map_err(|e| {
                format!(
                    "Failed to create http_bundles parent {}: {}",
                    parent.display(),
                    e
                )
            })?;
        }
        std::fs::rename(&extracted.source_path, &target_final).map_err(|e| {
            format!(
                "Failed to promote extracted bundle into cache at {}: {}",
                target_final.display(),
                e
            )
        })?;
        if let Some(op_dir) = extracted.cleanup_dir.as_ref() {
            let _ = std::fs::remove_dir_all(op_dir);
        }
        if let Some(sha) = sha_final.as_deref() {
            let marker = marker_path(&target_final);
            std::fs::write(&marker, sha).map_err(|e| {
                format!(
                    "Failed to write bundle sha marker {}: {}",
                    marker.display(),
                    e
                )
            })?;
        }
        locate_node_root_dir(&target_final)
    })
    .await
    .map_err(|e| format!("Bundle cache join error: {}", e))?
}

fn cached_node_root(target: &Path, sha256: Option<&str>) -> Option<PathBuf> {
    if !target.exists() {
        return None;
    }
    if let Some(expected) = sha256 {
        let recorded = recorded_sha(target).unwrap_or_default();
        if recorded != expected {
            let _ = std::fs::remove_dir_all(target);
            let _ = std::fs::remove_file(marker_path(target));
            return None;
        }
    }
    locate_node_root_dir(target).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_dir_for_distinct_sha_distinct_dirs() {
        let tmp = tempfile::tempdir().unwrap();
        let peppy_dirs = PeppyDirs::new(tmp.path());
        let url = Url::parse("https://example.com/node.tar.zst").unwrap();
        let a = extract_dir_for(&peppy_dirs, &url, Some("aa"));
        let b = extract_dir_for(&peppy_dirs, &url, Some("bb"));
        assert_ne!(a, b);
    }

    #[test]
    fn extract_dir_for_same_url_and_sha_same_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let peppy_dirs = PeppyDirs::new(tmp.path());
        let url = Url::parse("https://example.com/node.tar.zst").unwrap();
        let a = extract_dir_for(&peppy_dirs, &url, Some("aa"));
        let b = extract_dir_for(&peppy_dirs, &url, Some("aa"));
        assert_eq!(a, b);
    }
}
