//! Archive extraction primitives shared by node-stack and core-node.
//!
//! Lives at the crate root (rather than under `node_stack::start_steps`)
//! because the same `.tar.zst` format is used by two unrelated callers:
//! the start lifecycle's process-node archive extraction, and core-node's
//! node-add source resolution. Keeping this here avoids the start pipeline
//! "owning" a helper that has nothing to do with the start lifecycle.

use std::path::{Component, Path};
use tar::Archive;
use zstd::stream::read::Decoder;

/// Extracts a `.tar.zst` archive into `destination` with path safety checks.
/// Rejects entries containing `..`, root, or prefix path components.
/// Directories are applied last to avoid permission interference during extraction.
pub fn extract_tar_zst(archive_path: &Path, destination: &Path) -> std::result::Result<(), String> {
    let file = std::fs::File::open(archive_path)
        .map_err(|e| format!("Failed to open archive {}: {}", archive_path.display(), e))?;

    let decoder = Decoder::new(file).map_err(|e| {
        format!(
            "Failed to decode zstd archive {}: {}",
            archive_path.display(),
            e
        )
    })?;
    let mut archive = Archive::new(decoder);

    let entries = archive.entries().map_err(|e| {
        format!(
            "Failed to read archive entries from {}: {}",
            archive_path.display(),
            e
        )
    })?;

    let mut directories = Vec::new();
    for entry in entries {
        let mut entry = entry.map_err(|e| {
            format!(
                "Failed to read archive entry from {}: {}",
                archive_path.display(),
                e
            )
        })?;

        let entry_path = entry
            .path()
            .map_err(|e| {
                format!(
                    "Failed to read entry path from {}: {}",
                    archive_path.display(),
                    e
                )
            })?
            .into_owned();

        if entry_path.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(..)
            )
        }) {
            return Err(format!(
                "Archive {} contains unsafe path: {}",
                archive_path.display(),
                entry_path.display()
            ));
        }

        if entry.header().entry_type().is_dir() {
            directories.push(entry);
        } else {
            let unpacked = entry.unpack_in(destination).map_err(|e| {
                format!(
                    "Failed to unpack entry {} from {}: {}",
                    entry_path.display(),
                    archive_path.display(),
                    e
                )
            })?;
            if !unpacked {
                return Err(format!(
                    "Archive {} contains unsafe path: {}",
                    archive_path.display(),
                    entry_path.display()
                ));
            }
        }
    }

    // Apply directory entries at the end, matching tar::Archive::unpack behavior (avoids
    // directory permissions interfering with descendant extraction).
    directories.sort_by(|a, b| b.path_bytes().cmp(&a.path_bytes()));
    for mut dir in directories {
        let entry_path = dir
            .path()
            .map_err(|e| {
                format!(
                    "Failed to read entry path from {}: {}",
                    archive_path.display(),
                    e
                )
            })?
            .into_owned();
        let unpacked = dir.unpack_in(destination).map_err(|e| {
            format!(
                "Failed to unpack entry {} from {}: {}",
                entry_path.display(),
                archive_path.display(),
                e
            )
        })?;
        if !unpacked {
            return Err(format!(
                "Archive {} contains unsafe path: {}",
                archive_path.display(),
                entry_path.display()
            ));
        }
    }

    Ok(())
}
