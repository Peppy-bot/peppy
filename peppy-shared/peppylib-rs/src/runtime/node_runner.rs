use super::CancellationToken;

use crate::error::Result;
use crate::{MessengerHandle, SessionScope};

use futures::FutureExt;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Mutex, PoisonError};
use std::time::Duration;
use tracing::{error, info, warn};

use super::endpoints::{AnnouncedEndpoint, AnnouncedEndpoints, EndpointBinding};
use super::processor::Processor;

/// A registered shutdown hook: an async cleanup unit run by the runtime after
/// the cancellation token fires, before the process exits.
type ShutdownHook = Pin<Box<dyn Future<Output = ()> + Send + 'static>>;

/// The main runtime handle for a Peppy node.
///
/// Provides access to:
/// - Messaging system via `messenger()`
/// - Runtime configuration via `processor()`
/// - Cancellation token for graceful shutdown via `cancellation_token()`
/// - Shutdown cleanup registration via `on_shutdown()`
/// - Endpoint announcement via `announce_endpoint()`
pub struct NodeRunner {
    messenger: MessengerHandle,
    processor: Processor,
    cancellation_token: CancellationToken,
    shutdown_hooks: Mutex<Vec<ShutdownHook>>,
    endpoints: Mutex<AnnouncedEndpoints>,
}

impl NodeRunner {
    /// Create a new NodeRunner, connecting to the messaging system.
    pub async fn new(processor: Processor) -> Result<Self> {
        Self::with_cancellation_token(processor, CancellationToken::new()).await
    }

    /// Create a new NodeRunner with a provided cancellation token.
    pub async fn with_cancellation_token(
        processor: Processor,
        cancellation_token: CancellationToken,
    ) -> Result<Self> {
        // Nodes are long-lived: use a reconnecting session so a router restart
        // (e.g. the daemon's watchdog respawning zenohd) is recovered
        // transparently instead of leaving the node off the bus. The session is
        // a peer that forms direct links per the node's discovery settings.
        let messenger =
            MessengerHandle::connect(processor.messaging_host(), processor.messaging_port())
                .reconnecting()
                .scope(SessionScope::Discovery(processor.discovery()))
                .await?;

        let endpoints = AnnouncedEndpoints::new(processor.declared_endpoints().clone());
        Ok(Self {
            messenger,
            processor,
            cancellation_token,
            shutdown_hooks: Mutex::new(Vec::new()),
            endpoints: Mutex::new(endpoints),
        })
    }

    /// Announces the socket the node bound for the endpoint the manifest
    /// declares as `label` under `execution.endpoints`. Called inside
    /// `setup_fn`, after the bind, with the address the listener reports:
    /// the daemon turns it into the URLs an operator opens.
    ///
    /// Refused for a label the manifest does not declare, for a second
    /// announcement of one label, once setup has returned, and for a scheme
    /// or path that breaks [`EndpointBinding`]'s rules.
    pub fn announce_endpoint(&self, label: &str, binding: EndpointBinding) -> Result<()> {
        self.endpoints
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .announce(label, binding)
    }

    /// Ends the announcement window and checks the set against the
    /// manifest: every declared label must have been announced. The runtime
    /// calls this when `setup_fn` returns; a caller that owns the runner's
    /// lifecycle without going through `NodeBuilder::run` (the test harness
    /// core) does the same once its setup returns. Answers the sealed set, in
    /// label order.
    pub fn seal_endpoints(&self) -> Result<Vec<AnnouncedEndpoint>> {
        self.endpoints
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .seal()
    }

    /// The endpoints announced so far, in label order, sealed or not.
    pub fn announced_endpoints(&self) -> Vec<AnnouncedEndpoint> {
        self.endpoints
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .snapshot()
    }

    /// Whether the manifest declares any endpoint, which is what decides
    /// whether the runtime offers the `node_endpoints` service.
    pub(crate) fn declares_endpoints(&self) -> bool {
        self.endpoints
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .declares_any()
    }

    /// Get reference to the messenger handle
    pub fn messenger(&self) -> &MessengerHandle {
        &self.messenger
    }

    /// Get reference to the runtime processor
    pub fn processor(&self) -> &Processor {
        &self.processor
    }

    /// Handle onto the scalar pairing slot declared at `link_id` in
    /// `depends_on.pairings` (a `one` or `zero_or_one` slot). Exposes the
    /// slot's live pair: `peer(link_id)?.paired()` returns the current peer
    /// (if any) and `wait_paired()` awaits one. Errors if the manifest
    /// declares no such slot, and panics if it declares one with a
    /// multi-peer cardinality, which is read through [`Self::peer_set`].
    pub fn peer(&self, link_id: &str) -> crate::error::Result<super::PeerSlot> {
        let cardinality = self.pairing_slot_cardinality(link_id)?;
        if !cardinality.is_scalar() {
            super::pairing::pairing_shape_panic(link_id, "peer()", cardinality.as_str());
        }
        Ok(super::PeerSlot::new(
            link_id,
            cardinality,
            self.peer_set_watch(link_id)?,
        ))
    }

