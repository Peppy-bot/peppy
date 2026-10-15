use crate::error::Error;
use crate::log_export::LogExport;
use crate::messaging_router::WATCHDOG_RECOVERY;
use crate::serve::{ServeAsyncCommand, ServeAsyncHandle};
use core_node::{CoreNode, CoreNodeArguments, CoreNodeConfig, HealthMonitorPolicy, RestartBackoff};
use daemon_config::consts::PeppyDirs;
use pmi::Messenger;
use std::num::NonZeroU32;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{Mutex, oneshot, watch};
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

/// How long an instance may answer no health check before the daemon kills
/// it, under a policy that restarts after a failure.
const KILL_AFTER_UNANSWERED: Duration = Duration::from_secs(60);

/// Cadence and tolerance of the per-node health monitor (see
/// `core_node::services::node::health_monitor`). The monitor probes each
/// running instance every `interval`, allowing `timeout` per probe, and flips
/// the instance's health flag (surfaced by `stack list` and `node info`) only
/// after `failure_threshold` consecutive missed probes; one passing probe flips
/// it back. A lone missed probe is what a node, its container, or the host
/// produces when it stalls for a few seconds (a swapped-out VM, a saturated
/// CPU), and it leaves the flag untouched. A node that stays silent for the
/// full run of misses, about twenty seconds, is what turns `unhealthy`. Under
/// a restart policy that restarts on failure, an instance that has answered
/// no health check for [`KILL_AFTER_UNANSWERED`] is killed at its next miss,
/// and its exit counts as a failure: a silent node is killed 60 to 68 s after
/// the check it last answered, the minute and then the probe in flight, 8 s
/// at most (the 5 s interval and the 3 s timeout); it went quiet up to one
/// probe cycle (8 s) after that answer. The silence is measured the same way
/// whether probes time out or fail at once, so the minute outlasts
/// [`WATCHDOG_RECOVERY`] (45.5 s), the longest outage one restart of the
/// router watchdog heals, with the probes in flight when the router dies and
/// when it comes back (11 s at most), in either case. The kill is held while two
/// or more instances other than the core node's own run on the daemon and none
/// of them is healthy, which is a messaging outage; a lone silent instance is
/// killed.
const HEALTH_MONITOR: HealthMonitorPolicy = HealthMonitorPolicy {
    interval: Duration::from_secs(5),
    timeout: Duration::from_secs(3),
    failure_threshold: NonZeroU32::new(3).unwrap(),
    kill_after: Some(KILL_AFTER_UNANSWERED),
};

// The kill follows the unhealthy edge (`spawn_health_monitor` asserts the
// same for an injected policy), and the silence that kills outlasts the
// router watchdog's worst-case recovery with the probes in flight when the
// router dies (the silence began up to one probe cycle before the outage)
// and when it comes back (that probe can still fail).
const _: () = assert!(
    KILL_AFTER_UNANSWERED.as_millis()
        > (HEALTH_MONITOR.interval.as_millis() + HEALTH_MONITOR.timeout.as_millis())
            * HEALTH_MONITOR.failure_threshold.get() as u128
);
const _: () = assert!(
    KILL_AFTER_UNANSWERED.as_millis()
        > WATCHDOG_RECOVERY.as_millis()
            + HEALTH_MONITOR.interval.as_millis()
            + HEALTH_MONITOR.timeout.as_millis()
            + HEALTH_MONITOR.timeout.as_millis()
);

/// The backoff of an instance's restarts: one second between an exit
/// and the next start, doubling with each exit or failed start up to a
/// minute, and back to one second once the instance has run for two minutes
/// (a run the health monitor ends counts up to its last answered health
/// check).
const RESTART_BACKOFF: RestartBackoff = RestartBackoff {
    first_delay: Duration::from_secs(1),
    max_delay: Duration::from_secs(60),
    reset_after: Duration::from_secs(120),
};

