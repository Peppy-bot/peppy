//! Node-side daemon-liveness watchdog (the uncatchable-death safety net).
//!
//! The daemon publishes a periodic beat on the [`names::DAEMON_HEARTBEAT`]
//! topic (see `core_node::services::clock::publish_daemon_heartbeat`). This
//! watchdog subscribes and resets a deadline on every beat. If no beat arrives
//! for the configured grace period, the daemon is presumed dead for good and
//! the watchdog cancels the node's shutdown token, so the node tears down via
//! its normal clean-shutdown path instead of lingering as an orphan.
//!
//! A clean ctrl+C / `systemctl stop` of the daemon kills its nodes immediately
//! and never relies on this; this only handles an *uncatchable* daemon death
//! (SIGKILL / OOM / crash) where the daemon runs no cleanup. A daemon that
//! returns within the grace period resumes heart-beating on the same
//! machine-deterministic core-node key, each beat resets the window, and the
//! node survives the restart — which is what lets peer-mode nodes ride out a
//! brief daemon outage.

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use core_node_api::names;

use crate::error::Result;
use crate::messaging::Subscription;
use crate::runtime::{CancellationToken, NodeRunner, TaskHandle, spawn};

/// Abstraction over "wait for the next daemon beat" so the watchdog timing can
/// be unit-tested with paused time and no live messaging subscription.
trait BeatSource {
    /// Resolves when the next beat arrives, or `None` if the source closed.
    /// `Send` so the watchdog future stays spawnable.
    fn next_beat(&mut self) -> impl Future<Output = Option<()>> + Send;
}

impl BeatSource for Subscription {
    async fn next_beat(&mut self) -> Option<()> {
        self.on_next_message().await.map(|_| ())
    }
}

/// Subscribe to the daemon heartbeat and spawn the watchdog loop. Returns the
/// task handle so the caller can hold it for the node's lifetime. The
/// subscription is keyed the same way as the clock's, so the watchdog observes
/// the per-core-node key the daemon publishes on.
pub async fn spawn_daemon_watchdog(
    node_runner: Arc<NodeRunner>,
    grace: Duration,
    cancellation_token: CancellationToken,
) -> Result<TaskHandle<Result<()>>> {
    let subscription =
        crate::core_node::subscribe_core_topic(&node_runner, names::DAEMON_HEARTBEAT).await?;
    Ok(spawn(run_watchdog(subscription, grace, cancellation_token)))
}

/// Testable core: wait for the next beat, bounded by `grace`. Each beat resets
/// the window; a `grace`-long silence cancels the token (triggering the node's
/// clean shutdown) and exits. Also exits promptly if some other path cancels
/// the token (e.g. an explicit `SHUTDOWN_SERVICE`) so the task does not linger.
/// Returns `Ok(())` in every case — the loop ending is not itself an error.
async fn run_watchdog<S: BeatSource>(
    mut source: S,
    grace: Duration,
    cancellation_token: CancellationToken,
) -> Result<()> {
    loop {
        tokio::select! {
            biased;
            // Another shutdown path already fired; stop watching.
            _ = cancellation_token.cancelled() => break,
            res = tokio::time::timeout(grace, source.next_beat()) => {
                match res {
                    // A beat arrived: the window resets on the next iteration.
                    Ok(Some(())) => {}
                    // Source closed (session gone); let other paths handle it.
                    Ok(None) => break,
                    // No beat for the whole grace period: presume the daemon is
                    // gone for good and trigger a clean node shutdown.
                    Err(_elapsed) => {
                        tracing::warn!(
                            "No daemon heartbeat for {grace:?}; shutting down node to avoid \
                             orphaning after an uncatchable daemon death"
                        );
                        cancellation_token.cancel();
                        break;
                    }
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A source that never yields a beat: `next_beat` is pending forever, so
    /// only the grace timeout can fire.
    struct NeverBeats;
    impl BeatSource for NeverBeats {
        async fn next_beat(&mut self) -> Option<()> {
            std::future::pending().await
        }
    }

    /// A source that yields a beat every `every`.
    struct PeriodicBeats {
        every: Duration,
    }
    impl BeatSource for PeriodicBeats {
        async fn next_beat(&mut self) -> Option<()> {
            tokio::time::sleep(self.every).await;
            Some(())
        }
    }

    /// With no beats, the watchdog cancels the token — but not before the grace
    /// period elapses (a beat-less node must wait the full window).
    #[tokio::test(start_paused = true)]
    async fn fires_after_grace_with_no_beats() {
        let token = CancellationToken::new();
        let grace = Duration::from_secs(120);
        let handle = spawn(run_watchdog(NeverBeats, grace, token.clone()));

        tokio::time::advance(Duration::from_secs(119)).await;
        assert!(
            !token.is_cancelled(),
            "watchdog fired before the grace period"
        );

        tokio::time::advance(Duration::from_secs(2)).await;
        handle.await.expect("join").expect("watchdog ok");
        assert!(
            token.is_cancelled(),
            "watchdog did not fire after the grace period"
        );
    }

    /// While beats keep arriving inside the window, the watchdog never fires,
    /// even across many grace periods' worth of time.
    #[tokio::test(start_paused = true)]
    async fn never_fires_while_beats_arrive() {
        let token = CancellationToken::new();
        let grace = Duration::from_secs(120);
        // Beat well inside the window.
        let handle = spawn(run_watchdog(
            PeriodicBeats {
                every: Duration::from_secs(5),
            },
            grace,
            token.clone(),
        ));

        // Advance far past several grace periods; steady beats keep it alive.
        for _ in 0..100 {
            tokio::time::advance(Duration::from_secs(5)).await;
            tokio::task::yield_now().await;
        }
        assert!(
            !token.is_cancelled(),
            "watchdog fired despite a steady heartbeat"
        );

        // Stop the loop so the test ends cleanly.
        token.cancel();
        handle.await.expect("join").expect("watchdog ok");
    }

    /// If another path cancels the token, the watchdog exits immediately rather
    /// than waiting out the (here, very long) grace period.
    #[tokio::test(start_paused = true)]
    async fn exits_promptly_when_token_cancelled_elsewhere() {
        let token = CancellationToken::new();
        let handle = spawn(run_watchdog(
            NeverBeats,
            Duration::from_secs(3600),
            token.clone(),
        ));

        token.cancel();
        handle.await.expect("join").expect("watchdog ok");
    }
}
