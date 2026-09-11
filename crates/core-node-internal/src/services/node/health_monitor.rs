//! Background liveness monitor for a running node instance.
//!
//! The core node probes every running instance's `node_health` service on a
//! fixed cadence and folds the outcomes into the instance's health flag, which
//! `stack list` and `node info` surface. The flag tracks sustained state, not a
//! single probe: an instance turns `unhealthy` only after
//! [`HealthMonitorPolicy::failure_threshold`] consecutive missed probes and
//! turns `healthy` again on the first passing one. A lone missed probe (the
//! node, its container, or the messaging path stalling for a moment) leaves the
//! flag untouched and is visible only at debug level.

use super::append_stack_log;
use config::runtime::Name;
use daemon_config::consts::PeppyDirs;
use node_stack::NodeStack;
use peppylib::encoding::health::NodeHealthRequest;
use peppylib::messaging::{NODE_HEALTH_SERVICE, ProducerRef, SenderTarget, ServiceTarget};
use peppylib::{MessengerHandle, ServiceMessenger};
use std::num::NonZeroU32;
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;
use tracing::debug;

/// Cadence and tolerance of the per-instance health monitor.
#[derive(Clone, Copy, Debug)]
pub struct HealthMonitorPolicy {
    /// Pause between two probes of the same instance.
    pub interval: Duration,
    /// Budget for one probe. A probe with no reply inside it is a miss.
    pub timeout: Duration,
    /// Consecutive misses that turn a healthy instance `unhealthy`. One passing
    /// probe turns it back.
    pub failure_threshold: NonZeroU32,
}

/// A change of an instance's health caused by a probe outcome.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum HealthEdge {
    /// The miss that reached the failure threshold.
    Down,
    /// The first pass after the instance turned `unhealthy`.
    Up,
}

/// Folds probe outcomes into an instance's health.
///
/// Misses are counted while they are consecutive and the count resets on the
/// first pass. The health changes only on the miss that reaches the threshold
/// and on the pass that follows a run of misses, so a caller logs and records
/// exactly one event per transition.
#[derive(Debug)]
pub(crate) struct HealthTracker {
    failure_threshold: NonZeroU32,
    consecutive_misses: u32,
    healthy: bool,
}

impl HealthTracker {
    /// A tracker for an instance that starts out healthy, matching a fresh
    /// instance's flag.
    pub(crate) fn new(failure_threshold: NonZeroU32) -> Self {
        Self {
            failure_threshold,
            consecutive_misses: 0,
            healthy: true,
        }
    }

    pub(crate) fn healthy(&self) -> bool {
        self.healthy
    }

    /// Misses recorded since the last pass.
    pub(crate) fn consecutive_misses(&self) -> u32 {
        self.consecutive_misses
    }

    /// Folds one probe outcome in and reports the health edge it caused.
    pub(crate) fn record(&mut self, probe_passed: bool) -> Option<HealthEdge> {
        if probe_passed {
            self.consecutive_misses = 0;
            if self.healthy {
                return None;
            }
            self.healthy = true;
            return Some(HealthEdge::Up);
        }

        self.consecutive_misses = self.consecutive_misses.saturating_add(1);
        if !self.healthy || self.consecutive_misses < self.failure_threshold.get() {
            return None;
        }
        self.healthy = false;
        Some(HealthEdge::Down)
    }
}

pub(crate) struct HealthMonitorParams {
    pub(crate) messenger: MessengerHandle,
    pub(crate) core_node_name: String,
    pub(crate) caller_instance_id: String,
    pub(crate) to_node_name: String,
    pub(crate) target_core_node: String,
    pub(crate) target_instance_id: Name,
    pub(crate) node_tag: String,
    pub(crate) node_stack: Arc<NodeStack>,
    pub(crate) peppy_dirs: PeppyDirs,
    pub(crate) policy: HealthMonitorPolicy,
    pub(crate) shutdown_token: CancellationToken,
    /// Cancelled by the instance's exit watcher once its process exits on its
    /// own, so the monitor stops the moment the instance goes terminal rather
    /// than running one more probe (which would fail against the dead process
    /// and count a miss for a node that simply finished).
    pub(crate) instance_done: CancellationToken,
}

