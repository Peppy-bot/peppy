use super::types::NodeDetectionEvent;
use crate::consts::PEPPY_CONFIG_FILE;
use crate::{Error, Result};
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
    use super::types::FileEvent;
    use notify::{RecommendedWatcher, RecursiveMode, Watcher};

    let from_dir = from_dir.as_ref().to_path_buf();

    let (notify_tx, mut notify_rx) = mpsc::channel(100);

    let mut watcher: RecommendedWatcher =
        notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
            if let Ok(event) = res {
                let _ = notify_tx.blocking_send(event);
            }
        })
        .map_err(|e| Error::NodeWatcher(format!("Failed to create file watcher: {}", e)))?;

    watcher
        .watch(&from_dir, RecursiveMode::Recursive)
        .map_err(|e| Error::NodeWatcher(format!("Failed to watch directory: {}", e)))?;

    while let Some(event) = notify_rx.recv().await {
        use notify::EventKind;

        for path in &event.paths {
            if path.file_name() != Some(std::ffi::OsStr::new(PEPPY_CONFIG_FILE)) {
                continue;
            }

            let detection_event = match event.kind {
                EventKind::Create(_) => Some(NodeDetectionEvent::FileEvent(
                    FileEvent::NodeConfigCreated(path.clone()),
                )),
                EventKind::Modify(_) => Some(NodeDetectionEvent::FileEvent(
                    FileEvent::NodeConfigModified(path.clone()),
                )),
                EventKind::Remove(_) => Some(NodeDetectionEvent::FileEvent(
                    FileEvent::NodeConfigDeleted(path.clone()),
                )),
                _ => None,
            };

            if let Some(event) = detection_event {
                tx.send(event).await.map_err(|e| {
                    Error::NodeWatcher(format!("Failed to send node detection event: {}", e))
                })?;
            }
        }
    }

    Ok(())
}
