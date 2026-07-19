use crate::error::Error;
use crate::serve::{ServeAsyncCommand, ServeAsyncHandle};
use core_node::{CoreNode, CoreNodeArguments, CoreNodeConfig};
use daemon_config::consts::PeppyDirs;
use pmi::Messenger;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{Mutex, oneshot, watch};
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

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

/// Cadence of the daemon-liveness heartbeat each spawned node's watchdog
/// listens for. Small and fixed; the configurable grace period
/// (`peppy_config.lifecycle.daemon_grace_secs`) is many multiples of it, so a
/// couple of dropped beats never trip a node's watchdog. The seconds value is
/// defined next to the grace-period floor in `config::peppy_config`, where a
/// compile-time assert enforces that margin.
pub(crate) const DAEMON_HEARTBEAT_INTERVAL: Duration =
    Duration::from_secs(daemon_config::peppy_config::DAEMON_HEARTBEAT_INTERVAL_SECS);

/// Bound on the wait for the managed router's configured `connect` links
/// (operator-pinned federation, an applied cloud upstream) to establish before
/// the boot-time presence check. Link formation is normally tens of
/// milliseconds; the bound only caps the wait when a configured peer is down
/// or unresolvable, where the check proceeds standalone (fail-open) and the
/// runtime collision watch covers a peer that federates in later. Generous
/// enough to ride out zenohd's first connect-retry backoff (~1s) a few times
/// over.
const ROUTER_LINKS_SETTLE_TIMEOUT: Duration = Duration::from_secs(5);

pub struct CoreNodeRunner {
    core_node: CoreNode,
    /// The daemon's shared messenger, retained (alongside the clone moved into
    /// `core_node`) to build the router-links probe: the bounded wait for the
    /// managed router's configured `connect` links to establish before the
    /// boot presence check. Probing is lock-free; the messenger lock is held
    /// only to construct the probe.
    messenger: Arc<Mutex<Messenger>>,
    messaging_ready: Option<watch::Receiver<bool>>,
    /// Goes `true` once the [`RouterFederation`](super::router_federation) task's
    /// *initial* poll has settled (federation applied, or the connect timeout
    /// elapsed and the daemon proceeds standalone). The runner waits on it —
    /// after `messaging_ready`, before `start_with_ready` — so the boot-time
    /// core-node presence check sees the federated mesh, not the
    /// always-standalone just-started router. Bounded by the
    /// federation task's own `connect_timeout` and fail-open: a dropped sender
    /// (federation task torn down) lets the core node proceed. `None` when no
    /// federation task is armed (mock engine / no backend configured).
    federation_settled: Option<watch::Receiver<bool>>,
    /// Cancelled at the start of shutdown to stop the core node's clock +
    /// heartbeat publishers before the messaging session is closed. This is the
    /// core node's OWN internal token (publisher-stop), distinct from the shared
    /// serve coordinator token below.
    shutdown_token: CancellationToken,
    /// Shared serve coordinator token: the runner observes it to begin teardown
    /// on either a real OS shutdown signal or an in-process restart. Distinct
    /// from `shutdown_token`, which only stops the publishers.
    serve_teardown_token: CancellationToken,
    /// Signaled (`true`) once node teardown has finished, releasing the
    /// messaging router to close the session. The startup `messaging_ready`
    /// watch's shutdown-side counterpart.
    core_node_done: watch::Sender<bool>,
}

