use std::sync::Arc;

use crate::Error;
use config::node::NodeConfig;
use master_node::MasterNode;
use pmi::Messenger;
use tokio::sync::Mutex;

pub struct MasterNodeRunner {
    master_node: MasterNode,
}

impl MasterNodeRunner {
    pub fn new(messenger: Arc<Mutex<Messenger>>) -> Self {
        let master_node = MasterNode::new(messenger);
        Self { master_node }
    }

    pub fn config(&self) -> &NodeConfig {
        self.master_node.node_config()
    }
}

impl super::ServeAsyncCommand for MasterNodeRunner {
    fn run(self: Box<Self>) -> super::ServeAsyncHandle {
        let master_node = self.master_node;
        let future = Box::pin(async move {
            master_node.start().await.map_err(|err| {
                Error::ExecutionFailed(format!("Master node commands listener failed: {}", err))
            })
        });

        super::ServeAsyncHandle::new(future, None)
    }
}
