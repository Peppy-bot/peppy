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
/// zenohd, and lets node sessions reconnect promptly. While a session is down
/// the core node's health monitor flags the affected nodes unhealthy and clears
/// the flag once they reconnect, so a transient router hang surfaces as a brief
/// unhealthy blip rather than tearing the stack down.
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

/// How long the messaging router keeps the session open waiting for the core
/// node to finish tearing down its nodes (cooperative shutdown rides over that
/// session) before giving up, so a hung teardown cannot wedge the shutdown.
///
/// Sized to the core node's worst-case stop: `force_kill_deadline` (hook grace +
/// event-loop join + interpreter finalize) for a stuck node, plus the reap
/// budget and a one-second margin. Derived from the same `force_kill_deadline`
/// the teardown itself uses so the two cannot drift apart. An earlier regression
/// was exactly that drift: the deadline grew but this budget did not, so the
/// router closed the session out from under a node still stopping over it.
pub(super) fn teardown_budget_for(shutdown_grace_secs: u64) -> Duration {
    core_node::force_kill_deadline(Duration::from_secs(shutdown_grace_secs))
        + core_node::TEARDOWN_REAP_BUDGET
        + Duration::from_secs(1)
}

pub struct MessagingRouter {
    messenger: Arc<Mutex<Messenger>>,
    messaging_ready: watch::Sender<bool>,
    /// Signaled (`true`) by the core node once its teardown has finished. The
    /// router waits on this before closing the session so cooperative node
    /// shutdown (which sends `SHUTDOWN_SERVICE` over the session) can complete.
    /// `None` when the daemon runs without a core node, in which case there is
    /// nothing to wait for.
    core_node_done: Option<watch::Receiver<bool>>,
    /// Upper bound on the wait for `core_node_done`, so a hung teardown cannot
    /// wedge the messaging shutdown. Sized to the core node's worst-case stop
    /// duration: `force_kill_deadline(grace)` (hook grace + event-loop join +
    /// interpreter finalize) plus the reap budget and a small margin.
    teardown_budget: Duration,
}

impl MessagingRouter {
    pub fn new(
        messenger: Arc<Mutex<Messenger>>,
        messaging_ready: watch::Sender<bool>,
        core_node_done: Option<watch::Receiver<bool>>,
        teardown_budget: Duration,
    ) -> Self {
        Self {
            messenger,
            messaging_ready,
            core_node_done,
            teardown_budget,
        }
    }
}