impl CoreNodeRunner {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        messenger: Arc<Mutex<Messenger>>,
        core_node_name: Option<String>,
        node_startup_timeout: Duration,
        node_start_health_timeout: Duration,
        root_dir: PathBuf,
        peppy_dirs: PeppyDirs,
        messaging_ready: Option<watch::Receiver<bool>>,
        federation_settled: Option<watch::Receiver<bool>>,
        clock_source: crate::ClockSource,
        peppy_config: daemon_config::peppy_config::PeppyConfig,
        namespace: config::namespace::Namespace,
        name_claim_settle: Duration,
        serve_teardown_token: CancellationToken,
        core_node_done: watch::Sender<bool>,
    ) -> Self {
        let node_arguments = CoreNodeArguments {
            node_startup_timeout,
            node_start_health_timeout,
            health_monitor_interval: HEALTH_MONITOR_INTERVAL,
            health_monitor_timeout: HEALTH_MONITOR_TIMEOUT,
            // 10 Hz: high enough to correlate logs across nodes, low enough to
            // avoid flooding the bus.
            clock_publish_interval: Duration::from_millis(100),
            heartbeat_interval: DAEMON_HEARTBEAT_INTERVAL,
            daemon_use_sim_time: clock_source.use_sim_time(),
            name_claim_settle,
        };
        // Fail fast with a clean operator-facing message (no backtrace) when a
        // runtime prerequisite is missing. The library reports this as an error
        // rather than calling `std::process::exit`, so the binary owns the exit.
        if let Err(e) = core_node::check_runtime_prerequisites() {
            eprintln!("{e}");
            std::process::exit(1);
        }
        // The publishers select on this token; the runner cancels it at the
        // start of shutdown so they stop before the session is closed.
        let shutdown_token = CancellationToken::new();
        let core_node = CoreNode::new(CoreNodeConfig {
            messenger: Arc::clone(&messenger),
            node_name: core_node_name,
            arguments: node_arguments,
            root_dir,
            peppy_dirs,
            peppy_config,
            namespace,
            shutdown_token: shutdown_token.clone(),
        });
        Self {
            core_node,
            messenger,
            messaging_ready,
            federation_settled,
            shutdown_token,
            serve_teardown_token,
            core_node_done,
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
        let messenger = self.messenger;
        let mut messaging_ready = self.messaging_ready;
        let mut federation_settled = self.federation_settled;
        let shutdown_token = self.shutdown_token;
        let serve_teardown_token = self.serve_teardown_token;
        let core_node_done = self.core_node_done;
        let future = Box::pin(async move {
            // Tear down on a real OS shutdown signal OR an in-process restart
            // (the shared serve coordinator token).
            let teardown = crate::shutdown_signal::shutdown_or_token(&serve_teardown_token);
            tokio::pin!(teardown);

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

            // Wait for the router federation's initial poll to settle before
            // starting (and thus before `start_with_ready` checks and declares
            // this daemon's presence), so the check sees the federated mesh: a
            // same-name daemon reachable only through the platform router
            // must refuse boot, not slip past a check that raced the
            // federation apply. The gate is fired by the federation task
            // within its `connect_timeout` even when the backend is slow or
            // unreachable, so this cannot stall boot past that bound; a
            // closed channel (the federation task tore down first) fails
            // open to a standalone start.
            if let Some(mut settled_rx) = federation_settled.take()
                && !*settled_rx.borrow()
            {
                info!(
                    "Waiting for the initial router federation to settle before \
                     the core-node presence check..."
                );
                if settled_rx.wait_for(|settled| *settled).await.is_err() {
                    warn!(
                        "Router federation task exited before its initial poll \
                         settled; starting the core node against the standalone \
                         router"
                    );
                }
            }

            // A managed router accepts the daemon's session as soon as its
            // listener binds, while its configured `connect` links (an
            // operator-pinned federation, the upstream a federation apply just
            // wrote) are still being dialed. The boot presence check queries
            // liveliness once, so run it only after those links established or
            // a same-name daemon behind them is invisible to the check and the
            // collision is missed. Runs after the federation gate above so an
            // applied upstream is already in the router config it reads.
            // Bounded and fail-open: an unreachable peer degrades to a
            // standalone-looking boot, covered by the runtime collision watch.
            let links_probe = { messenger.lock().await.router_links_probe() };
            if let Some(probe) = links_probe {
                let endpoints = probe.endpoints().join(", ");
                info!(
                    "Waiting for the managed router's configured links \
                     ({endpoints}) before the core-node presence check..."
                );
                if !probe.wait_established(ROUTER_LINKS_SETTLE_TIMEOUT).await {
                    warn!(
                        "Configured router links ({endpoints}) did not all \
                         establish within {ROUTER_LINKS_SETTLE_TIMEOUT:?}; \
                         running the core-node presence check against the \
                         links that are up"
                    );
                }
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
                _ = &mut teardown => {
                    shutdown_triggered = true;
                    Ok(())
                }
            };

            if shutdown_triggered {
                info!("Shutting down commands listener...");
                // Stop the daemon's own clock + heartbeat publishers first, so
                // they don't spin against the session once the messaging router
                // closes it (logging a failed publish on every tick).
                shutdown_token.cancel();
                // Catchable shutdown (ctrl+C / SIGTERM): the daemon is exiting,
                // so tear down every spawned node now (cooperatively, then
                // force-kill any straggler's process group) so none is left
                // orphaned. `&mut core_node_future` still holds a shared borrow
                // of `core_node`; `teardown_node_stack` also takes `&self`, so
                // this second shared borrow is fine. Runs before this handler
                // returns, so the kills complete before the daemon process exits.
                // The cooperative stop sends SHUTDOWN_SERVICE over the messaging
                // session, so the session must still be open here; the router
                // waits for the `core_node_done` signal below before closing it.
                core_node.teardown_node_stack().await;
                // Release the messaging router to close the session now that the
                // core node no longer needs it.
                let _ = core_node_done.send(true);
            } else if result.is_err() {
                // The core node failed while running (a listener task died
                // with an error) and the daemon is about to exit on it. Same
                // offboarding as the signal path above: stop the publishers,
                // tear down any node spawned in the meantime, and release the
                // messaging router.
                shutdown_token.cancel();
                core_node.teardown_node_stack().await;
                let _ = core_node_done.send(true);
            }

            result
        });

        ServeAsyncHandle::new(future, Some(ready_rx))
    }
}
