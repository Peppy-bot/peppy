use super::serve::{ServeAsyncCommand, ServeAsyncHandle};
use crate::error::Error;
use pmi::{Messenger, MessengerBackend, RouterHealthChecker};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{Mutex, oneshot, watch};
use tracing::{error, info, warn};

/// How often the watchdog probes the router while it appears healthy.
///
/// The probe cadence ([`WATCHDOG_PROBE_INTERVAL`] / [`WATCHDOG_PROBE_TIMEOUT`] /
/// [`WATCHDOG_MAX_FAILURES`]) is tuned so the watchdog detects a wedge, respawns
/// zenohd, and lets node sessions reconnect *before* the core node's health
/// monitor evicts those nodes from the stack — otherwise a transient router
/// hang still tears the stack down even though the watchdog "fixed" the router.
/// The `watchdog_outpaces_health_monitor_eviction` test enforces this against
/// the health-monitor cadence in `core_node.rs`.
const WATCHDOG_PROBE_INTERVAL: Duration = Duration::from_secs(2);
/// Per-probe timeout. A wedged router exceeds any real localhost round-trip.
const WATCHDOG_PROBE_TIMEOUT: Duration = Duration::from_secs(2);
/// Consecutive failed probes before the router is declared wedged.
const WATCHDOG_MAX_FAILURES: u32 = 2;
/// Pause after a restart to let the (reconnecting) sessions re-establish
/// before re-probing.
const WATCHDOG_RESTART_GRACE: Duration = Duration::from_secs(3);
/// Extra backoff after a restart attempt so a persistently-failing router does
/// not spin in a tight restart loop.
const WATCHDOG_POST_RESTART_BACKOFF: Duration = Duration::from_secs(10);

pub struct MessagingRouter {
    messenger: Arc<Mutex<Messenger>>,
    messaging_ready: watch::Sender<bool>,
}

impl MessagingRouter {
    pub fn new(messenger: Arc<Mutex<Messenger>>, messaging_ready: watch::Sender<bool>) -> Self {
        Self {
            messenger,
            messaging_ready,
        }
    }
}

impl ServeAsyncCommand for MessagingRouter {
    fn run(self: Box<Self>) -> ServeAsyncHandle {
        let (ready_tx, ready_rx) = oneshot::channel();
        let messenger = self.messenger;
        let messaging_ready = self.messaging_ready;

        let future = Box::pin(async move {
            {
                let mut messenger = messenger.lock().await;
                info!("Starting the messaging router...");
                messenger
                    .start_router()
                    .await
                    .map_err(Error::PeppyMessagingInterface)?;
                messenger
                    .start_session()
                    .await
                    .map_err(Error::PeppyMessagingInterface)?;
                info!("Messaging session initialized");
            }

            messaging_ready.send(true).ok();
            ready_tx.send(()).ok();

            // Router watchdog: probe the router's liveness and respawn zenohd
            // if it wedges. Backends without a restartable router (the mock)
            // return `None` and just wait for ctrl-c. Either way, ctrl-c ends
            // the wait.
            let health_checker = { messenger.lock().await.router_health_checker() };
            match health_checker {
                Some(checker) => {
                    tokio::select! {
                        // The watchdog loops for the daemon's lifetime; in
                        // practice only ctrl-c resolves this select.
                        _ = run_router_watchdog(&messenger, &checker) => {}
                        res = tokio::signal::ctrl_c() => {
                            res.map_err(|e| {
                                Error::ExecutionFailed(format!("Failed to listen for ctrl-c: {}", e))
                            })?;
                        }
                    }
                }
                None => {
                    tokio::signal::ctrl_c().await.map_err(|e| {
                        Error::ExecutionFailed(format!("Failed to listen for ctrl-c: {}", e))
                    })?;
                }
            }

            {
                let mut messenger = messenger.lock().await;
                info!("Shutting down the messaging router...");
                // Close the client session before killing the router so the
                // session's undeclare-face messages can reach zenohd. Doing
                // it the other way around leaves zenoh spamming
                // "Undefined face context" when the session's lingering
                // Arc clones (publishers, etc.) finally drop and trigger
                // close over a dead transport.
                if let Err(err) = messenger.stop_session().await {
                    tracing::warn!("Failed to stop messaging session cleanly: {err}");
                }
                messenger
                    .stop_router()
                    .await
                    .map_err(Error::PeppyMessagingInterface)?;
            }

            Ok(())
        });

        ServeAsyncHandle::new(future, Some(ready_rx))
    }
}