impl ServeAsyncCommand for MessagingRouter {
    fn run(self: Box<Self>) -> ServeAsyncHandle {
        let (ready_tx, ready_rx) = oneshot::channel();
        let messenger = self.messenger;
        let messaging_ready = self.messaging_ready;
        let core_node_done = self.core_node_done;
        let teardown_budget = self.teardown_budget;

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
                        // practice only a shutdown signal resolves this select.
                        _ = run_router_watchdog(&messenger, &checker) => {}
                        res = super::shutdown_signal::shutdown_signal() => {
                            res.map_err(|e| {
                                Error::ExecutionFailed(format!("Failed to listen for shutdown signal: {}", e))
                            })?;
                        }
                    }
                }
                None => {
                    super::shutdown_signal::shutdown_signal()
                        .await
                        .map_err(|e| {
                            Error::ExecutionFailed(format!(
                                "Failed to listen for shutdown signal: {}",
                                e
                            ))
                        })?;
                }
            }

            // Catchable shutdown fired. The core node still needs the session to
            // stop its nodes cooperatively (it sends SHUTDOWN_SERVICE over the
            // session), so wait for it to finish tearing down before closing the
            // session. Bounded by `teardown_budget` so a hung teardown cannot
            // wedge the messaging shutdown.
            if core_node_done.is_some() {
                info!("Waiting for core node teardown before closing the messaging session...");
            }
            if !await_core_node_teardown(core_node_done, teardown_budget).await {
                warn!(
                    "Core node teardown did not signal completion within {:?}; \
                     closing the messaging session anyway",
                    teardown_budget
                );
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

/// Waits (bounded by `teardown_budget`) for the core node to signal that its
/// teardown has finished, so the caller can keep the messaging session open
/// until cooperative node shutdown (which rides over that session) completes.
///
/// Returns `true` when there is nothing left to wait for — either the core node
/// signaled completion, the daemon has no core node (`None`), or the sender was
/// dropped without signaling (the runner is already gone). Returns `false` only
/// when the wait exceeded the budget, so the caller logs and proceeds anyway.
async fn await_core_node_teardown(
    core_node_done: Option<watch::Receiver<bool>>,
    teardown_budget: Duration,
) -> bool {
    let Some(mut done) = core_node_done else {
        return true;
    };
    let wait = async {
        while !*done.borrow() {
            if done.changed().await.is_err() {
                // The core node runner is gone without signaling (it never
                // started, or exited early); stop waiting.
                break;
            }
        }
    };
    tokio::time::timeout(teardown_budget, wait).await.is_ok()
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
    use super::await_core_node_teardown;
    use std::time::Duration;
    use tokio::sync::watch;

    #[tokio::test]
    async fn proceeds_once_core_node_signals_completion() {
        let (done_tx, done_rx) = watch::channel(false);
        let waiter = tokio::spawn(await_core_node_teardown(
            Some(done_rx),
            Duration::from_secs(5),
        ));

        // The core node finishes teardown and signals.
        done_tx
            .send(true)
            .expect("receiver kept alive by the waiter");

        let ready = tokio::time::timeout(Duration::from_secs(1), waiter)
            .await
            .expect("waiter should resolve promptly after the signal")
            .expect("waiter task should not panic");
        assert!(
            ready,
            "a completion signal must count as ready, not a timeout"
        );
    }

    #[tokio::test]
    async fn times_out_when_core_node_never_signals() {
        // Keep the sender alive so the wait blocks on the (never-changing)
        // value rather than resolving early on a dropped sender.
        let (_done_tx, done_rx) = watch::channel(false);
        let ready = await_core_node_teardown(Some(done_rx), Duration::from_millis(50)).await;
        assert!(
            !ready,
            "a hung teardown must surface as a timeout so the router proceeds"
        );
    }

    #[test]
    fn teardown_budget_outlasts_the_daemon_force_kill_window() {
        // The router must not close the session before the core node can finish
        // its worst-case teardown, or a node still stopping cooperatively over
        // that session is cut off and force-killed. Pin the budget strictly above
        // the daemon's force-kill deadline plus its reap budget across every
        // accepted grace (minimum accepted is 1s), so a future change to
        // `force_kill_deadline` cannot silently outgrow this budget again (the
        // original regression was exactly that drift).
        for grace_secs in 1..=600 {
            let budget = super::teardown_budget_for(grace_secs);
            let worst_case = core_node::force_kill_deadline(Duration::from_secs(grace_secs))
                + core_node::TEARDOWN_REAP_BUDGET;
            assert!(
                budget > worst_case,
                "teardown budget {budget:?} for grace {grace_secs}s must exceed the daemon's \
                 worst-case teardown {worst_case:?} (force-kill deadline + reap)",
            );
        }
    }

    #[tokio::test]
    async fn proceeds_immediately_without_a_core_node() {
        let ready = await_core_node_teardown(None, Duration::from_secs(5)).await;
        assert!(ready, "no core node means nothing to wait for");
    }

    #[tokio::test]
    async fn proceeds_when_the_sender_is_dropped_without_signaling() {
        let (done_tx, done_rx) = watch::channel(false);
        drop(done_tx); // runner gone without ever signaling.
        let ready = await_core_node_teardown(Some(done_rx), Duration::from_secs(5)).await;
        assert!(
            ready,
            "a dropped sender means the runner is already gone, not a timeout"
        );
    }
}
