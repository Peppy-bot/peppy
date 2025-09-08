use std::collections::HashMap;
use std::path::{Path, PathBuf};

use super::discovery::find_peppy_nodes_from_dir;
use super::events::NodeConfigEvent;
use super::fs::watch_files;
use crate::NodeConfigParser;
use crate::error::{ParsingError, Result};
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use tracing::{info, warn};

use crate::{NodeConfig, consts::PEPPY_CONFIG_FILE};

/// A simple, self-contained watcher that maintains an aggregated mapping of
/// `peppy.yaml` file paths to parsed `NodeConfig`s for a directory tree.
///
/// Usage:
/// - Create with `new(dir)`.
/// - Subscribe to state updates via `subscribe()`.
/// - Start background watching with `start().await` (returns a handle you can await/abort).
pub struct NodeConfigWatcher {
    from_dir: PathBuf,
    state_tx: watch::Sender<HashMap<PathBuf, NodeConfig>>,
}

impl NodeConfigWatcher {
    /// Initialize the watcher with the initial aggregated state.
    pub fn new(from_dir: impl AsRef<Path>) -> Result<Self> {
        let from_dir = from_dir.as_ref().to_path_buf();
        let initial_state = Self::load_initial_configs(&from_dir)?;
        let (state_tx, _state_rx) = watch::channel(initial_state);
        Ok(Self { from_dir, state_tx })
    }

    /// Subscribe to the aggregated state stream. Each subscriber receives a
    /// watch receiver seeded with the current state and subsequent updates.
    pub fn subscribe(&self) -> watch::Receiver<HashMap<PathBuf, NodeConfig>> {
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

    fn update_state(state: &mut HashMap<PathBuf, NodeConfig>, event: NodeConfigEvent) {
        match event {
            NodeConfigEvent::Created(path) | NodeConfigEvent::Modified(path) => {
                match NodeConfigParser::from_path(&path) {
                    Ok(config) => {
                        state.insert(path, config);
                    }
                    Err(err) => warn!("Could not parse {}: {}", path.display(), err),
                }
            }
            NodeConfigEvent::Deleted(path) => {
                state.remove(&path);
            }
        }
    }

    fn load_initial_configs(from_dir: &Path) -> Result<HashMap<PathBuf, NodeConfig>> {
        let config_files = find_peppy_nodes_from_dir(from_dir);
        info!(
            "Found {} initial {} files in {:?}",
            config_files.len(),
            PEPPY_CONFIG_FILE,
            from_dir
        );

        config_files
            .into_iter()
            .map(|path| NodeConfigParser::from_path(&path).map(|config| (path, config)))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::consts::PEPPY_CONFIG_FILE;
    use std::fs;
    use std::time::Duration;
    use tempfile::TempDir;
    use tokio::time::timeout;

    fn write_config(dir: &Path, name: &str, namespace: &str) -> PathBuf {
        let path = dir.join(PEPPY_CONFIG_FILE);
        let yaml = format!(
            r#"node_config:
  name: {name}
  namespace: {namespace}
"#
        );
        fs::write(&path, yaml).unwrap();
        path
    }

    #[test]
    fn test_initial_state_loads_all_configs() {
        let temp = TempDir::new().unwrap();

        // root config
        let root = write_config(temp.path(), "root_node", "/root");

        // nested config
        let nested_dir = temp.path().join("nested");
        fs::create_dir(&nested_dir).unwrap();
        let nested = write_config(&nested_dir, "nested_node", "/nested");

        let watcher = NodeConfigWatcher::new(temp.path()).expect("watcher init");
        let rx = watcher.subscribe();
        let state = rx.borrow().clone();

        assert_eq!(state.len(), 2);
        assert!(state.contains_key(&root));
        assert!(state.contains_key(&nested));
        assert_eq!(state[&root].node_config.name.as_str(), "root_node");
        assert_eq!(state[&nested].node_config.name.as_str(), "nested_node");
    }

    #[test]
    fn test_new_errors_on_invalid_initial_config() {
        let temp = TempDir::new().unwrap();

        // Invalid name (spaces and '!') should fail parsing on initial load
        fs::write(
            temp.path().join(PEPPY_CONFIG_FILE),
            "node_config:\n  name: Invalid Name!\n  namespace: /ns\n",
        )
        .unwrap();

        let res = NodeConfigWatcher::new(temp.path());
        assert!(
            res.is_err(),
            "watcher should error on invalid initial config"
        );
    }

    #[tokio::test]
    async fn test_invalid_modify_does_not_replace_existing_state() {
        let temp = TempDir::new().unwrap();
        let config_path = write_config(temp.path(), "ok", "/ns");

        let watcher = NodeConfigWatcher::new(temp.path()).expect("watcher init");
        let mut rx = watcher.subscribe();
        assert_eq!(rx.borrow()[&config_path].node_config.name.as_str(), "ok");

        let handle = watcher.start().await.expect("start background");

        // Write invalid content (invalid node name)
        fs::write(
            &config_path,
            "node_config:\n  name: Invalid Name!\n  namespace: /ns\n",
        )
        .unwrap();

        // Wait for a change notification
        timeout(Duration::from_secs(2), rx.changed())
            .await
            .expect("state change expected")
            .expect("receiver still active");
        // State should remain with previous valid content
        assert_eq!(rx.borrow()[&config_path].node_config.name.as_str(), "ok");

        handle.abort();
    }

    #[tokio::test]
    async fn test_state_updates_propagate_to_multiple_subscribers() {
        let temp = TempDir::new().unwrap();
        let watcher = NodeConfigWatcher::new(temp.path()).expect("watcher init");

        let mut rx1 = watcher.subscribe();
        let mut rx2 = watcher.subscribe();

        let handle = watcher.start().await.expect("start background");

        // Create a new config
        let created = write_config(temp.path(), "multi_sub", "/ns");

        // Both subscribers should receive the update
        timeout(Duration::from_secs(2), rx1.changed())
            .await
            .expect("rx1 should receive update")
            .expect("rx1 still active");
        timeout(Duration::from_secs(2), rx2.changed())
            .await
            .expect("rx2 should receive update")
            .expect("rx2 still active");

        assert!(rx1.borrow().contains_key(&created));
        assert!(rx2.borrow().contains_key(&created));

        handle.abort();
    }
}
