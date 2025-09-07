use super::ServeAsyncCommand;
use crate::error::Result;
use config::consts::PEPPY_CONFIG_FILE;
use config::{find_peppy_nodes_from_dir, watch_files};
use tokio::{sync::mpsc, task::JoinHandle};
use tracing::info;

pub struct NodeWatcher {}

impl NodeWatcher {
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
            // Transform the files to NodeConfig
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