/// Periodically probes the Zenoh router and respawns zenohd if it stops
/// responding. Loops for the daemon's lifetime. Every restart is announced with
/// a prominent warning banner, and recovery (or continued failure) is reported.
async fn run_router_watchdog(messenger: &Arc<Mutex<Messenger>>, checker: &RouterHealthChecker) {
    let mut consecutive_failures: u32 = 0;
    loop {
        tokio::time::sleep(WATCHDOG_PROBE_INTERVAL).await;

        if checker.is_router_responsive(WATCHDOG_PROBE_TIMEOUT).await {
            consecutive_failures = 0;
            continue;
        }

        consecutive_failures += 1;
        warn!(
            "Zenoh router liveness probe failed ({}/{})",
            consecutive_failures, WATCHDOG_MAX_FAILURES
        );
        if consecutive_failures < WATCHDOG_MAX_FAILURES {
            continue;
        }

        // The router is wedged. Warn loudly *before* touching it, then respawn.
        warn_messaging_restarting(consecutive_failures);

        let restart = {
            let mut messenger = messenger.lock().await;
            // stop_router is best-effort: the old process may already be
            // unresponsive, but we still need its listening port freed.
            if let Err(e) = messenger.stop_router().await {
                warn!("Watchdog: stop_router returned an error (continuing to restart): {e}");
            }
            messenger.start_router().await
        };

        match restart {
            Ok(()) => {
                // Give the daemon's reconnecting session (and any nodes) a
                // moment to re-establish before re-probing.
                tokio::time::sleep(WATCHDOG_RESTART_GRACE).await;
                if checker.is_router_responsive(WATCHDOG_PROBE_TIMEOUT).await {
                    warn_messaging_restarted();
                } else {
                    error!(
                        "Watchdog: Zenoh router still not responding after restart. Will keep \
                         monitoring; a full `peppy service` restart may be required."
                    );
                }
            }
            Err(e) => {
                error!(
                    "Watchdog: failed to restart the Zenoh router: {e}. Will keep monitoring; a \
                     full `peppy service` restart may be required."
                );
            }
        }

        // Reset and back off so a persistently-failing router does not spin in a
        // tight restart loop; the next failure has to accrue from scratch.
        consecutive_failures = 0;
        tokio::time::sleep(WATCHDOG_POST_RESTART_BACKOFF).await;
    }
}

/// Loud, multi-line banner emitted just before the watchdog respawns the router.
fn warn_messaging_restarting(failures: u32) {
    // Rough lower bound on how long the router has been unresponsive.
    let unresponsive_secs =
        ((WATCHDOG_PROBE_INTERVAL + WATCHDOG_PROBE_TIMEOUT) * failures).as_secs();
    error!(
        "\n\
         ====================================================================\n\
         ⚠️  MESSAGING SYSTEM RESTART — the Zenoh router is unresponsive\n\
         ====================================================================\n\
         The Zenoh router (zenohd) failed {failures} consecutive liveness\n\
         probes (no response for ~{unresponsive_secs}s). The peppy daemon is\n\
         RESTARTING the messaging router now to recover the bus.\n\
         \n\
         Impact: in-flight messages were lost. The daemon and node sessions\n\
         reconnect automatically; after recovery check `peppy stack list` and\n\
         relaunch any node that did not rejoin.\n\
         ===================================================================="
    );
}

/// Banner emitted once the respawned router is confirmed responsive again.
fn warn_messaging_restarted() {
    warn!(
        "\n\
         ====================================================================\n\
         ✅  MESSAGING ROUTER RESTARTED — accepting connections again\n\
         ====================================================================\n\
         The Zenoh router was respawned and is responsive again. The daemon and\n\
         node sessions reconnect automatically; relaunch your stack only if\n\
         `peppy stack list` shows a node did not rejoin.\n\
         ===================================================================="
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The router watchdog must detect a wedged router, respawn it, and leave
    /// time for node sessions to reconnect *before* the core node's health
    /// monitor evicts those nodes from the stack. If this inverts, a transient
    /// router hang tears the whole stack down even though the watchdog "fixed"
    /// the router. Both cadences are compile-time constants, so this is a pure
    /// arithmetic guard against a future tweak silently breaking the ordering.
    #[test]
    fn watchdog_outpaces_health_monitor_eviction() {
        use super::super::core_node::{
            HEALTH_MONITOR_INTERVAL, HEALTH_MONITOR_MAX_FAILURES, HEALTH_MONITOR_TIMEOUT,
        };

        // Worst case for the watchdog to declare the router wedged and finish
        // respawning it.
        let watchdog_recovery = (WATCHDOG_PROBE_INTERVAL + WATCHDOG_PROBE_TIMEOUT)
            * WATCHDOG_MAX_FAILURES
            + WATCHDOG_RESTART_GRACE;

        // Earliest the health monitor can evict an instance: the wedge lands
        // just before a scheduled poll, so the first failure costs only the
        // poll timeout and each later failure a full interval + timeout cycle.
        let health_earliest_evict = HEALTH_MONITOR_TIMEOUT
            + (HEALTH_MONITOR_INTERVAL + HEALTH_MONITOR_TIMEOUT)
                * (HEALTH_MONITOR_MAX_FAILURES - 1);

        // Headroom for node sessions to reconnect (zenoh retry backoff ~1-4s)
        // after the router is back but before the health monitor's final poll.
        const NODE_RECONNECT_HEADROOM: Duration = Duration::from_secs(4);

        assert!(
            watchdog_recovery + NODE_RECONNECT_HEADROOM < health_earliest_evict,
            "watchdog recovery ({watchdog_recovery:?}) + reconnect headroom \
             ({NODE_RECONNECT_HEADROOM:?}) must beat the health monitor's earliest \
             eviction ({health_earliest_evict:?})"
        );
    }
}
