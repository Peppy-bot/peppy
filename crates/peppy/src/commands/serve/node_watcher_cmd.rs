use super::{ServeAsyncCommand, ServeFuture};
use crate::{AppContext, AppEvent, Result};
use config::NodeConfigWatcher;
use tokio::sync::broadcast;

pub struct NodeWatcher {
    strict: bool,
    event_sender: broadcast::Sender<AppEvent>,
}

impl NodeWatcher {
    pub fn new(strict: bool, ctx: &AppContext) -> Self {
        let event_sender = ctx.event_sender();
        Self {
            strict,
            event_sender,
        }
    }

    async fn watch_nodes(_strict: bool, _event_sender: broadcast::Sender<AppEvent>) -> Result<()> {
        let root_dir = std::env::current_dir().expect("Failed to get current directory");
        let _node_config_watcher = NodeConfigWatcher::new(root_dir);

        // TODO: Using `node_config_watcher`, start/restart/stop the nodes and send a signal to InterfacesGenerator and root_node

        Ok(())
    }
}

impl ServeAsyncCommand for NodeWatcher {
    fn run(&self) -> ServeFuture {
        let strict = self.strict;
        let event_sender = self.event_sender.clone();
        Box::pin(async move { NodeWatcher::watch_nodes(strict, event_sender).await })
    }
}
