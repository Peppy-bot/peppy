use std::collections::HashMap;
use std::path::{Path, PathBuf};

use super::super::node::NodeConfigParser;
use super::discovery::find_peppy_nodes_from_dir;
use super::events::NodeConfigEvent;
use super::fs::watch_files;
use crate::error::{Error, ParsingError, Result};
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use tracing::{info, warn};

use crate::{consts::NODE_CONFIG_FILE, node::NodeConfig};

/// Aggregated state keyed by config file path. Each entry reflects the
/// current parse result of the corresponding `peppy.json5` file.
pub type NodeIndexState = HashMap<PathBuf, core::result::Result<NodeConfig, ParsingError>>;

/// A simple, self-contained watcher that maintains aggregated state for a directory tree.
/// The state maps each `peppy.json5` file path to the latest parse result
/// (`Ok(NodeConfig)` or `Err(ParsingError)`).
///
/// Usage:
/// - Create with `new(dir)`.
/// - Subscribe to state updates via `subscribe()`.
/// - Start background watching with `start().await` (returns a handle you can await/abort).
pub struct FSNodeConfigWatcher {
    from_dir: PathBuf,
    state_tx: watch::Sender<NodeIndexState>,
}

impl FSNodeConfigWatcher {
    /// Initialize the watcher with the initial aggregated state.
    pub fn new(from_dir: impl AsRef<Path>) -> Result<Self> {
        let from_dir = from_dir.as_ref().to_path_buf();
        let initial_state = Self::load_initial_state(&from_dir);
        let (state_tx, _state_rx) = watch::channel(initial_state);
        Ok(Self { from_dir, state_tx })
    }

    /// Subscribe to the aggregated state stream. Each subscriber receives a
    /// watch receiver seeded with the current state and subsequent updates.
    pub fn subscribe(&self) -> watch::Receiver<NodeIndexState> {
        self.state_tx.subscribe()
    }

    /// Start watching for changes and updating subscribers with the full state
    /// on every change. The returned handle can be awaited or aborted. The
    /// background task will also stop automatically once all receivers are
    /// dropped.
    pub async fn start(&self) -> Result<JoinHandle<Result<()>>> {
        let from_dir = self.from_dir.clone();
        let state_tx = self.state_tx.clone();

        let (file_events_tx, mut file_events_rx) = mpsc::channel(100);
        let watch_handle = watch_files(file_events_tx, &from_dir).await?;

        let initial_state = state_tx.borrow().clone();

        let handle = tokio::spawn(async move {
            let mut state = initial_state;
            loop {
                tokio::select! {
                    // Stop watching when all receivers drop
                    _ = state_tx.closed() => {
                        watch_handle.abort();
                        break;
                    }
                    Some(event) = file_events_rx.recv() => {
                        Self::update_state(&mut state, event);
                        if state_tx.send(state.clone()).is_err() {
                            // No receivers left; exit loop
                            watch_handle.abort();
                            break;
                        }
                    }
                    else => break,
                }
            }
            Ok(())
        });

        Ok(handle)
    }

    fn update_state(state: &mut NodeIndexState, event: NodeConfigEvent) {
        match event {
            NodeConfigEvent::Created(path) => match NodeConfigParser::from_path(&path) {
                Ok(config) => {
                    state.insert(path, Ok(config));
                }
                Err(err) => {
                    warn!("Could not parse {}: {}", path.display(), err);
                    if let Error::Parsing(pe) = err {
                        state.insert(path, Err(pe));
                    }
                }
            },
            NodeConfigEvent::Modified(path) => {
                // Only update if the path already exists in state
                if let Some(entry) = state.get_mut(&path) {
                    match NodeConfigParser::from_path(&path) {
                        Ok(config) => {
                            *entry = Ok(config);
                        }
                        Err(err) => {
                            // Replace with the latest error to reflect current state
                            warn!("Could not parse {}: {}", path.display(), err);
                            if let Error::Parsing(pe) = err {
                                *entry = Err(pe);
                            }
                        }
                    }
                }
            }
            NodeConfigEvent::Deleted(path) => {
                // Keep the entry but mark it as deleted, so consumers
                // can surface a meaningful error instead of losing history.
                state.insert(
                    path.clone(),
                    Err(ParsingError::DeletedFile(path.display().to_string())),
                );
            }
        }
    }

