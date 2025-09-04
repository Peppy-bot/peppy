//use super::types::{CommandContext, ServeAsyncCommand};

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::Result;
use crate::consts::PEPPY_CONFIG_FILE;
use notify::RecursiveMode;
use notify_debouncer_mini::{DebounceEventResult, new_debouncer};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tracing::{error, info, warn};

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub enum NodeEventType {
    Created(PathBuf),
    Modified(PathBuf),
    Deleted(PathBuf),
}

pub struct NodeWatcher {
    #[allow(dead_code)]
    event_tx: Option<mpsc::UnboundedSender<NodeEventType>>,
}

#[allow(dead_code)]
pub struct WatcherHandle {
    handle: tokio::task::JoinHandle<()>,
    stop_tx: std::sync::mpsc::Sender<()>,
}

impl WatcherHandle {
    pub fn stop(self) {
        let _ = self.stop_tx.send(());
        // The handle will naturally complete when the blocking task exits
    }
}

impl NodeWatcher {
    pub fn new() -> Self {
        Self { event_tx: None }
    }

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

    /// Watch a directory for changes to PEPPY_CONFIG_FILE files
    /// Returns a tuple of (receiver for events, handle to stop watching)
    pub async fn watch_directory(
        from_dir: impl AsRef<Path>,
        callback: impl Fn(NodeEventType) + Send + 'static,
    ) -> Result<(mpsc::UnboundedReceiver<NodeEventType>, WatcherHandle)> {
        let from_dir = from_dir.as_ref().to_path_buf();
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let (stop_tx, stop_rx) = std::sync::mpsc::channel();

        // Clone for the async block
        let event_tx_clone = event_tx.clone();

        // Spawn blocking task for file watcher (notify is sync)
        let handle = tokio::task::spawn_blocking(move || {
            let (tx, rx) = std::sync::mpsc::channel();

            // Create debounced watcher with 500ms delay to reduce CPU usage
            let mut debouncer = new_debouncer(
                Duration::from_millis(500),
                move |res: DebounceEventResult| {
                    match res {
                        Ok(events) => {
                            for event in events {
                                // Check if the event is for a PEPPY_CONFIG_FILE
                                if event.path.file_name()
                                    == Some(std::ffi::OsStr::new(PEPPY_CONFIG_FILE))
                                {
                                    let event_type = match event.kind {
                                        notify_debouncer_mini::DebouncedEventKind::Any => {
                                            if event.path.exists() {
                                                Some(NodeEventType::Modified(event.path.clone()))
                                            } else {
                                                Some(NodeEventType::Deleted(event.path.clone()))
                                            }
                                        }
                                        _ => {
                                            if event.path.exists() && !event.path.is_dir() {
                                                Some(NodeEventType::Modified(event.path.clone()))
                                            } else {
                                                None
                                            }
                                        }
                                    };

                                    if let Some(evt) = event_type {
                                        let _ = tx.send(evt);
                                    }
                                }
                            }
                        }
                        Err(e) => {
                            error!("Watch error: {:?}", e);
                        }
                    }
                },
            )
            .expect("Failed to create debouncer");

            // Add path to watcher
            debouncer
                .watcher()
                .watch(&from_dir, RecursiveMode::Recursive)
                .expect("Failed to watch directory");

            info!(
                "Started watching directory: {:?} for {} changes",
                from_dir, PEPPY_CONFIG_FILE
            );

            // Process events with timeout to allow checking for stop signal
            loop {
                // Check for stop signal
                if stop_rx.try_recv().is_ok() {
                    info!("Stopping file watcher");
                    break;
                }

                // Process events with timeout
                match rx.recv_timeout(Duration::from_millis(100)) {
                    Ok(event) => {
                        callback(event.clone());
                        if let Err(e) = event_tx_clone.send(event) {
                            warn!("Failed to send event through channel: {}", e);
                            break;
                        }
                    }
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                        // Continue checking for stop signal
                    }
                    Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                        break;
                    }
                }
            }
        });

        Ok((event_rx, WatcherHandle { handle, stop_tx }))
    }

    // Use the following design pattern in this module:
    // Observer Pattern for Node Watching
    //
    // The node_watcher.rs could be enhanced with:
    // - A proper event system where components can subscribe to node configuration changes
    // - This would decouple the watcher from its consumers
    // - Makes it easier to add new reactions to configuration changes

    // The goal of this module is to observe the change in `PEPPY_CONFIG_FILE` configuration files the `PEPPY_CONFIG_FILE` root configuration is pointing to.
    // If a file has changed, the watcher should notify all subscribers about the change.
    // Beyond configuration file change, a node can also communicate with other nodes via pubsub even tho this node is not part of the file configuration of the current project.
    // The node_watcher should specify what type event has been detected, for example if it's an internal event (a file belonging to this project has changed) or an external event (a node outside this project has joined the network of nodes).
    // The main subscriber to this node_watcher is the python dependency or the Rust crate that is automatically generated inside the .pixi virtualenv (and added to pixi.toml) when a file configuration changes.
    async fn watch_node_configuration_files_changes() -> Result<()> {
        // 1. Starting from its root directory, look for all the `PEPPY_CONFIG_FILE`
        let cur_dir = std::env::current_dir().expect("Failed to get current directory");
        let initial_config_files = NodeWatcher::find_peppy_nodes_from_dir(&cur_dir);

        info!(
            "Found {} initial {} files in {:?}",
            initial_config_files.len(),
            PEPPY_CONFIG_FILE,
            cur_dir
        );

        for file in &initial_config_files {
            info!("  - {:?}", file);
        }

        // Set up the watcher with a callback that logs events
        let (mut event_rx, _watcher_handle) = NodeWatcher::watch_directory(&cur_dir, |event| {
            match &event {
                NodeEventType::Created(path) => {
                    info!("New {} created: {:?}", PEPPY_CONFIG_FILE, path);
                    // TODO: Handle new config file (e.g., update pixi env)
                }
                NodeEventType::Modified(path) => {
                    info!("{} modified: {:?}", PEPPY_CONFIG_FILE, path);
                    // TODO: Handle modified config file (e.g., regenerate bindings)
                }
                NodeEventType::Deleted(path) => {
                    info!("{} deleted: {:?}", PEPPY_CONFIG_FILE, path);
                    // TODO: Handle deleted config file (e.g., cleanup pixi env)
                }
            }
        })
        .await?;

        info!(
            "Started watching for {} changes in {:?}",
            PEPPY_CONFIG_FILE, cur_dir
        );

        // Process events from the channel
        while let Some(event) = event_rx.recv().await {
            // Additional processing can be done here if needed
            // The callback above already handles logging
            match event {
                NodeEventType::Created(_) | NodeEventType::Modified(_) => {
                    // Could trigger regeneration of Python/Rust bindings here
                }
                NodeEventType::Deleted(_) => {
                    // Could trigger cleanup here
                }
            }
        }

        Ok(())
    }
}

