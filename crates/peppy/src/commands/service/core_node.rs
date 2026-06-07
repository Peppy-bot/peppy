use super::serve::{ServeAsyncCommand, ServeAsyncHandle};
use crate::error::Error;
use config::consts::PeppyDirs;
use core_node::{CoreNode, CoreNodeArguments};
use pmi::Messenger;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{Mutex, oneshot, watch};
use tracing::info;

/// Cadence of the per-node health monitor (see
/// `core_node::services::node::run::spawn_health_monitor`). The monitor probes
/// each running instance every [`HEALTH_MONITOR_INTERVAL`], allowing
/// [`HEALTH_MONITOR_TIMEOUT`] per probe, and flips the instance's health flag
/// (surfaced by `stack list` and `node info`). A failing probe marks the
/// instance unhealthy; a later passing one marks it healthy again. The monitor
/// never removes an instance, so a transient router hang shows up as a brief
/// unhealthy blip rather than tearing the stack down.
pub(crate) const HEALTH_MONITOR_INTERVAL: Duration = Duration::from_secs(5);
pub(crate) const HEALTH_MONITOR_TIMEOUT: Duration = Duration::from_secs(3);

pub struct CoreNodeRunner {
    core_node: CoreNode,
    messaging_ready: Option<watch::Receiver<bool>>,
}

impl CoreNodeRunner {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        messenger: Arc<Mutex<Messenger>>,
        core_node_name: Option<String>,
        node_startup_timeout: Duration,
        node_start_health_timeout: Duration,
        root_dir: PathBuf,
        messaging_ready: Option<watch::Receiver<bool>>,
        clock_source: super::ClockSource,
        peppy_config: config::peppy_config::PeppyConfig,
    ) -> Self {
        let node_arguments = CoreNodeArguments {
            node_startup_timeout,
            node_start_health_timeout,
            health_monitor_interval: HEALTH_MONITOR_INTERVAL,
            health_monitor_timeout: HEALTH_MONITOR_TIMEOUT,
            // 10 Hz: high enough to correlate logs across nodes, low enough to
            // avoid flooding the bus.
            clock_publish_interval: Duration::from_millis(100),
            daemon_use_sim_time: clock_source.use_sim_time(),
        };
        let peppy_dirs = PeppyDirs::default();
        let core_node = CoreNode::new(
            messenger,
            core_node_name.as_deref(),
            node_arguments,
            root_dir,
            peppy_dirs,
            peppy_config,
        );
        Self {
            core_node,
            messaging_ready,
        }
    }

    pub fn node_name(&self) -> &str {
        self.core_node.node_name()
    }
}

impl ServeAsyncCommand for CoreNodeRunner {
    fn run(self: Box<Self>) -> ServeAsyncHandle {
        let (ready_tx, ready_rx) = oneshot::channel();
        let core_node = self.core_node;
        let mut messaging_ready = self.messaging_ready;
        let future = Box::pin(async move {
            let shutdown_signal = tokio::signal::ctrl_c();
            tokio::pin!(shutdown_signal);

            if let Some(mut ready_rx) = messaging_ready.take() {
                if !*ready_rx.borrow() {
                    info!("Waiting for messaging session before starting core node...");
                    ready_rx.changed().await.map_err(|_| {
                        Error::ExecutionFailed(
                            "Messaging router exited before session was ready".to_string(),
                        )
                    })?;
                }
                info!("Messaging session ready. Starting core node...");
            }

            let core_node_future = core_node.start_with_ready(Some(ready_tx));
            tokio::pin!(core_node_future);

            let mut shutdown_triggered = false;
            let result: Result<(), Error> = tokio::select! {
                result = &mut core_node_future => {
                    result.map_err(|err| {
                        Error::ExecutionFailed(format!(
                            "Core node commands listener failed: {}",
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
