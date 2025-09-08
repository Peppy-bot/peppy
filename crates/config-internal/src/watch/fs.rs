use super::discovery::find_peppy_nodes_from_dir;
use crate::consts::PEPPY_CONFIG_FILE;
use crate::error::{Error, Result};
use notify;
use notify::event::{AccessKind, AccessMode, ModifyKind, RenameMode};
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use super::events::FileEvent;

// Async function to watch file changes using notify and send into channel
pub async fn watch_files(
    tx: mpsc::Sender<FileEvent>,
    from_dir: impl AsRef<Path>,
) -> Result<JoinHandle<Result<()>>> {
    let from_dir_input = from_dir.as_ref().to_path_buf();
    let from_dir_abs = if from_dir_input.is_absolute() {
        from_dir_input.clone()
    } else {
        match std::env::current_dir() {
            Ok(cwd) => cwd.join(&from_dir_input),
            Err(_) => from_dir_input.clone(),
        }
    };
    let from_dir_canon = std::fs::canonicalize(&from_dir_abs).unwrap_or(from_dir_abs.clone());

    // Track existing configs at startup to suppress spurious Create events
    let mut known_configs: HashSet<PathBuf> = find_peppy_nodes_from_dir(&from_dir_abs)
        .into_iter()
        .collect();

    let (notify_tx, mut notify_rx) = mpsc::channel(100);

    let mut watcher: RecommendedWatcher =
        notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
            if let Ok(event) = res {
                let _ = notify_tx.blocking_send(event);
            }
        })
        .map_err(|e| Error::NodeWatcher(format!("Failed to create file watcher: {}", e)))?;

    watcher
        .watch(&from_dir_abs, RecursiveMode::Recursive)
        .map_err(|e| Error::NodeWatcher(format!("Failed to watch directory: {}", e)))?;

    // Spawn the processing loop and return immediately with a handle.
    let handle: JoinHandle<Result<()>> = tokio::spawn(async move {
        // Keep watcher alive within this task's scope
        let mut _watcher = watcher;

        while let Some(event) = notify_rx.recv().await {
            for path in &event.paths {
                if path.file_name() != Some(std::ffi::OsStr::new(PEPPY_CONFIG_FILE)) {
                    continue;
                }

                let path = normalize_event_path_to_base(path, &from_dir_abs, &from_dir_canon);

                // Normalize platform-specific variants into our 3 high-level events
                let detection_event = match event.kind {
                    // File created
                    notify::EventKind::Create(_) => {
                        // Ignore create notifications for files that already existed before watching
                        if known_configs.contains(&path) {
                            None
                        } else {
                            known_configs.insert(path.clone());
                            Some(FileEvent::NodeConfigCreated(path.clone()))
                        }
                    }
                    // Rename events: treat rename-from as deleted and rename-to as created
                    notify::EventKind::Modify(ModifyKind::Name(RenameMode::From)) => {
                        known_configs.remove(&path);
                        Some(FileEvent::NodeConfigDeleted(path.clone()))
                    }
                    notify::EventKind::Modify(ModifyKind::Name(RenameMode::To)) => {
                        if !known_configs.contains(&path) {
                            known_configs.insert(path.clone());
                            Some(FileEvent::NodeConfigCreated(path.clone()))
                        } else {
                            None
                        }
                    }
                    // Any other modification: if the file no longer exists, surface as Deleted
                    notify::EventKind::Modify(_) => {
                        let exists = path.exists();
                        if exists {
                            Some(FileEvent::NodeConfigModified(path.clone()))
                        } else {
                            known_configs.remove(&path);
                            Some(FileEvent::NodeConfigDeleted(path.clone()))
                        }
                    }
                    // Some platforms (e.g. macOS) emit a close-write access instead of a modify
                    notify::EventKind::Access(AccessKind::Close(AccessMode::Write)) => {
                        let exists = path.exists();
                        if exists {
                            Some(FileEvent::NodeConfigModified(path.clone()))
                        } else {
                            known_configs.remove(&path);
                            Some(FileEvent::NodeConfigDeleted(path.clone()))
                        }
                    }
                    // File removed
                    notify::EventKind::Remove(_) => {
                        known_configs.remove(&path);
                        Some(FileEvent::NodeConfigDeleted(path.clone()))
                    }
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
    });

    Ok(handle)
}

#[inline]
fn normalize_event_path_to_base(path: &Path, base: &Path, base_canon: &Path) -> PathBuf {
    // Try to canonicalize the file path. For deleted paths, fall back to canonicalizing the parent
    let canonicalized = match std::fs::canonicalize(path) {
        Ok(p) => p,
        Err(_) => match (path.parent(), path.file_name()) {
            (Some(parent), Some(name)) => match std::fs::canonicalize(parent) {
                Ok(parent_canon) => parent_canon.join(name),
                Err(_) => path.to_path_buf(),
            },
            _ => path.to_path_buf(),
        },
    };

    // If the canonicalized path is under the canonicalized base, remap it to the original base
    if let Ok(rel) = canonicalized.strip_prefix(base_canon) {
        return base.join(rel);
    }

    // Otherwise, return the original path unchanged
    path.to_path_buf()
}