    /// Handle onto the multi-peer pairing slot declared at `link_id` in
    /// `depends_on.pairings` (a `one_or_more` or `zero_or_more` slot).
    /// Exposes every pair the slot holds, in establishment order, through
    /// `members()` or `peers()`. Errors if the manifest declares no such
    /// slot, and panics if it declares one with a scalar cardinality, which
    /// is read through [`Self::peer`].
    pub fn peer_set(&self, link_id: &str) -> crate::error::Result<super::PeerSlotSet> {
        let cardinality = self.pairing_slot_cardinality(link_id)?;
        if cardinality.is_scalar() {
            super::pairing::pairing_shape_panic(link_id, "peer_set()", cardinality.as_str());
        }
        Ok(super::PeerSlotSet::new(self.peer_set_watch(link_id)?))
    }

    /// The copy this instance belongs to, as the launch composed it; `None`
    /// for an instance run outside a copy.
    pub fn copy(&self) -> Option<&str> {
        self.processor.copy()
    }

    fn pairing_slot_cardinality(
        &self,
        link_id: &str,
    ) -> crate::error::Result<config::node::Cardinality> {
        self.processor
            .pairing_slot_cardinality(link_id)
            .ok_or_else(|| crate::error::Error::UnknownPairingSlot {
                link_id: link_id.to_string(),
            })
    }

    fn peer_set_watch(
        &self,
        link_id: &str,
    ) -> crate::error::Result<tokio::sync::watch::Receiver<crate::messaging::PeerSetState>> {
        self.processor.peer_set_watch(link_id).ok_or_else(|| {
            crate::error::Error::UnknownPairingSlot {
                link_id: link_id.to_string(),
            }
        })
    }

    /// Handle onto the scalar observer slot declared at `link_id` in
    /// `depends_on.pairing_observers` (a `one` or `zero_or_one` slot). Exposes
    /// the slot's observed source, through the accessor its cardinality declares:
    /// `observation_slot(link_id)?.sole_source()` for a `one` slot, which always
    /// observes a pairing, and `observation_slot(link_id)?.source()` for a
    /// `zero_or_one` slot, which answers `None` where the deployment wrote it
    /// vacant. Errors if the manifest declares no such observer slot, and panics
    /// if it declares one with a multi-member cardinality, which is read through
    /// [`Self::observation_slot_set`] instead.
    pub fn observation_slot(&self, link_id: &str) -> crate::error::Result<super::ObservationSlot> {
        let cardinality = self.observation_slot_cardinality(link_id)?;
        if !cardinality.is_scalar() {
            super::observation::observer_shape_panic(
                link_id,
                "observation_slot()",
                cardinality.as_str(),
            );
        }
        Ok(super::ObservationSlot::new(
            link_id,
            cardinality,
            self.observation_slot_watch(link_id)?,
        ))
    }

    /// Handle onto the multi-member observer slot declared at `link_id` in
    /// `depends_on.pairing_observers` (a `one_or_more` or `zero_or_more` slot).
    /// Exposes every pairing the slot observes, in plan order, through the
    /// accessor its cardinality declares:
    /// `observation_slot_set(link_id)?.non_empty_sources()` for a `one_or_more`
    /// slot, whose set is never empty, and
    /// `observation_slot_set(link_id)?.sources()` for a `zero_or_more` slot,
    /// whose set is empty where the plan bound no pairing. Errors if the
    /// manifest declares no such observer slot, and panics if it declares one
    /// with a scalar cardinality, which is read through
    /// [`Self::observation_slot`] instead.
    pub fn observation_slot_set(
        &self,
        link_id: &str,
    ) -> crate::error::Result<super::ObservationSlotSet> {
        let cardinality = self.observation_slot_cardinality(link_id)?;
        if cardinality.is_scalar() {
            super::observation::observer_shape_panic(
                link_id,
                "observation_slot_set()",
                cardinality.as_str(),
            );
        }
        Ok(super::ObservationSlotSet::new(
            link_id,
            cardinality,
            self.observation_slot_watch(link_id)?,
        ))
    }

    fn observation_slot_cardinality(
        &self,
        link_id: &str,
    ) -> crate::error::Result<config::node::Cardinality> {
        self.processor
            .observation_slot_cardinality(link_id)
            .ok_or_else(|| crate::error::Error::UnknownObservationSlot {
                link_id: link_id.to_string(),
            })
    }

