use crate::consts::PEPPY_CONFIG_FILE;
use crate::error::{Error, Result};
use notify;
use notify::event::{AccessKind, AccessMode, ModifyKind, RenameMode};
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use super::types::FileEvent;

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::Duration;
    use tempfile::TempDir;
    use tokio::time::timeout;

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
        let temp_dir = TempDir::new().unwrap();
        let (tx, mut rx) = mpsc::channel(10);
        let watch_dir = temp_dir.path().to_path_buf();

        // Initialize watcher (await until ready) and get background handle
        let watch_handle = watch_files(tx, watch_dir).await.expect("watcher init");

        // Create a peppy config file
        let peppy_file = temp_dir.path().join(PEPPY_CONFIG_FILE);
        fs::write(&peppy_file, "node: test").unwrap();

        // Wait for the event with timeout
        let event = timeout(Duration::from_secs(2), rx.recv()).await;

        assert!(event.is_ok(), "Timeout waiting for file event");
        let event = event.unwrap();
        assert!(event.is_some());

        if let Some(FileEvent::NodeConfigCreated(path)) = event {
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
        let peppy_file = temp_dir.path().join(PEPPY_CONFIG_FILE);

        // Create file before starting watcher
        fs::write(&peppy_file, "node: test").unwrap();

        let (tx, mut rx) = mpsc::channel(10);
        let watch_dir = temp_dir.path().to_path_buf();

        // Initialize watcher and get background handle
        let watch_handle = watch_files(tx, watch_dir).await.expect("watcher init");

        // Modify the file
        fs::write(&peppy_file, "node: modified").unwrap();

        // Wait for the event
        let event = timeout(Duration::from_secs(2), rx.recv()).await;

        assert!(event.is_ok(), "Timeout waiting for file event");
        let event = event.unwrap();
        assert!(event.is_some());

        if let Some(FileEvent::NodeConfigModified(path)) = event {
            assert_eq!(path, peppy_file);
        } else {
            panic!("Expected NodeConfigModified event, got: {:?}", event);
        }

        watch_handle.abort();
    }

    #[tokio::test]
    async fn test_watch_files_detects_deleted_config() {
        let temp_dir = TempDir::new().unwrap();
        let peppy_file = temp_dir.path().join(PEPPY_CONFIG_FILE);

        // Create file before starting watcher
        fs::write(&peppy_file, "node: test").unwrap();

        let (tx, mut rx) = mpsc::channel(10);
        let watch_dir = temp_dir.path().to_path_buf();

        // Initialize watcher and get background handle
        let watch_handle = watch_files(tx, watch_dir).await.expect("watcher init");

        // Delete the file
        fs::remove_file(&peppy_file).unwrap();

        // Wait for the event
        let event = timeout(Duration::from_secs(2), rx.recv()).await;

        assert!(event.is_ok(), "Timeout waiting for file event");
        let event = event.unwrap();
        assert!(event.is_some());

        if let Some(FileEvent::NodeConfigDeleted(path)) = event {
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
        let nested_peppy = nested_dir.join(PEPPY_CONFIG_FILE);
        fs::write(&nested_peppy, "node: nested").unwrap();

        // Wait for the event
        let event = timeout(Duration::from_secs(2), rx.recv()).await;

        assert!(event.is_ok(), "Timeout waiting for nested file event");
        let event = event.unwrap();
        assert!(event.is_some());

        if let Some(FileEvent::NodeConfigCreated(path)) = event {
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
        let peppy1 = temp_dir.path().join(PEPPY_CONFIG_FILE);
        fs::write(&peppy1, "node: one").unwrap();

        // Wait for first event
        let event1 = timeout(Duration::from_secs(2), rx.recv()).await;
        assert!(event1.is_ok(), "Should receive first event");

        // Verify the first event is a create event
        if let Ok(Some(FileEvent::NodeConfigCreated(path))) = event1 {
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
        if let Ok(Some(FileEvent::NodeConfigModified(path))) = event2 {
            assert_eq!(
                path, peppy1,
                "Second event should be for peppy1 modification"
            );
        } else {
            panic!("Expected NodeConfigModified event for peppy1");
        }

        while let Ok(Some(FileEvent::NodeConfigModified(_))) =
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
        if let Ok(Some(FileEvent::NodeConfigDeleted(path))) = event3 {
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
