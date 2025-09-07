use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::error::Result;
use crate::{FileEvent, NodeConfigParser};
use tokio::sync::mpsc;
use tracing::{info, warn};

use crate::{NodeConfig, consts::PEPPY_CONFIG_FILE, find_peppy_nodes_from_dir, watch_files};

// TODO: Accumulate all the nodes into a Vec<NodeConfig> and pass them into a function that handles the business logic
// The node_watcher should specify what type event has been detected, for example if it's an internal event (a file belonging to this project has changed) or an external event (a node outside this project has joined the network of nodes).
pub async fn get_node_config_from_files(
    from_dir: impl AsRef<Path>,
) -> Result<HashMap<PathBuf, NodeConfig>> {
    let (tx, mut rx) = mpsc::channel(100);
    let initial_config_files = find_peppy_nodes_from_dir(&from_dir);

    info!(
        "Found {} initial {} files in {:?}",
        initial_config_files.len(),
        PEPPY_CONFIG_FILE,
        from_dir.as_ref()
    );

    // Parse config files into a HashMap
    let mut configs_by_path = HashMap::with_capacity(initial_config_files.len());
    for path in initial_config_files {
        // Parse config and propagate errors during initial loading
        let config = NodeConfigParser::from_path(&path)?;
        configs_by_path.insert(path, config);
    }

    // Initialize file watcher (returns immediately once ready and emits new files on tx)
    if let Err(e) = watch_files(tx, from_dir).await {
        eprintln!("File watcher failed to initialize: {:?}", e);
    }

    // TODO notify the caller
    // Aggregate: receive from unified event channel
    while let Some(event) = rx.recv().await {
        match event {
            FileEvent::NodeConfigCreated(path) => match NodeConfigParser::from_path(&path) {
                Ok(config) => {
                    configs_by_path.insert(path, config);
                }
                Err(err) => warn!("Could not parse {}: {}", path.display(), err),
            },
            FileEvent::NodeConfigModified(path) => {
                // Re-parse the modified config and update it in the map
                match NodeConfigParser::from_path(&path) {
                    Ok(config) => {
                        configs_by_path.insert(path, config);
                    }
                    Err(err) => warn!(
                        "Could not parse modified config {}: {}",
                        path.display(),
                        err
                    ),
                }
            }
            FileEvent::NodeConfigDeleted(path) => {
                // Remove the deleted config from the map
                configs_by_path.remove(&path);
            }
        }
    }

    Ok(configs_by_path)
}