/// Cadence of the daemon-liveness heartbeat each spawned node's watchdog
/// listens for. Small and fixed; the configurable grace period
/// (`peppy_config.lifecycle.daemon_grace_secs`) is many multiples of it, so a
/// couple of dropped beats never trip a node's watchdog. The seconds value is
/// defined next to the grace-period floor in `config::peppy_config`, where a
/// compile-time assert enforces that margin.
const DAEMON_HEARTBEAT_INTERVAL: Duration =
    Duration::from_secs(daemon_config::peppy_config::DAEMON_HEARTBEAT_INTERVAL_SECS);

/// Bound on the wait for the managed router's configured `connect` links
/// (the project's cloud router when enrolled, an operator-pinned federation) to establish before
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
    messaging_ready: watch::Receiver<bool>,
    /// Cancelled at the start of shutdown to stop the core node's clock +
    /// heartbeat publishers before the messaging session is closed. This is the
    /// core node's OWN internal token (publisher-stop), distinct from the shared
    /// serve coordinator token below.
    shutdown_token: CancellationToken,
    /// Shared serve coordinator token: the runner begins teardown when it is
    /// cancelled. The coordinator cancels it when the generation stops for any
    /// reason. Distinct from `shutdown_token`, which only stops the publishers.
    serve_teardown_token: CancellationToken,
    /// Signaled (`true`) once node teardown has finished, releasing the
    /// messaging router to close the session. The startup `messaging_ready`
    /// watch's shutdown-side counterpart.
    core_node_done: watch::Sender<bool>,
    /// The export of the core node's log files, started with the core node
    /// and stopped once its teardown has written its last lines.
    log_export: LogExport,
}

impl CoreNodeRunner {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        messenger: Arc<Mutex<Messenger>>,
        core_node_name: Option<String>,
        node_startup_timeout: Duration,
        default_setup_timeout: config::node::SetupTimeout,
        root_dir: PathBuf,
        peppy_dirs: PeppyDirs,
        messaging_ready: watch::Receiver<bool>,
        peppy_config: daemon_config::peppy_config::PeppyConfig,
        namespace: config::namespace::Namespace,
        name_claim_settle: Duration,
        serve_teardown_token: CancellationToken,
        core_node_done: watch::Sender<bool>,
        log_export: LogExport,
    ) -> Self {
        let node_arguments = CoreNodeArguments {
            node_startup_timeout,
            default_setup_timeout,
            health_monitor: HEALTH_MONITOR,
            restart_backoff: RESTART_BACKOFF,
            // 10 Hz: high enough to correlate logs across nodes, low enough to
            // avoid flooding the bus.
            clock_publish_interval: Duration::from_millis(100),
            heartbeat_interval: DAEMON_HEARTBEAT_INTERVAL,
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
            host_addresses: None,
            root_dir,
            peppy_dirs,
            peppy_config,
            namespace,
            shutdown_token: shutdown_token.clone(),
            log_exporter: log_export.exporter(),
        });
        Self {
            core_node,
            messenger,
            messaging_ready,
            shutdown_token,
            serve_teardown_token,
            core_node_done,
            log_export,
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
        let shutdown_token = self.shutdown_token;
        let serve_teardown_token = self.serve_teardown_token;
        let core_node_done = self.core_node_done;
        let log_export = self.log_export;
        let future = Box::pin(async move {
            // Tear down when the serve coordinator stops the generation.
            let teardown = serve_teardown_token.cancelled();
            tokio::pin!(teardown);

            if !*messaging_ready.borrow() {
                info!("Waiting for messaging session before starting core node...");
                messaging_ready.changed().await.map_err(|_| {
                    Error::ExecutionFailed(
                        "Messaging router exited before session was ready".to_string(),
                    )
                })?;
            }
            info!("Messaging session ready. Starting core node...");

            // A managed router accepts the daemon's session as soon as its
            // listener binds, while its configured `connect` links (the
            // project's cloud router when enrolled, an operator-pinned
            // federation) are still being dialed. The boot presence check
            // queries liveliness once, so run it only after those links
            // established or a same-name daemon behind them is invisible to the
            // check and the collision is missed. Bounded and fail-open: an
            // unreachable peer degrades to a standalone-looking boot, covered
            // by the runtime collision watch.
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

            let log_export = log_export.start(core_node.node_name());
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
            }
            // Offboard when the generation is stopping (a shutdown signal, an
            // in-process restart, or a failed sibling handler) and when the
            // core node failed while running (a listener task died with an
            // error) and the daemon is about to exit on it.
            if shutdown_triggered || result.is_err() {
                // Stop the daemon's own clock + heartbeat publishers first, so
                // they don't spin against the session once the messaging router
                // closes it (logging a failed publish on every tick).
                shutdown_token.cancel();
                // Tear down every spawned node now (cooperatively, then
                // force-kill any straggler's process group) so none is left
                // orphaned. `&mut core_node_future` still holds a shared borrow
                // of `core_node`; `teardown_node_stack` also takes `&self`, so
                // this second shared borrow is fine. Runs before this handler
                // returns, so the kills complete before the daemon process exits.
                // The cooperative stop sends SHUTDOWN_SERVICE over the messaging
                // session, so the session must still be open here; the router
                // waits for the `core_node_done` signal below before closing it.
                core_node.teardown_node_stack().await;
                // Withdraw this daemon's own presence while the session and the
                // router are still up, so a managed router can propagate the
                // undeclare to its upstream. Dropping `CoreNode` would also
                // release the token, but that happens after the router has
                // closed the session, leaving an upstream advertising a core
                // node that has exited.
                core_node.release_presence();
                // Release the messaging router to close the session now that the
                // core node no longer needs it.
                let _ = core_node_done.send(true);
            }
            // The teardown has written its last lines by now.
            log_export.stop().await;

            result
        });

        ServeAsyncHandle::new(future, Some(ready_rx))
    }
}

