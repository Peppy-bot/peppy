use super::CancellationToken;

use crate::MessengerHandle;
use crate::error::Result;

use futures::FutureExt;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Mutex, PoisonError};
use std::time::Duration;
use tracing::{error, info, warn};

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
pub struct NodeRunner {
    messenger: MessengerHandle,
    processor: Processor,
    cancellation_token: CancellationToken,
    shutdown_hooks: Mutex<Vec<ShutdownHook>>,
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
        let messenger = MessengerHandle::from_host_port_reconnecting_with_discovery(
            processor.messaging_host(),
            processor.messaging_port(),
            processor.discovery(),
        )
        .await?;

        Ok(Self {
            messenger,
            processor,
            cancellation_token,
            shutdown_hooks: Mutex::new(Vec::new()),
        })
    }

    /// Get reference to the messenger handle
    pub fn messenger(&self) -> &MessengerHandle {
        &self.messenger
    }

    /// Get reference to the runtime processor
    pub fn processor(&self) -> &Processor {
        &self.processor
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
    pub(crate) async fn run_shutdown_hooks(&self, grace: Duration) {
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