/// Spawns a background task that periodically polls the node's health service
/// and records the resulting health on the instance's flag, which `stack list`
/// and `node info` surface. The flag follows a [`HealthTracker`]: it turns
/// `unhealthy` after [`HealthMonitorPolicy::failure_threshold`] consecutive
/// missed probes and `healthy` again on the next passing probe. This task never
/// removes the instance from the stack, so an unhealthy node stays visible until
/// it recovers or is stopped explicitly (e.g. `node stop`).
///
/// The task exits when the instance is no longer found in the stack (stopped
/// externally), when `instance_done` is cancelled (the instance's process
/// exited on its own and the exit watcher has moved it to a terminal state), or
/// when `shutdown_token` is cancelled (the daemon is shutting down, so the
/// monitored nodes are being torn down on purpose and must not be reported
/// unhealthy for it).
pub(crate) fn spawn_health_monitor(p: HealthMonitorParams) {
    tokio::spawn(async move {
        let instance_id_str = p.target_instance_id.as_str().to_owned();
        let request_payload = match NodeHealthRequest::new().encode() {
            Ok(payload) => payload,
            Err(e) => {
                tracing::warn!(
                    "Health monitor for '{}' failed to encode request: {}",
                    instance_id_str,
                    e
                );
                return;
            }
        };

        let mut tracker = HealthTracker::new(p.policy.failure_threshold);

        loop {
            // Wait out the probe interval, but bail the instant the daemon starts
            // shutting down: probing nodes that are intentionally being torn down
            // would log spurious "unhealthy" / "Session not initialized" warnings
            // for the whole teardown window.
            tokio::select! {
                _ = p.shutdown_token.cancelled() => return,
                _ = p.instance_done.cancelled() => return,
                _ = tokio::time::sleep(p.policy.interval) => {}
            }

            // Resolve the monitored instance once per tick. If it was removed
            // externally (e.g. user ran `node stop`), our job is done: skip the
            // poll and exit. The returned clone shares the instance's health
            // flag (an `Arc<AtomicBool>`), so recording the probe result on it
            // after the poll still updates the tracked instance even though it
            // was resolved beforehand. Should the instance be removed during the
            // poll, that write lands on a now-detached flag no reader can reach,
            // so it is harmless.
            let Some(instance) = p.node_stack.find_by_instance_id(&p.target_instance_id) else {
                debug!(
                    "Health monitor: instance '{}' no longer in stack, exiting",
                    instance_id_str
                );
                return;
            };

            // Bound to a local so the borrow in `ServiceTarget::Producer` outlives
            // the `select!` expansion (a temporary would be dropped too early).
            let producer_ref =
                ProducerRef::new(p.target_core_node.as_str(), p.target_instance_id.as_str());
            // Abandon an in-flight probe the moment shutdown starts, so a probe
            // racing the session close cannot emit a teardown-time warning.
            let poll_result = tokio::select! {
                biased;
                _ = p.shutdown_token.cancelled() => return,
                _ = p.instance_done.cancelled() => return,
                result = ServiceMessenger::poll(
                    &p.messenger,
                    &p.core_node_name,
                    &p.caller_instance_id,
                    SenderTarget::node_from_validated(&p.to_node_name, &p.node_tag),
                    NODE_HEALTH_SERVICE,
                    ServiceTarget::Producer(&producer_ref),
                    request_payload.clone(),
                    p.policy.timeout,
                ) => result,
            };

            // If either cancellation fired while this probe was in flight, stop
            // here without recording or logging. `instance_done` means the
            // instance's process exited on its own and the exit watcher is moving
            // it to a terminal state, so a node that simply finished never
            // produces a trailing "became unhealthy". `shutdown_token` means the
            // daemon is tearing down on purpose, so a probe that raced the session
            // close must not emit a teardown-time warning.
            if p.instance_done.is_cancelled() || p.shutdown_token.is_cancelled() {
                return;
            }

            let probe_error = poll_result.err();
            let misses_before_probe = tracker.consecutive_misses();
            let edge = tracker.record(probe_error.is_none());

            // Record the tracked health so `stack list` and `node info` can
            // report it without re-probing. The instance is never removed here,
            // so an unhealthy node stays visible in the stack until it recovers
            // or is stopped explicitly.
            instance.set_healthy(tracker.healthy());

            // Log and record only on health edges, so a node that stays down
            // does not re-emit the same warning every tick.
            match edge {
                Some(HealthEdge::Down) => {
                    let reason = probe_error
                        .as_ref()
                        .map(ToString::to_string)
                        .unwrap_or_default();
                    tracing::warn!(
                        "Health monitor: instance '{}' of node '{}:{}' became unhealthy after {} \
                         consecutive missed health checks, last: {}",
                        instance_id_str,
                        p.to_node_name,
                        p.node_tag,
                        tracker.consecutive_misses(),
                        reason
                    );
                    append_stack_log(
                        &p.peppy_dirs,
                        &format!(
                            "Instance '{}' of node '{}:{}' became unhealthy: {} consecutive \
                             health checks failed, last: {}",
                            instance_id_str,
                            p.to_node_name,
                            p.node_tag,
                            tracker.consecutive_misses(),
                            reason,
                        ),
                    );
                }
                Some(HealthEdge::Up) => {
                    tracing::info!(
                        "Health monitor: instance '{}' of node '{}:{}' recovered after {} missed \
                         health checks",
                        instance_id_str,
                        p.to_node_name,
                        p.node_tag,
                        misses_before_probe
                    );
                    append_stack_log(
                        &p.peppy_dirs,
                        &format!(
                            "Instance '{}' of node '{}:{}' recovered after {} missed health checks",
                            instance_id_str, p.to_node_name, p.node_tag, misses_before_probe,
                        ),
                    );
                }
                // No edge. A miss below the threshold, or one more miss of an
                // instance already `unhealthy`, is a low-noise debug heartbeat;
                // a pass of a healthy instance is a no-op.
                None => {
                    if let Some(err) = &probe_error {
                        debug!(
                            "Health monitor: instance '{}' missed a health check ({} consecutive, \
                             threshold {}): {}",
                            instance_id_str,
                            tracker.consecutive_misses(),
                            p.policy.failure_threshold,
                            err
                        );
                    }
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::{HealthEdge, HealthTracker};
    use std::num::NonZeroU32;

    fn threshold(n: u32) -> NonZeroU32 {
        NonZeroU32::new(n).expect("test thresholds are non-zero")
    }

    #[test]
    fn a_fresh_tracker_is_healthy_with_no_misses() {
        let tracker = HealthTracker::new(threshold(3));
        assert!(tracker.healthy());
        assert_eq!(tracker.consecutive_misses(), 0);
    }

    #[test]
    fn passes_of_a_healthy_instance_cause_no_edge() {
        let mut tracker = HealthTracker::new(threshold(3));
        assert_eq!(tracker.record(true), None);
        assert_eq!(tracker.record(true), None);
        assert!(tracker.healthy());
    }

    #[test]
    fn misses_below_the_threshold_leave_the_instance_healthy() {
        let mut tracker = HealthTracker::new(threshold(3));
        assert_eq!(tracker.record(false), None);
        assert_eq!(tracker.record(false), None);
        assert!(tracker.healthy());
        assert_eq!(tracker.consecutive_misses(), 2);
    }

    #[test]
    fn the_miss_reaching_the_threshold_turns_the_instance_unhealthy() {
        let mut tracker = HealthTracker::new(threshold(3));
        tracker.record(false);
        tracker.record(false);
        assert_eq!(tracker.record(false), Some(HealthEdge::Down));
        assert!(!tracker.healthy());
        assert_eq!(tracker.consecutive_misses(), 3);
    }

    #[test]
    fn a_pass_resets_the_miss_count_before_the_threshold() {
        let mut tracker = HealthTracker::new(threshold(3));
        tracker.record(false);
        tracker.record(false);
        assert_eq!(tracker.record(true), None);
        assert_eq!(tracker.consecutive_misses(), 0);
        // The run restarts from zero, so two more misses are still short of
        // the threshold.
        assert_eq!(tracker.record(false), None);
        assert_eq!(tracker.record(false), None);
        assert!(tracker.healthy());
    }

    #[test]
    fn further_misses_of_an_unhealthy_instance_cause_no_edge() {
        let mut tracker = HealthTracker::new(threshold(2));
        tracker.record(false);
        assert_eq!(tracker.record(false), Some(HealthEdge::Down));
        assert_eq!(tracker.record(false), None);
        assert_eq!(tracker.record(false), None);
        assert!(!tracker.healthy());
        assert_eq!(tracker.consecutive_misses(), 4);
    }

    #[test]
    fn one_pass_recovers_an_unhealthy_instance() {
        let mut tracker = HealthTracker::new(threshold(2));
        tracker.record(false);
        tracker.record(false);
        tracker.record(false);
        assert_eq!(tracker.record(true), Some(HealthEdge::Up));
        assert!(tracker.healthy());
        assert_eq!(tracker.consecutive_misses(), 0);
    }

    #[test]
    fn a_threshold_of_one_flips_on_the_first_miss() {
        let mut tracker = HealthTracker::new(threshold(1));
        assert_eq!(tracker.record(false), Some(HealthEdge::Down));
        assert_eq!(tracker.record(true), Some(HealthEdge::Up));
        assert_eq!(tracker.record(false), Some(HealthEdge::Down));
    }

    #[test]
    fn the_miss_count_saturates_instead_of_overflowing() {
        let mut tracker = HealthTracker::new(threshold(1));
        tracker.record(false);
        tracker.consecutive_misses = u32::MAX;
        assert_eq!(tracker.record(false), None);
        assert_eq!(tracker.consecutive_misses(), u32::MAX);
    }
}
