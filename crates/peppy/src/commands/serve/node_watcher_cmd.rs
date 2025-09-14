use super::ServeAsyncCommand;
use crate::error::Result;
use config::NodeConfigWatcher;
use tokio::task::JoinHandle;

pub struct NodeWatcher {
    pub strict: bool,
}

impl NodeWatcher {
    async fn watch_nodes(_strict: bool) -> Result<()> {
        let root_dir = std::env::current_dir().expect("Failed to get current directory");
        let _node_config_watcher = NodeConfigWatcher::new(root_dir);

        // TODO: Using `node_config_watcher`, start/restart/stop the nodes and send a signal to InterfacesGenerator and root_node

        Ok(())
    }
}

impl ServeAsyncCommand for NodeWatcher {
    fn execute_async(&self) -> Result<JoinHandle<Result<()>>> {
        let strict = self.strict;
        Ok(tokio::spawn(async move {
            NodeWatcher::watch_nodes(strict).await
        }))
    }
}
