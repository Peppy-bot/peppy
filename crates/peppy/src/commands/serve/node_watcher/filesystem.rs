use super::types::NodeDetectionEvent;
use crate::Result;
use crate::consts::PEPPY_CONFIG_FILE;
use notify;
use std::path::{Path, PathBuf};
use tokio::sync::mpsc;

/// Finds the `PEPPY_CONFIG_FILE` recursively starting at `from_dir`
pub fn find_peppy_nodes_from_dir(from_dir: impl AsRef<Path>) -> Vec<PathBuf> {
    let mut peppy_files = Vec::new();
    let from_dir = from_dir.as_ref();

    if !from_dir.is_dir() {
        return peppy_files;
    }

    let walker = walkdir::WalkDir::new(from_dir)
        .follow_links(false)
        .into_iter()
        .filter_map(|entry| entry.ok());

    for entry in walker {
        let path = entry.path();
        if path.is_file() && path.file_name() == Some(std::ffi::OsStr::new(PEPPY_CONFIG_FILE)) {
            peppy_files.push(path.to_path_buf());
        }
    }

    peppy_files
}

// Async function to watch file changes using notify and send into channel
pub async fn watch_files(
    tx: mpsc::Sender<NodeDetectionEvent>,
    from_dir: impl AsRef<Path>,
) -> Result<()> {
    Ok(())
}
