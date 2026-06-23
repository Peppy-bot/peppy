//! Router liveness probe for the daemon's router watchdog.
//!
//! This lives in peppy (not in `pmi`) because spawning, supervising, and
//! restarting the zenohd router is a daemon concern. `pmi` only exposes the
//! probe client config through [`pmi::ZenohAdapter::router_probe_config`]; the
//! watchdog policy that consumes it is owned here.

use std::time::Duration;

/// Lock-free handle for probing whether the Zenoh router is responsive.
///
/// Holds a fail-fast probe config (scouting disabled, single connect attempt to
/// the router endpoint). [`Self::is_router_responsive`] opens a throwaway
/// session bounded by `timeout`, the same operation a CLI client performs, so it
/// detects a wedged router that still accepts TCP connections but never
/// completes the Zenoh session handshake. Obtain one via
/// [`MessengerRouterExt::router_health_checker`] and probe without holding the
/// central messenger lock.
pub(crate) struct RouterHealthChecker {
    probe_config: zenoh::config::Config,
}

impl RouterHealthChecker {
    /// Builds a checker from a ready-to-open probe config, as rendered by
    /// [`pmi::ZenohAdapter::router_probe_config`].
    pub(crate) fn new(probe_config: zenoh::config::Config) -> Self {
        Self { probe_config }
    }

    /// Returns `true` if a fresh session to the router completes within
    /// `timeout`; `false` otherwise (timed out, connection refused, ...).
    pub(crate) async fn is_router_responsive(&self, timeout: Duration) -> bool {
        match tokio::time::timeout(timeout, zenoh::open(self.probe_config.clone())).await {
            Ok(Ok(session)) => {
                // We only needed the handshake. Close the probe session, but
                // don't let a slow close stall the watchdog.
                let _ = tokio::time::timeout(Duration::from_secs(1), session.close()).await;
                true
            }
            // Open errored, or our timeout elapsed before the handshake settled.
            _ => false,
        }
    }
}

/// Extension trait that hangs the watchdog's health-check builder off the shared
/// [`pmi::Messenger`]. Kept peppy-local so `pmi` carries no daemon-only router
/// supervision API.
pub(crate) trait MessengerRouterExt {
    /// Returns a lock-free [`RouterHealthChecker`] for the router watchdog, or
    /// `None` for backends without a restartable router (the mock).
    fn router_health_checker(&self) -> Option<RouterHealthChecker>;
}

impl MessengerRouterExt for pmi::Messenger {
    fn router_health_checker(&self) -> Option<RouterHealthChecker> {
        match &self.adapter {
            pmi::MessengerAdapter::Zenoh(adapter) => {
                Some(RouterHealthChecker::new(adapter.router_probe_config()))
            }
            pmi::MessengerAdapter::Mock(_) => None,
        }
    }
}
