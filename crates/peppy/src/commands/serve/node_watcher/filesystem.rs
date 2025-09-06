use super::types::NodeDetectionEvent;
use crate::consts::PEPPY_CONFIG_FILE;
use crate::{Error, Result};
use notify;
use notify::event::{AccessKind, AccessMode, ModifyKind, RenameMode};
use std::collections::HashSet;
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

// Async function to watch file changes using notify and send into channel
pub async fn watch_files(
    tx: mpsc::Sender<NodeDetectionEvent>,
    from_dir: impl AsRef<Path>,
) -> Result<()> {
    use super::types::FileEvent;
    use notify::{RecommendedWatcher, RecursiveMode, Watcher};

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
                        Some(NodeDetectionEvent::FileEvent(FileEvent::NodeConfigCreated(
                            path.clone(),
                        )))
                    }
                }
                // Rename events: treat rename-from as deleted and rename-to as created
                notify::EventKind::Modify(ModifyKind::Name(RenameMode::From)) => {
                    known_configs.remove(&path);
                    Some(NodeDetectionEvent::FileEvent(FileEvent::NodeConfigDeleted(
                        path.clone(),
                    )))
                }
                notify::EventKind::Modify(ModifyKind::Name(RenameMode::To)) => {
                    if !known_configs.contains(&path) {
                        known_configs.insert(path.clone());
                        Some(NodeDetectionEvent::FileEvent(FileEvent::NodeConfigCreated(
                            path.clone(),
                        )))
                    } else {
                        None
                    }
                }
                // Any other modification: if the file no longer exists, surface as Deleted
                notify::EventKind::Modify(_) => {
                    let exists = path.exists();
                    if exists {
                        Some(NodeDetectionEvent::FileEvent(
                            FileEvent::NodeConfigModified(path.clone()),
                        ))
                    } else {
                        known_configs.remove(&path);
                        Some(NodeDetectionEvent::FileEvent(FileEvent::NodeConfigDeleted(
                            path.clone(),
                        )))
                    }
                }
                // Some platforms (e.g. macOS) emit a close-write access instead of a modify
                notify::EventKind::Access(AccessKind::Close(AccessMode::Write)) => {
                    let exists = path.exists();
                    if exists {
                        Some(NodeDetectionEvent::FileEvent(
                            FileEvent::NodeConfigModified(path.clone()),
                        ))
                    } else {
                        known_configs.remove(&path);
                        Some(NodeDetectionEvent::FileEvent(FileEvent::NodeConfigDeleted(
                            path.clone(),
                        )))
                    }
                }
                // File removed
                notify::EventKind::Remove(_) => {
                    known_configs.remove(&path);
                    Some(NodeDetectionEvent::FileEvent(FileEvent::NodeConfigDeleted(
                        path.clone(),
                    )))
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn test_find_peppy_nodes_empty_dir() {
        let temp_dir = TempDir::new().unwrap();
        let result = find_peppy_nodes_from_dir(temp_dir.path());
        assert_eq!(result.len(), 0);
    }

    #[test]
    fn test_find_peppy_nodes_non_existent_dir() {
        let result = find_peppy_nodes_from_dir("/path/that/does/not/exist");
        assert_eq!(result.len(), 0);
    }

    #[test]
    fn test_find_peppy_nodes_file_instead_of_dir() {
        let temp_dir = TempDir::new().unwrap();
        let file_path = temp_dir.path().join("test.txt");
        fs::write(&file_path, "test content").unwrap();

        let result = find_peppy_nodes_from_dir(&file_path);
        assert_eq!(result.len(), 0);
    }

    #[test]
    fn test_find_single_peppy_node() {
        let temp_dir = TempDir::new().unwrap();
        let peppy_file = temp_dir.path().join(PEPPY_CONFIG_FILE);
        fs::write(&peppy_file, "node_config: test").unwrap();

        let result = find_peppy_nodes_from_dir(temp_dir.path());
        assert_eq!(result.len(), 1);
        assert_eq!(result[0], peppy_file);
    }

    #[test]
    fn test_find_multiple_peppy_nodes_nested() {
        let temp_dir = TempDir::new().unwrap();

        // Create peppy.yaml in root
        let root_peppy = temp_dir.path().join(PEPPY_CONFIG_FILE);
        fs::write(&root_peppy, "node_config: root").unwrap();

        // Create nested directory with peppy.yaml
        let nested_dir = temp_dir.path().join("nested");
        fs::create_dir(&nested_dir).unwrap();
        let nested_peppy = nested_dir.join(PEPPY_CONFIG_FILE);
        fs::write(&nested_peppy, "node_config: nested").unwrap();

        // Create deeply nested directory with peppy.yaml
        let deep_dir = nested_dir.join("deep");
        fs::create_dir(&deep_dir).unwrap();
        let deep_peppy = deep_dir.join(PEPPY_CONFIG_FILE);
        fs::write(&deep_peppy, "node_config: deep").unwrap();

        let result = find_peppy_nodes_from_dir(temp_dir.path());
        assert_eq!(result.len(), 3);
        assert!(result.contains(&root_peppy));
        assert!(result.contains(&nested_peppy));
        assert!(result.contains(&deep_peppy));
    }

    #[test]
    fn test_find_peppy_nodes_ignores_other_files() {
        let temp_dir = TempDir::new().unwrap();

        // Create peppy.yaml
        let peppy_file = temp_dir.path().join(PEPPY_CONFIG_FILE);
        fs::write(&peppy_file, "node: test").unwrap();

        // Create other files that should be ignored
        fs::write(temp_dir.path().join("config.yaml"), "other: config").unwrap();
        fs::write(temp_dir.path().join("peppy.toml"), "wrong extension").unwrap();
        fs::write(temp_dir.path().join("not_peppy.yaml"), "not peppy").unwrap();

        let result = find_peppy_nodes_from_dir(temp_dir.path());
        assert_eq!(result.len(), 1);
        assert_eq!(result[0], peppy_file);
    }

    #[test]
    fn test_find_peppy_nodes_does_not_follow_symlinks() {
        let temp_dir = TempDir::new().unwrap();
        let external_dir = TempDir::new().unwrap();

        // Create peppy.yaml in external directory
        let external_peppy = external_dir.path().join(PEPPY_CONFIG_FILE);
        fs::write(&external_peppy, "node: external").unwrap();

        // Create symlink to external directory
        let symlink_path = temp_dir.path().join("link");
        #[cfg(unix)]
        std::os::unix::fs::symlink(external_dir.path(), &symlink_path).unwrap();
        #[cfg(windows)]
        std::os::windows::fs::symlink_dir(external_dir.path(), &symlink_path).unwrap();

        // Create peppy.yaml in main directory
        let main_peppy = temp_dir.path().join(PEPPY_CONFIG_FILE);
        fs::write(&main_peppy, "node: main").unwrap();

        let result = find_peppy_nodes_from_dir(temp_dir.path());
        // Should only find the main peppy.yaml, not the one through symlink
        assert_eq!(result.len(), 1);
        assert_eq!(result[0], main_peppy);
    }

    #[test]
    fn test_find_peppy_nodes_handles_permissions() {
        // This test is platform-specific and may need adjustment
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            let temp_dir = TempDir::new().unwrap();

            // Create accessible peppy.yaml
            let accessible_peppy = temp_dir.path().join(PEPPY_CONFIG_FILE);
            fs::write(&accessible_peppy, "node: accessible").unwrap();

            // Create directory with restricted permissions
            let restricted_dir = temp_dir.path().join("restricted");
            fs::create_dir(&restricted_dir).unwrap();
            let restricted_peppy = restricted_dir.join(PEPPY_CONFIG_FILE);
            fs::write(&restricted_peppy, "node: restricted").unwrap();

            // Remove read permissions from the directory
            let mut perms = fs::metadata(&restricted_dir).unwrap().permissions();
            perms.set_mode(0o000);
            fs::set_permissions(&restricted_dir, perms).unwrap();

            let result = find_peppy_nodes_from_dir(temp_dir.path());

            // Restore permissions for cleanup
            let mut perms = fs::metadata(&restricted_dir).unwrap().permissions();
            perms.set_mode(0o755);
            fs::set_permissions(&restricted_dir, perms).unwrap();

            // Should only find the accessible one
            assert!(result.len() == 1);
            assert!(result.contains(&accessible_peppy));
        }
    }

    #[tokio::test]
    async fn test_watch_files_detects_created_config() {
        use super::super::types::FileEvent;
        use std::time::Duration;
        use tokio::time::timeout;

        let temp_dir = TempDir::new().unwrap();
        let (tx, mut rx) = mpsc::channel(10);
        let watch_dir = temp_dir.path().to_path_buf();

        // Start watching in a background task
        let watch_handle = tokio::spawn(async move { watch_files(tx, watch_dir).await });

        // Give the watcher time to initialize
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Create a peppy config file
        let peppy_file = temp_dir.path().join(PEPPY_CONFIG_FILE);
        fs::write(&peppy_file, "node: test").unwrap();

        // Wait for the event with timeout
        let event = timeout(Duration::from_secs(2), rx.recv()).await;

        assert!(event.is_ok(), "Timeout waiting for file event");
        let event = event.unwrap();
        assert!(event.is_some());

        if let Some(NodeDetectionEvent::FileEvent(FileEvent::NodeConfigCreated(path))) = event {
            assert_eq!(path, peppy_file);
        } else {
            panic!("Expected NodeConfigCreated event, got: {:?}", event);
        }

        // Clean up
        watch_handle.abort();
    }

    #[tokio::test]
    async fn test_watch_files_detects_modified_config() {
        use super::super::types::FileEvent;
        use std::time::Duration;
        use tokio::time::timeout;

        let temp_dir = TempDir::new().unwrap();
        let peppy_file = temp_dir.path().join(PEPPY_CONFIG_FILE);

        // Create file before starting watcher
        fs::write(&peppy_file, "node: test").unwrap();

        let (tx, mut rx) = mpsc::channel(10);
        let watch_dir = temp_dir.path().to_path_buf();

        // Start watching
        let watch_handle = tokio::spawn(async move { watch_files(tx, watch_dir).await });

        // Give the watcher time to initialize
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Modify the file
        fs::write(&peppy_file, "node: modified").unwrap();

        // Wait for the event
        let event = timeout(Duration::from_secs(2), rx.recv()).await;

        assert!(event.is_ok(), "Timeout waiting for file event");
        let event = event.unwrap();
        assert!(event.is_some());

        if let Some(NodeDetectionEvent::FileEvent(FileEvent::NodeConfigModified(path))) = event {
            assert_eq!(path, peppy_file);
        } else {
            panic!("Expected NodeConfigModified event, got: {:?}", event);
        }

        watch_handle.abort();
    }

    #[tokio::test]
    async fn test_watch_files_detects_deleted_config() {
        use super::super::types::FileEvent;
        use std::time::Duration;
        use tokio::time::timeout;

        let temp_dir = TempDir::new().unwrap();
        let peppy_file = temp_dir.path().join(PEPPY_CONFIG_FILE);

        // Create file before starting watcher
        fs::write(&peppy_file, "node: test").unwrap();

        let (tx, mut rx) = mpsc::channel(10);
        let watch_dir = temp_dir.path().to_path_buf();

        // Start watching
        let watch_handle = tokio::spawn(async move { watch_files(tx, watch_dir).await });

        // Give the watcher time to initialize
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Delete the file
        fs::remove_file(&peppy_file).unwrap();

        // Wait for the event
        let event = timeout(Duration::from_secs(2), rx.recv()).await;

        assert!(event.is_ok(), "Timeout waiting for file event");
        let event = event.unwrap();
        assert!(event.is_some());

        if let Some(NodeDetectionEvent::FileEvent(FileEvent::NodeConfigDeleted(path))) = event {
            assert_eq!(path, peppy_file);
        } else {
            panic!("Expected NodeConfigDeleted event, got: {:?}", event);
        }

        watch_handle.abort();
    }

    #[tokio::test]
    async fn test_watch_files_ignores_non_peppy_files() {
        use std::time::Duration;
        use tokio::time::timeout;

        let temp_dir = TempDir::new().unwrap();
        let (tx, mut rx) = mpsc::channel(10);
        let watch_dir = temp_dir.path().to_path_buf();

        // Start watching
        let watch_handle = tokio::spawn(async move { watch_files(tx, watch_dir).await });

        // Give the watcher time to initialize
        tokio::time::sleep(Duration::from_millis(100)).await;

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
        use super::super::types::FileEvent;
        use std::time::Duration;
        use tokio::time::timeout;

        let temp_dir = TempDir::new().unwrap();
        let nested_dir = temp_dir.path().join("nested");
        fs::create_dir(&nested_dir).unwrap();

        let (tx, mut rx) = mpsc::channel(10);
        let watch_dir = temp_dir.path().to_path_buf();

        // Start watching
        let watch_handle = tokio::spawn(async move { watch_files(tx, watch_dir).await });

        // Give the watcher time to initialize
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Create a peppy config in nested directory
        let nested_peppy = nested_dir.join(PEPPY_CONFIG_FILE);
        fs::write(&nested_peppy, "node: nested").unwrap();

        // Wait for the event
        let event = timeout(Duration::from_secs(2), rx.recv()).await;

        assert!(event.is_ok(), "Timeout waiting for nested file event");
        let event = event.unwrap();
        assert!(event.is_some());

        if let Some(NodeDetectionEvent::FileEvent(FileEvent::NodeConfigCreated(path))) = event {
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
        use super::super::types::FileEvent;
        use std::time::Duration;
        use tokio::time::timeout;

        let temp_dir = TempDir::new().unwrap();
        let (tx, mut rx) = mpsc::channel(10);
        let watch_dir = temp_dir.path().to_path_buf();

        // Start watching
        let watch_handle = tokio::spawn(async move { watch_files(tx, watch_dir).await });

        // Give the watcher time to initialize
        tokio::time::sleep(Duration::from_millis(200)).await;

        // Create first peppy config file
        let peppy1 = temp_dir.path().join(PEPPY_CONFIG_FILE);
        fs::write(&peppy1, "node: one").unwrap();

        // Wait for first event
        let event1 = timeout(Duration::from_secs(2), rx.recv()).await;
        assert!(event1.is_ok(), "Should receive first event");

        // Verify the first event is a create event
        if let Ok(Some(NodeDetectionEvent::FileEvent(FileEvent::NodeConfigCreated(path)))) = event1
        {
            assert_eq!(path, peppy1, "First event should be for peppy1");
        } else {
            panic!("Expected NodeConfigCreated event for peppy1");
        }

        // Modify the file to trigger another event
        fs::write(&peppy1, "node: modified").unwrap();

        // Wait for modify event
        let event2 = timeout(Duration::from_secs(2), rx.recv()).await;
        assert!(event2.is_ok(), "Should receive second event");

        // Verify the second event is a modify event
        if let Ok(Some(NodeDetectionEvent::FileEvent(FileEvent::NodeConfigModified(path)))) = event2
        {
            assert_eq!(
                path, peppy1,
                "Second event should be for peppy1 modification"
            );
        } else {
            panic!("Expected NodeConfigModified event for peppy1");
        }

        // Small delay and drain any pending modify events from the second write
        tokio::time::sleep(Duration::from_millis(100)).await;
        while let Ok(Some(NodeDetectionEvent::FileEvent(FileEvent::NodeConfigModified(_)))) =
            timeout(Duration::from_millis(10), rx.recv()).await
        {
            // Drain extra modify events
        }

        // Delete the file to trigger a delete event
        fs::remove_file(&peppy1).unwrap();

        // Wait for delete event
        let event3 = timeout(Duration::from_secs(2), rx.recv()).await;
        assert!(event3.is_ok(), "Should receive third event");

        // Verify the third event is a delete event
        if let Ok(Some(NodeDetectionEvent::FileEvent(FileEvent::NodeConfigDeleted(path)))) = event3
        {
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
