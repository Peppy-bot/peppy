use super::discovery::find_peppy_nodes_from_dir;
use crate::consts::PEPPY_NODE_CONFIG_FILE;
use crate::error::{Error, Result};
use notify::event::{AccessKind, AccessMode, ModifyKind, RenameMode};
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use super::events::NodeConfigEvent;

// Async function to watch file changes using notify and send into channel
pub async fn watch_files(
    tx: mpsc::Sender<NodeConfigEvent>,
    from_dir: impl AsRef<Path>,
) -> Result<JoinHandle<Result<()>>> {
    let from_dir_input = from_dir.as_ref().to_path_buf();
    let from_dir_canon = std::fs::canonicalize(&from_dir_input)?;
    let from_dir_user_abs = if from_dir_input.is_absolute() {
        from_dir_input.clone()
    } else {
        std::env::current_dir()?.join(&from_dir_input)
    };

    // Track existing configs at startup to suppress spurious Create events
    let mut known_configs: HashSet<PathBuf> = find_peppy_nodes_from_dir(&from_dir_canon)
        .into_iter()
        .map(|p| {
            // Store known configs using the user path prefix to keep consistency with emitted events
            match p.strip_prefix(&from_dir_canon) {
                Ok(rel) => from_dir_user_abs.join(rel),
                Err(_) => p,
            }
        })
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
        .watch(&from_dir_canon, RecursiveMode::Recursive)
        .map_err(|e| Error::NodeWatcher(format!("Failed to watch directory: {}", e)))?;

    // Spawn the processing loop and return immediately with a handle.
    let handle: JoinHandle<Result<()>> = tokio::spawn(async move {
        // Keep watcher alive within this task's scope
        let mut _watcher = watcher;

        while let Some(event) = notify_rx.recv().await {
            for raw_path in &event.paths {
                // Rebase the path from the canonical root to the user root when possible
                let path = match raw_path.strip_prefix(&from_dir_canon) {
                    Ok(rel) => from_dir_user_abs.join(rel),
                    Err(_) => raw_path.clone(),
                };

                if path.file_name() != Some(std::ffi::OsStr::new(PEPPY_NODE_CONFIG_FILE)) {
                    continue;
                }

                // Normalize platform-specific variants into our 3 high-level events
                let detection_event = match event.kind {
                    // File created
                    notify::EventKind::Create(_) => {
                        // Ignore create notifications for files that already existed before watching
                        if known_configs.contains(&path) {
                            None
                        } else {
                            known_configs.insert(path.clone());
                            Some(NodeConfigEvent::Created(path.clone()))
                        }
                    }
                    // Rename events: treat rename-from as deleted and rename-to as created
                    notify::EventKind::Modify(ModifyKind::Name(RenameMode::From)) => {
                        known_configs.remove(&path);
                        Some(NodeConfigEvent::Deleted(path.clone()))
                    }
                    notify::EventKind::Modify(ModifyKind::Name(RenameMode::To)) => {
                        if !known_configs.contains(&path) {
                            known_configs.insert(path.clone());
                            Some(NodeConfigEvent::Created(path.clone()))
                        } else {
                            None
                        }
                    }
                    // Any other modification: if the file no longer exists, surface as Deleted
                    notify::EventKind::Modify(_) => {
                        let exists = path.exists();
                        if exists {
                            Some(NodeConfigEvent::Modified(path.clone()))
                        } else {
                            known_configs.remove(&path);
                            Some(NodeConfigEvent::Deleted(path.clone()))
                        }
                    }
                    // Some platforms (e.g. macOS) emit a close-write access instead of a modify
                    notify::EventKind::Access(AccessKind::Close(AccessMode::Write)) => {
                        let exists = path.exists();
                        if exists {
                            Some(NodeConfigEvent::Modified(path.clone()))
                        } else {
                            known_configs.remove(&path);
                            Some(NodeConfigEvent::Deleted(path.clone()))
                        }
                    }
                    // File removed
                    notify::EventKind::Remove(_) => {
                        known_configs.remove(&path);
                        Some(NodeConfigEvent::Deleted(path.clone()))
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

#[cfg(test)]
mod tests {
    use super::super::events::NodeConfigEvent;
    use super::*;
    use crate::consts::PEPPY_NODE_CONFIG_FILE;
    use std::fs;
    use std::time::Duration;
    use tempfile::TempDir;
    use tokio::sync::mpsc;
    use tokio::time::timeout;

    #[tokio::test]
    async fn test_watch_files_detects_created_config() {
        let temp_dir = TempDir::new().unwrap();
        let (tx, mut rx) = mpsc::channel(10);
        let watch_dir = temp_dir.path().to_path_buf();

        // Initialize watcher (await until ready) and get background handle
        let watch_handle = watch_files(tx, watch_dir).await.expect("watcher init");

        // Create a peppy config file
        let peppy_file = temp_dir.path().join(PEPPY_NODE_CONFIG_FILE);
        fs::write(&peppy_file, "node: test").unwrap();

        // Wait for the event with timeout
        let event = timeout(Duration::from_secs(1), rx.recv()).await;

        assert!(event.is_ok(), "Timeout waiting for file event");
        let event = event.unwrap();
        assert!(event.is_some());

        if let Some(NodeConfigEvent::Created(path)) = event {
            assert_eq!(path, peppy_file);
        } else {
            panic!("Expected NodeConfigCreated event, got: {:?}", event);
        }

        // Clean up
        watch_handle.abort();
    }

    #[tokio::test]
    async fn test_watch_files_detects_modified_config() {
        let temp_dir = TempDir::new().unwrap();
        let peppy_file = temp_dir.path().join(PEPPY_NODE_CONFIG_FILE);

        // Create file before starting watcher
        fs::write(&peppy_file, "node: test").unwrap();

        let (tx, mut rx) = mpsc::channel(10);
        let watch_dir = temp_dir.path().to_path_buf();

        // Initialize watcher and get background handle
        let watch_handle = watch_files(tx, watch_dir).await.expect("watcher init");

        // Modify the file
        fs::write(&peppy_file, "node: modified").unwrap();

        // Wait for the event
        let event = timeout(Duration::from_secs(1), rx.recv()).await;

        assert!(event.is_ok(), "Timeout waiting for file event");
        let event = event.unwrap();
        assert!(event.is_some());

        if let Some(NodeConfigEvent::Modified(path)) = event {
            assert_eq!(path, peppy_file);
        } else {
            panic!("Expected NodeConfigModified event, got: {:?}", event);
        }

        watch_handle.abort();
    }

    #[tokio::test]
    async fn test_watch_files_detects_deleted_config() {
        let temp_dir = TempDir::new().unwrap();
        let peppy_file = temp_dir.path().join(PEPPY_NODE_CONFIG_FILE);

        // Create file before starting watcher
        fs::write(&peppy_file, "node: test").unwrap();

        let (tx, mut rx) = mpsc::channel(10);
        let watch_dir = temp_dir.path().to_path_buf();

        // Initialize watcher and get background handle
        let watch_handle = watch_files(tx, watch_dir).await.expect("watcher init");

        // Delete the file
        fs::remove_file(&peppy_file).unwrap();

        // Wait for the event
        let event = timeout(Duration::from_secs(1), rx.recv()).await;

        assert!(event.is_ok(), "Timeout waiting for file event");
        let event = event.unwrap();
        assert!(event.is_some());

        if let Some(NodeConfigEvent::Deleted(path)) = event {
            assert_eq!(path, peppy_file);
        } else {
            panic!("Expected NodeConfigDeleted event, got: {:?}", event);
        }

        watch_handle.abort();
    }

    #[tokio::test]
    async fn test_watch_files_ignores_non_peppy_files() {
        let temp_dir = TempDir::new().unwrap();
        let (tx, mut rx) = mpsc::channel(10);
        let watch_dir = temp_dir.path().to_path_buf();

        // Initialize watcher and get background handle
        let watch_handle = watch_files(tx, watch_dir).await.expect("watcher init");

        // Create non-peppy files
        fs::write(temp_dir.path().join("other.yaml"), "some: content").unwrap();
        fs::write(temp_dir.path().join("config.toml"), "config = true").unwrap();
        fs::write(temp_dir.path().join("README.md"), "# README").unwrap();

        // Should not receive any events for these files
        let event = timeout(Duration::from_millis(500), rx.recv()).await;
        assert!(
            event.is_err(),
            "Should not receive events for non-peppy files"
        );

        watch_handle.abort();
    }

    #[tokio::test]
    async fn test_watch_files_detects_nested_config_changes() {
        let temp_dir = TempDir::new().unwrap();
        let nested_dir = temp_dir.path().join("nested");
        fs::create_dir(&nested_dir).unwrap();

        let (tx, mut rx) = mpsc::channel(10);
        let watch_dir = temp_dir.path().to_path_buf();

        // Initialize watcher and get background handle
        let watch_handle = watch_files(tx, watch_dir).await.expect("watcher init");

        // Create a peppy config in nested directory
        let nested_peppy = nested_dir.join(PEPPY_NODE_CONFIG_FILE);
        fs::write(&nested_peppy, "node: nested").unwrap();

        // Wait for the event
        let event = timeout(Duration::from_secs(1), rx.recv()).await;

        assert!(event.is_ok(), "Timeout waiting for nested file event");
        let event = event.unwrap();
        assert!(event.is_some());

        if let Some(NodeConfigEvent::Created(path)) = event {
            assert_eq!(path, nested_peppy);
        } else {
            panic!(
                "Expected NodeConfigCreated event for nested file, got: {:?}",
                event
            );
        }

        watch_handle.abort();
    }

    #[tokio::test]
    async fn test_watch_files_handles_multiple_events() {
        let temp_dir = TempDir::new().unwrap();
        let (tx, mut rx) = mpsc::channel(10);
        let watch_dir = temp_dir.path().to_path_buf();

        // Initialize watcher and get background handle
        let watch_handle = watch_files(tx, watch_dir).await.expect("watcher init");

        // Create first peppy config file
        let peppy1 = temp_dir.path().join(PEPPY_NODE_CONFIG_FILE);
        fs::write(&peppy1, "node: one").unwrap();

        // Wait for first event
        let event1 = timeout(Duration::from_secs(1), rx.recv()).await;
        assert!(event1.is_ok(), "Should receive first event");

        // Verify the first event is a create event
        if let Ok(Some(NodeConfigEvent::Created(path))) = event1 {
            assert_eq!(path, peppy1, "First event should be for peppy1");
        } else {
            panic!("Expected NodeConfigCreated event for peppy1");
        }

        // Modify the file to trigger another event
        fs::write(&peppy1, "node: modified").unwrap();

        // Wait for modify event
        let event2 = timeout(Duration::from_secs(1), rx.recv()).await;
        assert!(event2.is_ok(), "Should receive second event");

        // Verify the second event is a modify event
        if let Ok(Some(NodeConfigEvent::Modified(path))) = event2 {
            assert_eq!(
                path, peppy1,
                "Second event should be for peppy1 modification"
            );
        } else {
            panic!("Expected NodeConfigModified event for peppy1");
        }

        while let Ok(Some(NodeConfigEvent::Modified(_))) =
            timeout(Duration::from_millis(10), rx.recv()).await
        {
            // Drain extra modify events
        }

        // Delete the file to trigger a delete event
        fs::remove_file(&peppy1).unwrap();

        // Wait for delete event
        let event3 = timeout(Duration::from_secs(1), rx.recv()).await;
        assert!(event3.is_ok(), "Should receive third event");

        // Verify the third event is a delete event
        if let Ok(Some(NodeConfigEvent::Deleted(path))) = event3 {
            assert_eq!(path, peppy1, "Third event should be for peppy1 deletion");
        } else {
            panic!(
                "Expected NodeConfigDeleted event for peppy1, got: {:?}",
                event3
            );
        }

        watch_handle.abort();
    }
}
