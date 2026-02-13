use super::serve::{ServeAsyncCommand, ServeAsyncHandle};
use crate::error::Error;
use daemon_node::{DaemonNode, DaemonNodeArguments};
use pmi::Messenger;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{Mutex, oneshot, watch};
use tracing::info;

pub struct DaemonNodeRunner {
    daemon_node: DaemonNode,
    messaging_ready: Option<watch::Receiver<bool>>,
}

impl DaemonNodeRunner {
    pub fn new(
        messenger: Arc<Mutex<Messenger>>,
        daemon_name: Option<String>,
        node_startup_timeout: Duration,
        node_start_health_timeout: Duration,
        root_dir: PathBuf,
        messaging_ready: Option<watch::Receiver<bool>>,
    ) -> Self {
        let node_arguments = DaemonNodeArguments {
            node_startup_timeout,
            node_start_health_timeout,
        };
        let daemon_node =
            DaemonNode::new(messenger, daemon_name.as_deref(), node_arguments, root_dir);
        Self {
            daemon_node,
            messaging_ready,
        }
    }

    pub fn node_name(&self) -> &str {
        self.daemon_node.node_name()
    }
}

impl ServeAsyncCommand for DaemonNodeRunner {
    fn run(self: Box<Self>) -> ServeAsyncHandle {
        let (ready_tx, ready_rx) = oneshot::channel();
        let daemon_node = self.daemon_node;
        let mut messaging_ready = self.messaging_ready;
        let future = Box::pin(async move {
            let shutdown_signal = tokio::signal::ctrl_c();
            tokio::pin!(shutdown_signal);

            if let Some(mut ready_rx) = messaging_ready.take() {
                if !*ready_rx.borrow() {
                    info!("Waiting for messaging session before starting daemon node...");
                    ready_rx.changed().await.map_err(|_| {
                        Error::ExecutionFailed(
                            "Messaging router exited before session was ready".to_string(),
                        )
                    })?;
                }
                info!("Messaging session ready. Starting daemon node...");
            }

            let daemon_node_future = daemon_node.start_with_ready(Some(ready_tx));
            tokio::pin!(daemon_node_future);

            let mut shutdown_triggered = false;
            let result: Result<(), Error> = tokio::select! {
                result = &mut daemon_node_future => {
                    result.map_err(|err| {
                        Error::ExecutionFailed(format!(
                            "Daemon node commands listener failed: {}",
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
