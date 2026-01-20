use super::serve::{ServeAsyncCommand, ServeAsyncHandle};
use crate::error::Error;
use master_node::{MasterNode, MasterNodeArguments};
use pmi::Messenger;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{Mutex, oneshot, watch};
use tracing::info;

pub struct MasterNodeRunner {
    master_node: MasterNode,
    messaging_ready: Option<watch::Receiver<bool>>,
}

impl MasterNodeRunner {
    pub fn new(
        messenger: Arc<Mutex<Messenger>>,
        master_name: Option<String>,
        node_startup_timeout: Duration,
        node_start_health_timeout: Duration,
        root_dir: PathBuf,
        messaging_ready: Option<watch::Receiver<bool>>,
        daemon_git_hash: impl Into<String>,
    ) -> Self {
        let node_arguments = MasterNodeArguments {
            node_startup_timeout,
            node_start_health_timeout,
        };
        let master_node = MasterNode::new(
            messenger,
            master_name.as_deref(),
            node_arguments,
            root_dir,
            daemon_git_hash,
        );
        Self {
            master_node,
            messaging_ready,
        }
    }

    pub fn node_name(&self) -> &str {
        self.master_node.node_name()
    }
}

impl ServeAsyncCommand for MasterNodeRunner {
    fn run(self: Box<Self>) -> ServeAsyncHandle {
        let (ready_tx, ready_rx) = oneshot::channel();
        let master_node = self.master_node;
        let mut messaging_ready = self.messaging_ready;
        let future = Box::pin(async move {
            let shutdown_signal = tokio::signal::ctrl_c();
            tokio::pin!(shutdown_signal);

            if let Some(mut ready_rx) = messaging_ready.take() {
                if !*ready_rx.borrow() {
                    info!("Waiting for messaging session before starting master node...");
                    ready_rx.changed().await.map_err(|_| {
                        Error::ExecutionFailed(
                            "Messaging router exited before session was ready".to_string(),
                        )
                    })?;
                }
                info!("Messaging session ready. Starting master node...");
            }

            let master_node_future = master_node.start_with_ready(Some(ready_tx));
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

        ServeAsyncHandle::new(future, Some(ready_rx))
    }
}