    fn load_initial_state(from_dir: &Path) -> NodeIndexState {
        let config_files = find_peppy_nodes_from_dir(from_dir);
        info!(
            "Found {} initial {} files in {:?}",
            config_files.len(),
            NODE_CONFIG_FILE,
            from_dir
        );
        let mut state: NodeIndexState = HashMap::new();
        for path in config_files {
            match NodeConfigParser::from_path(&path) {
                Ok(cfg) => {
                    state.insert(path, Ok(cfg));
                }
                Err(err) => {
                    warn!("Could not parse {}: {}", path.display(), err);
                    if let Error::Parsing(pe) = err {
                        state.insert(path, Err(pe));
                    }
                }
            }
        }
        state
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::consts::NODE_CONFIG_FILE;
    use std::fs;
    use std::time::Duration;
    use tempfile::TempDir;
    use tokio::time::timeout;

    fn write_config(dir: &Path, name: &str) -> PathBuf {
        let path = dir.join(NODE_CONFIG_FILE);
        let json5 = format!(
            r#"{{
                schema_version: 1,
                manifest: {{
                    name: "{name}",
                    tag: "0.1.0",
                    launch_cmd: ["cargo", "run", "--release"]
                }}
            }}"#
        );
        fs::write(&path, json5).unwrap();
        path
    }

    #[test]
    fn test_initial_state_loads_all_configs() {
        let temp = TempDir::new().unwrap();

        // config 1
        let base = write_config(temp.path(), "base_node");

        // nested config
        let nested_dir = temp.path().join("nested");
        fs::create_dir(&nested_dir).unwrap();
        let nested = write_config(&nested_dir, "nested_node");

        let watcher = FSNodeConfigWatcher::new(temp.path()).expect("watcher init");
        let rx = watcher.subscribe();
        let state = rx.borrow().clone();

        assert_eq!(state.len(), 2);
        assert!(state.contains_key(&base));
        assert!(state.contains_key(&nested));
        assert_eq!(
            state[&base].as_ref().unwrap().manifest.name.as_str(),
            "base_node"
        );
        assert_eq!(
            state[&nested].as_ref().unwrap().manifest.name.as_str(),
            "nested_node"
        );
        assert!(state.values().all(|e| e.is_ok()));
    }

    #[test]
    fn test_new_reports_invalid_initial_config_via_state() {
        let temp = TempDir::new().unwrap();

        // Invalid name (spaces and '!') should fail parsing on initial load
        fs::write(
            temp.path().join(NODE_CONFIG_FILE),
            "{ schema_version: 1, manifest: { name: 'Invalid Name!', tag: '0.1.0', launch_cmd: ['cargo', 'run', '--release'] } }",
        )
        .unwrap();

        let watcher = FSNodeConfigWatcher::new(temp.path()).expect("watcher init");
        let rx = watcher.subscribe();
        let state = rx.borrow().clone();
        assert_eq!(state.len(), 1);
        let entry = state.values().next().unwrap();
        assert!(entry.is_err());
    }

    #[tokio::test]
    async fn test_invalid_modify_sets_error_state() {
        let temp = TempDir::new().unwrap();
        let config_path = write_config(temp.path(), "ok");

        let watcher = FSNodeConfigWatcher::new(temp.path()).expect("watcher init");
        let mut rx = watcher.subscribe();
        assert_eq!(
            rx.borrow()[&config_path]
                .as_ref()
                .unwrap()
                .manifest
                .name
                .as_str(),
            "ok"
        );

        let handle = watcher.start().await.expect("start background");

        // Write invalid content (invalid node name)
        fs::write(
            &config_path,
            "{ schema_version: 1, manifest: { name: 'Invalid Name!', tag: '0.1.0', language: 'rust' } }",
        )
        .unwrap();

        // Wait for a change notification
        timeout(Duration::from_secs(1), rx.changed())
            .await
            .expect("state change expected")
            .expect("receiver still active");
        assert!(rx.borrow()[&config_path].is_err());

        handle.abort();
    }

    #[tokio::test]
    async fn test_state_updates_propagate_to_multiple_subscribers() {
        let temp = TempDir::new().unwrap();
        let watcher = FSNodeConfigWatcher::new(temp.path()).expect("watcher init");

        let mut rx1 = watcher.subscribe();
        let mut rx2 = watcher.subscribe();

        let handle = watcher.start().await.expect("start background");

        // Create a new config
        let created = write_config(temp.path(), "multi_sub");

        // Both subscribers should receive the update
        timeout(Duration::from_secs(1), rx1.changed())
            .await
            .expect("rx1 should receive update")
            .expect("rx1 still active");
        timeout(Duration::from_secs(1), rx2.changed())
            .await
            .expect("rx2 should receive update")
            .expect("rx2 still active");

        assert!(rx1.borrow().contains_key(&created));
        assert!(rx2.borrow().contains_key(&created));

        // Modify the config with a new valid name
        write_config(temp.path(), "multi_sub_v2");

        // Both subscribers should receive the modification
        timeout(Duration::from_secs(1), rx1.changed())
            .await
            .expect("rx1 should receive modify update")
            .expect("rx1 still active");
        timeout(Duration::from_secs(1), rx2.changed())
            .await
            .expect("rx2 should receive modify update")
            .expect("rx2 still active");

        assert_eq!(
            rx1.borrow()[&created]
                .as_ref()
                .unwrap()
                .manifest
                .name
                .as_str(),
            "multi_sub_v2"
        );
        assert_eq!(
            rx2.borrow()[&created]
                .as_ref()
                .unwrap()
                .manifest
                .name
                .as_str(),
            "multi_sub_v2"
        );

        // Delete the config
        std::fs::remove_file(&created).expect("delete config file");

        // Both subscribers should receive the deletion
        timeout(Duration::from_secs(1), rx1.changed())
            .await
            .expect("rx1 should receive delete update")
            .expect("rx1 still active");
        timeout(Duration::from_secs(1), rx2.changed())
            .await
            .expect("rx2 should receive delete update")
            .expect("rx2 still active");

        // Entry should remain but reflect a DeletedFile error
        let s1 = rx1.borrow();
        let s2 = rx2.borrow();
        assert!(s1.contains_key(&created));
        assert!(s2.contains_key(&created));
        assert!(matches!(s1[&created], Err(ParsingError::DeletedFile(_))));
        assert!(matches!(s2[&created], Err(ParsingError::DeletedFile(_))));

        handle.abort();
    }

    #[tokio::test]
    async fn test_create_delete_recreate_updates_state() {
        let temp = TempDir::new().unwrap();
        let config_path = write_config(temp.path(), "first");

        let watcher = FSNodeConfigWatcher::new(temp.path()).expect("watcher init");
        let mut rx = watcher.subscribe();

        // Initial state reflects the created file
        assert_eq!(
            rx.borrow()[&config_path]
                .as_ref()
                .unwrap()
                .manifest
                .name
                .as_str(),
            "first"
        );

        let handle = watcher.start().await.expect("start background");

        // Delete the file and expect an error state with DeletedFile
        std::fs::remove_file(&config_path).expect("delete config file");
        timeout(Duration::from_secs(1), rx.changed())
            .await
            .expect("state change expected after delete")
            .expect("receiver still active");
        assert!(matches!(
            rx.borrow()[&config_path],
            Err(ParsingError::DeletedFile(_))
        ));

        // Recreate the file with a new valid name and expect Ok again
        write_config(temp.path(), "second");
        timeout(Duration::from_secs(1), rx.changed())
            .await
            .expect("state change expected after recreate")
            .expect("receiver still active");
        assert_eq!(
            rx.borrow()[&config_path]
                .as_ref()
                .unwrap()
                .manifest
                .name
                .as_str(),
            "second"
        );

        handle.abort();
    }
}