impl super::ServeAsyncCommand for NodeWatcher {
    fn execute_async(&self) -> Result<JoinHandle<Result<()>>> {
        let handle =
            tokio::spawn(
                async move { NodeWatcher::watch_node_configuration_files_changes().await },
            );

        Ok(handle)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::Duration;
    use tempfile::TempDir;
    use tokio::sync::mpsc;

    #[test]
    fn test_find_peppy_nodes_from_dir() {
        let temp_dir = TempDir::new().unwrap();
        let temp_path = temp_dir.path();

        let node1_dir = temp_path.join("node1");
        fs::create_dir(&node1_dir).unwrap();
        fs::write(node1_dir.join(PEPPY_CONFIG_FILE), "test content 1").unwrap();

        let node2_dir = temp_path.join("node2");
        let node2_subdir = node2_dir.join("subdir");
        fs::create_dir_all(&node2_subdir).unwrap();
        fs::write(node2_subdir.join(PEPPY_CONFIG_FILE), "test content 2").unwrap();

        fs::write(temp_path.join(PEPPY_CONFIG_FILE), "root config").unwrap();

        fs::write(temp_path.join("not_peppy.yaml"), "should not be found").unwrap();
        fs::write(temp_path.join("peppy.txt"), "should not be found").unwrap();

        let found_files = NodeWatcher::find_peppy_nodes_from_dir(temp_path);

        assert_eq!(found_files.len(), 3);

        let found_names: Vec<_> = found_files
            .iter()
            .map(|p| p.strip_prefix(temp_path).unwrap().to_path_buf())
            .collect();

        assert!(found_names.contains(&PathBuf::from(PEPPY_CONFIG_FILE)));
        assert!(found_names.contains(&PathBuf::from("node1").join(PEPPY_CONFIG_FILE)));
        assert!(
            found_names.contains(
                &PathBuf::from("node2")
                    .join("subdir")
                    .join(PEPPY_CONFIG_FILE)
            )
        );
    }

    #[test]
    fn test_find_peppy_nodes_from_empty_dir() {
        let temp_dir = TempDir::new().unwrap();
        let found_files = NodeWatcher::find_peppy_nodes_from_dir(temp_dir.path());
        assert_eq!(found_files.len(), 0);
    }

    #[test]
    fn test_find_peppy_nodes_from_non_existent_dir() {
        let found_files = NodeWatcher::find_peppy_nodes_from_dir("/non/existent/path");
        assert_eq!(found_files.len(), 0);
    }

    #[test]
    fn test_find_peppy_nodes_from_file_path() {
        let temp_dir = TempDir::new().unwrap();
        let file_path = temp_dir.path().join("some_file.txt");
        fs::write(&file_path, "content").unwrap();

        let found_files = NodeWatcher::find_peppy_nodes_from_dir(&file_path);
        assert_eq!(found_files.len(), 0);
    }

    #[tokio::test]
    async fn test_watch_directory_detects_new_file() {
        let temp_dir = TempDir::new().unwrap();
        let temp_path = temp_dir.path().to_path_buf();

        // Create a channel to capture events
        let (test_tx, mut test_rx) = mpsc::unbounded_channel();

        // Start watching
        let (mut event_rx, watcher_handle) =
            NodeWatcher::watch_directory(&temp_path, move |event| {
                let _ = test_tx.send(event);
            })
            .await
            .unwrap();

        // Give the watcher time to initialize
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Create a new PEPPY_CONFIG_FILE
        let config_path = temp_path.join(PEPPY_CONFIG_FILE);
        fs::write(&config_path, "test content").unwrap();

        // Wait for the debounce period plus some buffer
        tokio::time::sleep(Duration::from_millis(600)).await;

        // Check if we received an event
        if let Ok(event) = test_rx.try_recv() {
            match event {
                NodeEventType::Created(path) | NodeEventType::Modified(path) => {
                    assert_eq!(path, config_path);
                }
                _ => panic!("Unexpected event type"),
            }
        } else {
            // Sometimes the event might come through the main channel
            if let Ok(event) = event_rx.try_recv() {
                match event {
                    NodeEventType::Created(path) | NodeEventType::Modified(path) => {
                        assert_eq!(path, config_path);
                    }
                    _ => panic!("Unexpected event type"),
                }
            } else {
                panic!("No event received for new file");
            }
        }

        // Clean up
        watcher_handle.stop();
    }

    #[tokio::test]
    async fn test_watch_directory_detects_modified_file() {
        let temp_dir = TempDir::new().unwrap();
        let temp_path = temp_dir.path().to_path_buf();

        // Create initial file
        let config_path = temp_path.join(PEPPY_CONFIG_FILE);
        fs::write(&config_path, "initial content").unwrap();

        // Create a channel to capture events
        let (test_tx, mut test_rx) = mpsc::unbounded_channel();

        // Start watching
        let (mut event_rx, watcher_handle) =
            NodeWatcher::watch_directory(&temp_path, move |event| {
                let _ = test_tx.send(event);
            })
            .await
            .unwrap();

        // Give the watcher time to initialize
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Modify the file
        fs::write(&config_path, "modified content").unwrap();

        // Wait for the debounce period plus some buffer
        tokio::time::sleep(Duration::from_millis(600)).await;

        // Check if we received an event
        if let Ok(event) = test_rx.try_recv() {
            match event {
                NodeEventType::Modified(path) => {
                    assert_eq!(path, config_path);
                }
                _ => panic!("Unexpected event type"),
            }
        } else {
            // Sometimes the event might come through the main channel
            if let Ok(event) = event_rx.try_recv() {
                match event {
                    NodeEventType::Modified(path) => {
                        assert_eq!(path, config_path);
                    }
                    _ => panic!("Unexpected event type"),
                }
            } else {
                panic!("No event received for modified file");
            }
        }

        // Clean up
        watcher_handle.stop();
    }

    #[tokio::test]
    async fn test_watch_directory_ignores_non_peppy_files() {
        let temp_dir = TempDir::new().unwrap();
        let temp_path = temp_dir.path().to_path_buf();

        // Create a channel to capture events
        let (test_tx, mut test_rx) = mpsc::unbounded_channel();

        // Start watching
        let (_event_rx, watcher_handle) = NodeWatcher::watch_directory(&temp_path, move |event| {
            let _ = test_tx.send(event);
        })
        .await
        .unwrap();

        // Give the watcher time to initialize
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Create a non-PEPPY file
        let test_path = temp_dir.path().to_path_buf();
        fs::write(test_path.join("other.yaml"), "test content").unwrap();
        fs::write(test_path.join("test.txt"), "test content").unwrap();

        // Wait for the debounce period
        tokio::time::sleep(Duration::from_millis(600)).await;

        // Should not receive any events
        assert!(
            test_rx.try_recv().is_err(),
            "Should not receive events for non-peppy files"
        );

        // Clean up
        watcher_handle.stop();
    }
}
