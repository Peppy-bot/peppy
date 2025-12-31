use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use crate::error::Error;
use master_node::{MasterNode, MasterNodeArguments};
use pmi::Messenger;
use tokio::sync::Mutex;
use tracing::info;

pub struct MasterNodeRunner {
    master_node: MasterNode,
}

impl MasterNodeRunner {
    pub fn new(
        messenger: Arc<Mutex<Messenger>>,
        master_name: Option<String>,
        node_start_health_timeout: Duration,
        root_dir: PathBuf,
    ) -> Self {
        let node_arguments = MasterNodeArguments {
            node_start_health_timeout,
        };
        let master_node =
            MasterNode::new(messenger, master_name.as_deref(), node_arguments, root_dir);
        Self { master_node }
    }

    pub fn node_name(&self) -> &str {
        self.master_node.node_name()
    }
}

impl super::ServeAsyncCommand for MasterNodeRunner {
    fn run(self: Box<Self>) -> super::ServeAsyncHandle {
        let master_node = self.master_node;
        let future = Box::pin(async move {
            let shutdown_signal = tokio::signal::ctrl_c();
            tokio::pin!(shutdown_signal);

            let master_node_future = master_node.start();
            tokio::pin!(master_node_future);

            let mut shutdown_triggered = false;
            let result: Result<(), Error> = tokio::select! {
                result = &mut master_node_future => {
                    result.map_err(|err| {
                        Error::ExecutionFailed(format!(
                            "Master node commands listener failed: {}",
                            err
                        ))
                    })
                }
                ctrl_c_result = &mut shutdown_signal => {
                    shutdown_triggered = true;
                    match ctrl_c_result {
                        Ok(()) => Ok(()),
                        Err(err) => Err(Error::ExecutionFailed(format!(
                            "Failed to listen for shutdown signal: {}",
                            err
                        ))),
                    }
                }
            };

            if shutdown_triggered {
                info!("Shutting down commands listener...");
            }

            result
        });

        super::ServeAsyncHandle::new(future, None)
    }
}