#[cfg(test)]
mod tests {
    use super::{HEALTH_MONITOR, KILL_AFTER_UNANSWERED, RESTART_BACKOFF, WATCHDOG_RECOVERY};
    use std::time::Duration;

    /// The attempt and the wait the shipped backoff gives after `previous`
    /// attempts and a run of `ran_for`.
    fn next(previous: u32, ran_for: Option<Duration>) -> (u32, Duration) {
        let next = RESTART_BACKOFF.next(previous, ran_for);
        (next.attempt.get(), next.delay)
    }

    /// The backoff the daemon ships: 1 s doubling to a minute, then a minute
    /// per attempt, and back to 1 s after a two-minute run.
    #[test]
    fn the_shipped_backoff_doubles_to_a_minute_and_resets_after_two() {
        assert_eq!(next(0, None), (1, Duration::from_secs(1)));
        assert_eq!(
            next(7, Some(Duration::from_secs(119))),
            (8, Duration::from_secs(60)),
            "a run just short of two minutes backs off further"
        );
        assert_eq!(
            next(7, Some(Duration::from_secs(120))),
            (1, Duration::from_secs(1))
        );
    }

    /// A silent node is killed 60 to 68 s after the check it last answered,
    /// the minute and the probe in flight; the minute outlasts the router
    /// watchdog's 45.5 s with the probes in flight when the router dies and
    /// comes back (11 s); the kill's SIGKILL follows its SIGTERM by 12 s at
    /// the default grace.
    #[test]
    fn the_shipped_kill_window_outlasts_the_router_recovery() {
        assert_eq!(KILL_AFTER_UNANSWERED, Duration::from_secs(60));
        let probe_to_probe = HEALTH_MONITOR.interval + HEALTH_MONITOR.timeout;
        assert_eq!(
            KILL_AFTER_UNANSWERED + probe_to_probe,
            Duration::from_secs(68),
            "a silent node is killed at its first miss past the minute, within one probe cycle"
        );
        assert_eq!(WATCHDOG_RECOVERY, Duration::from_millis(45_500));
        assert_eq!(
            core_node::force_kill_deadline(Duration::from_secs(
                config::peppy_config::DEFAULT_SHUTDOWN_GRACE_SECS
            )),
            Duration::from_secs(12)
        );
    }
}
