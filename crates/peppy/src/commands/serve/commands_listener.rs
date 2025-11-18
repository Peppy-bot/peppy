use std::sync::Arc;

use crate::Error;
use pmi::Messenger;
use tokio::sync::Mutex;

pub struct MasterNode {
    messenger: Arc<Mutex<Messenger>>,
}

impl MasterNode {
    pub fn new(messenger: Arc<Mutex<Messenger>>) -> Self {
        Self { messenger }
    }
}

impl super::ServeAsyncCommand for MasterNode {
    fn run(self: Box<Self>) -> super::ServeAsyncHandle {
        let messenger = self.messenger;

        let future = Box::pin(async move {
            master_node::start_node(messenger).await.map_err(|err| {
                Error::ExecutionFailed(format!("Master node commands listener failed: {}", err))
            })
        });

        super::ServeAsyncHandle::new(future, None)
    }
}
