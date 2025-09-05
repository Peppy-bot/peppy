mod filesystem;
mod network;
mod types;

use crate::Result;
use crate::consts::PEPPY_CONFIG_FILE;
use tokio::{sync::mpsc, task::JoinHandle};
use tracing::info;
use types::NodeDetectionEvent;

pub struct NodeWatcher {}

impl NodeWatcher {
    // TODO: Accumulate all the nodes into a Vec<NodeConfig> and pass them into a function that handles the business logic
    // The node_watcher should specify what type event has been detected, for example if it's an internal event (a file belonging to this project has changed) or an external event (a node outside this project has joined the network of nodes).
    async fn watch_nodes() -> Result<()> {
        let (tx, mut rx) = mpsc::channel(100);
        // 1. Starting from its root directory, look for all the `PEPPY_CONFIG_FILE` configurations
        let root_dir = std::env::current_dir().expect("Failed to get current directory");
        let initial_config_files = filesystem::find_peppy_nodes_from_dir(&root_dir);

        info!(
            "Found {} initial {} files in {:?}",
            initial_config_files.len(),
            PEPPY_CONFIG_FILE,
            root_dir
        );

        // 2. Spawn file watcher - watches current dir recursively
        let tx_files = tx.clone();
        tokio::spawn(async move {
            if let Err(e) = filesystem::watch_files(tx_files, root_dir).await {
                eprintln!("File watcher failed: {:?}", e);
            }
        });

        // 3. Spawn network event producer, nodes can be outside the `root_dir` so they have to be detected on the network
        let tx_net = tx.clone();
        tokio::spawn(async move {
            if let Err(e) = network::network_events(tx_net).await {
                eprintln!("Network watcher failed: {:?}", e);
            }
        });

        // Aggregate: receive from unified event channel
        while let Some(event) = rx.recv().await {
            match event {
                NodeDetectionEvent::FileEvent(file_event) => {
                    println!("File event: {:?}", file_event);
                }
                NodeDetectionEvent::NetworkEvent(uri) => {
                    println!("Network event: {:?}", uri);
                }
            }
        }

        Ok(())
    }
}

impl super::ServeAsyncCommand for NodeWatcher {
    fn execute_async(&self) -> Result<JoinHandle<Result<()>>> {
        Ok(tokio::spawn(
            async move { NodeWatcher::watch_nodes().await },
        ))
    }
}
