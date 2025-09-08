use std::collections::HashMap;
use std::path::{Path, PathBuf};

use super::discovery::find_peppy_nodes_from_dir;
use super::events::FileEvent;
use super::fs::watch_files;
use crate::NodeConfigParser;
use crate::error::Result;
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

    fn update_state(state: &mut HashMap<PathBuf, NodeConfig>, event: FileEvent) {
        match event {
            FileEvent::NodeConfigCreated(path) | FileEvent::NodeConfigModified(path) => {
                match NodeConfigParser::from_path(&path) {
                    Ok(config) => {
                        state.insert(path, config);
                    }
                    Err(err) => warn!("Could not parse {}: {}", path.display(), err),
                }
            }
            FileEvent::NodeConfigDeleted(path) => {
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