    fn observation_slot_watch(
        &self,
        link_id: &str,
    ) -> crate::error::Result<tokio::sync::watch::Receiver<crate::messaging::ObservationState>>
    {
        self.processor
            .observation_slot_watch(link_id)
            .ok_or_else(|| crate::error::Error::UnknownObservationSlot {
                link_id: link_id.to_string(),
            })
    }

    /// Get the cancellation token for coordinating graceful shutdown.
    ///
    /// The token fires on every stop path: `peppy node stop`, the daemon
    /// tearing the stack down, SIGINT/SIGTERM, and daemon-liveness loss. Use it
    /// to stop in-flight work in long-running tasks:
    /// ```ignore
    /// let token = node_runner.cancellation_token().clone();
    /// tokio::spawn(async move {
    ///     loop {
    ///         tokio::select! {
    ///             _ = token.cancelled() => break,
    ///             _ = do_work() => {}
    ///         }
    ///     }
    /// });
    /// ```
    ///
    /// Do NOT run cleanup (hardware teardown, lock release) from a spawned task
    /// observing this token: once the token fires, `NodeBuilder::run` returns
    /// and the runtime is torn down, so a detached task is not guaranteed to be
    /// polled again. Register cleanup with [`Self::on_shutdown`] instead; the
    /// runtime awaits those hooks before it returns.
    pub fn cancellation_token(&self) -> &CancellationToken {
        &self.cancellation_token
    }

    /// Register an async cleanup hook to run when the node shuts down.
    ///
    /// Hooks run on every shutdown path (`peppy node stop`, daemon teardown,
    /// SIGINT/SIGTERM, daemon-liveness loss, Ctrl+C in standalone mode, and a
    /// `setup_fn` error) after the cancellation token has been cancelled and
    /// before `NodeBuilder::run` returns. This is the place for hardware
    /// teardown, lock release, and state flushing; unlike a spawned task
    /// observing the cancellation token, a hook is guaranteed to be awaited
    /// before the runtime is torn down. The messenger is still connected while
    /// hooks run, so they can use messaging (e.g. `datastore::remove`).
    ///
    /// Hooks run sequentially in reverse registration order (last registered,
    /// first run), mirroring how resources acquired early in setup are released
    /// last. All hooks share one grace window
    /// (`peppy_config.lifecycle.shutdown_grace_secs`, shipped to the node by
    /// the daemon): cleanup that exceeds it is abandoned mid-await, and on stop
    /// paths the daemon force-kills the process at the same deadline. A hook
    /// that panics is logged and skipped; the remaining hooks still run.
    ///
    /// Register hooks during setup. A hook registered after shutdown has begun
    /// (i.e. after the cancellation token fired) may never run.
    pub fn on_shutdown<F>(&self, hook: F)
    where
        F: Future<Output = ()> + Send + 'static,
    {
        self.shutdown_hooks
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(Box::pin(hook));
    }

    /// Run the registered shutdown hooks, bounded collectively by `grace`.
    ///
    /// Called by `NodeBuilder::run` after the cancellation token has been
    /// cancelled, on every exit path. Takes the hooks out of the registry, so
    /// a second call (and any `Arc<NodeRunner>` reference cycle through a hook
    /// closure) is harmless.
    ///
    /// Public because a caller that owns the runner's lifecycle without going
    /// through `NodeBuilder::run` — the test harness core here and its Python
    /// twin over `NodeRunner.new_standalone` — is responsible for this exact
    /// convergence step itself; the sensible grace to pass is
    /// [`Processor::shutdown_grace`](crate::runtime::Processor::shutdown_grace).
    pub async fn run_shutdown_hooks(&self, grace: Duration) {
        let hooks: Vec<ShutdownHook> = std::mem::take(
            &mut *self
                .shutdown_hooks
                .lock()
                .unwrap_or_else(PoisonError::into_inner),
        );
        if hooks.is_empty() {
            return;
        }
        let count = hooks.len();
        info!("Running {count} shutdown hook(s) within a {grace:?} grace window");
        let run_all = async {
            // Reverse registration order: tear down in the opposite order of
            // setup, like destructors.
            for (idx, hook) in hooks.into_iter().rev().enumerate() {
                if std::panic::AssertUnwindSafe(hook)
                    .catch_unwind()
                    .await
                    .is_err()
                {
                    error!(
                        "Shutdown hook {}/{count} panicked; continuing with remaining hooks",
                        idx + 1
                    );
                }
            }
        };
        if tokio::time::timeout(grace, run_all).await.is_err() {
            warn!(
                "Shutdown hooks did not finish within the {grace:?} grace window; \
                 exiting with cleanup incomplete"
            );
        }
    }
}
