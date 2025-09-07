use super::ServeAsyncCommand;
use crate::error::Result;
use config::consts::PEPPY_CONFIG_FILE;
use config::{find_peppy_nodes_from_dir, watch_files};
use tokio::{sync::mpsc, task::JoinHandle};
use tracing::info;

pub struct NodeWatcher {}

impl NodeWatcher {
    // TODO: Accumulate all the nodes into a Vec<Node> and pass them into a function that handles the business logic
    // The node_watcher should specify what type event has been detected, for example if it's an internal event (a file belonging to this project has changed) or an external event (a node outside this project has joined the network of nodes).
    async fn watch_nodes() -> Result<()> {
        let (tx, mut rx) = mpsc::channel(100);
        // 1. Starting from its root directory, look for all the `PEPPY_CONFIG_FILE` configurations
        let root_dir = std::env::current_dir().expect("Failed to get current directory");
        let initial_config_files = find_peppy_nodes_from_dir(&root_dir);

        info!(
            "Found {} initial {} files in {:?}",
            initial_config_files.len(),
            PEPPY_CONFIG_FILE,
            root_dir
        );

        // 2. Initialize file watcher (returns immediately once ready)
        let tx_files = tx.clone();
        if let Err(e) = watch_files(tx_files, root_dir).await {
            eprintln!("File watcher failed to initialize: {:?}", e);
        }

        // Aggregate: receive from unified event channel
        while let Some(event) = rx.recv().await {
            // Do something with the event
            let _ = event;
        }

        Ok(())
    }
}

impl ServeAsyncCommand for NodeWatcher {
    // TODO: Function signature looks weird
    fn execute_async(&self) -> Result<JoinHandle<Result<()>>> {
        Ok(tokio::spawn(
            async move { NodeWatcher::watch_nodes().await },
        ))
    }
}
