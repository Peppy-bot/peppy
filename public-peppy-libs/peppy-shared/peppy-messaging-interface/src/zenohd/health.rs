//! Router liveness probe for the daemon's router watchdog.

/// Lock-free handle for probing whether the Zenoh router is responsive.
///
/// Holds a fail-fast probe config (scouting disabled, single connect attempt to
/// the router endpoint). [`Self::is_router_responsive`] opens a throwaway
/// session bounded by `timeout` — the same operation a CLI client performs — so
/// it detects a wedged router that still accepts TCP connections but never
/// completes the Zenoh session handshake. Obtain one via
/// [`crate::Messenger::router_health_checker`] and probe without holding the
/// central messenger lock.
pub struct RouterHealthChecker {
    probe_config: zenoh::config::Config,
}

impl RouterHealthChecker {
    /// Builds a checker from a ready-to-open probe config. Constructed by
    /// `ZenohAdapter::router_health_checker`, which renders the probe client
    /// config template.
    pub(crate) fn new(probe_config: zenoh::config::Config) -> Self {
        Self { probe_config }
    }

    /// Returns `true` if a fresh session to the router completes within
    /// `timeout`; `false` otherwise (timed out, connection refused, …).
    pub async fn is_router_responsive(&self, timeout: std::time::Duration) -> bool {
        match tokio::time::timeout(timeout, zenoh::open(self.probe_config.clone())).await {
            Ok(Ok(session)) => {
                // We only needed the handshake. Close the probe session, but
                // don't let a slow close stall the watchdog.
                let _ =
                    tokio::time::timeout(std::time::Duration::from_secs(1), session.close()).await;
                true
            }
            // Open errored, or our timeout elapsed before the handshake settled.
            _ => false,
        }
    }
}
