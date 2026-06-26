//! Federates the daemon's local zenohd router to the caller's *per-user cloud
//! router* and keeps it federated for the daemon's lifetime.
//!
//! The local router is always started *standalone* by the builder; resolving the
//! cloud-router endpoint needs a backend round-trip. This task owns the whole
//! federation lifecycle:
//!
//! * **Initial federation (gates startup).** Once the router is up (it waits on
//!   `messaging_ready` so it cannot race [`MessagingRouter`](super::messaging_router)'s
//!   own `start_router`), the first poll resolves the upstream and federates the
//!   local router to it. This task signals a readiness gate to `serve` once that
//!   first poll completes, so `serve` only reports ready *after* federation is in
//!   place — bounded by `connect_timeout` so a slow/unreachable backend can't
//!   stall startup past it (the daemon then proceeds standalone and keeps
//!   retrying in the background).
//! * **Immediate (re)federation on login/logout.** `peppy auth login`/`logout`
//!   poke the daemon over the control socket
//!   ([`FederationControl`](super::federation_control)); the poke is delivered
//!   here as a [`RefederateRequest`] that runs a poll *now* (not on the next
//!   interval) and acks the resulting [`FederationOutcome`] so the CLI knows
//!   federation is in place before it returns.
//! * **Keepalive.** The cloud router is idle-reaped by the backend unless its
//!   config is re-pulled within the idle window. The periodic re-resolve re-pulls
//!   when the cached config goes stale, refreshing the server-side `last_seen_at`
//!   (and re-provisioning a router that was already reaped).
//! * **Live (re)federation.** When the resolved upstream changes — the user logs
//!   in, logs out, or the endpoint moves — the local router's zenohd config is
//!   re-rendered and the router restarted, so the change takes effect without a
//!   full daemon restart.

use super::serve::{ServeAsyncCommand, ServeAsyncHandle};
use crate::error::{Error, Result};
use pmi::{Messenger, MessengerBackend};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{Mutex, mpsc, oneshot, watch};
use tracing::{info, warn};

/// How often the manager re-resolves the caller's cloud-router config. Frequent
/// enough to (de)federate within ~a poll even if a login/logout poke is missed
/// (no daemon control socket); the config-pull keepalive is itself cache-gated,
/// so a steady state mostly hits the local cache and does no network I/O.
const POLL_INTERVAL: Duration = Duration::from_secs(30);

/// Resolves the desired upstream `(endpoint, tls)` for the local router, or
/// `None` when there is nothing to federate to (logged out / unreachable). A
/// boxed closure so tests can inject a deterministic resolver in place of the
/// real (blocking, networked) `resolve_federation_target`.
type Resolver = Arc<dyn Fn() -> Option<(String, pmi::TlsConfig)> + Send + Sync>;

/// Outcome of one federation poll, reported back to a control-socket poke so the
/// CLI can tell the user the post-apply state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum FederationOutcome {
    /// Federation is in effect: `Some(ep)` federated to `ep`, `None`
    /// de-federated. Covers both "just applied" and "already in place" (a no-op
    /// poll where the upstream was unchanged).
    Applied(Option<String>),
    /// An operator-pinned `ZENOH_CONFIG` owns the router config; nothing changed.
    Pinned,
    /// The resolve or apply failed; the periodic loop will keep retrying.
    Failed(String),
}

/// A "refederate now" request from the control socket: run a poll immediately and
/// reply with the resulting [`FederationOutcome`] over `ack`.
pub(crate) struct RefederateRequest {
    pub(crate) ack: oneshot::Sender<FederationOutcome>,
}

/// Sends pokes to the federation loop (held by [`FederationControl`]).
pub(crate) type TriggerSender = mpsc::Sender<RefederateRequest>;
/// Receives pokes in the federation loop.
pub(crate) type TriggerReceiver = mpsc::Receiver<RefederateRequest>;

/// Background task (a [`ServeAsyncCommand`]) that federates the local router to
/// the per-user cloud router and keeps it federated. See the module docs.
pub(crate) struct RouterFederation {
    messenger: Arc<Mutex<Messenger>>,
    resolver: Resolver,
    /// Goes `true` once the router process is up (MessagingRouter ran
    /// `start_router` + `start_session`). The task waits on this before touching
    /// the router so its initial federation cannot race the router's own startup.
    messaging_ready: watch::Receiver<bool>,
    /// Immediate-refederation pokes from the control socket.
    trigger_rx: TriggerReceiver,
    /// Bound on the initial federation (the startup gate) and on each resolve, so
    /// a slow/unreachable backend can't stall startup or a poll past it.
    connect_timeout: Duration,
}

impl RouterFederation {
    pub(crate) fn new(
        messenger: Arc<Mutex<Messenger>>,
        api_url: String,
        messaging_ready: watch::Receiver<bool>,
        trigger_rx: TriggerReceiver,
        connect_timeout: Duration,
    ) -> Self {
        let resolver: Resolver = Arc::new(move || {
            crate::auth::router::resolve_federation_target(&api_url, connect_timeout)
        });
        Self {
            messenger,
            resolver,
            messaging_ready,
            trigger_rx,
            connect_timeout,
        }
    }
}

impl ServeAsyncCommand for RouterFederation {
    fn run(self: Box<Self>) -> ServeAsyncHandle {
        let RouterFederation {
            messenger,
            resolver,
            messaging_ready,
            trigger_rx,
            connect_timeout,
        } = *self;
        // Readiness gate: fired by `manage_federation` once the first federation
        // poll completes (or the timeout elapses), so `serve` blocks on federation
        // being in place but never longer than `connect_timeout`. `serve` treats
        // this gate as non-fatal, so a drop here (e.g. shutdown wins the race
        // below before it fires) degrades to "proceed standalone", never a crash.
        let (ready_tx, ready_rx) = oneshot::channel();
        let future = Box::pin(async move {
            // Race the maintenance loop against shutdown so the daemon can exit
            // promptly (the loop is otherwise infinite).
            tokio::select! {
                _ = manage_federation(
                    messenger, resolver, messaging_ready, trigger_rx, ready_tx, connect_timeout,
                ) => {}
                res = super::shutdown_signal::shutdown_signal() => {
                    res.map_err(|e| Error::ExecutionFailed(
                        format!("router federation: failed to listen for shutdown: {e}")
                    ))?;
                }
            }
            Ok(())
        });
        ServeAsyncHandle::new_optional_ready(future, ready_rx)
    }
}

/// Fires the startup readiness gate exactly once. Idempotent: the `Option` is
/// `take`n, so later calls (or a drop) are no-ops.
fn fire_gate(gate: &mut Option<oneshot::Sender<()>>) {
    if let Some(tx) = gate.take() {
        let _ = tx.send(());
    }
}

/// Waits for the router to come up, runs the initial federation (firing the
/// startup gate when it completes or the timeout elapses), then services periodic
/// keepalive re-resolves and immediate login/logout pokes for the daemon's
/// lifetime (the caller races it against the shutdown signal).
async fn manage_federation(
    messenger: Arc<Mutex<Messenger>>,
    resolver: Resolver,
    mut messaging_ready: watch::Receiver<bool>,
    mut trigger_rx: TriggerReceiver,
    ready_tx: oneshot::Sender<()>,
    connect_timeout: Duration,
) {
    let mut ready_tx = Some(ready_tx);

    // Phase 1 — wait for the router, bounded by `connect_timeout`. Don't touch the
    // router until it is up, or the initial federation could race MessagingRouter's
    // `start_router`/`start_session`. `wait_for` checks the current value first,
    // then awaits changes. Drop the borrowed `Ref` it returns immediately (map to
    // `()`) so `messaging_ready` is free for the timeout-elapsed arm to reuse.
    let armed = tokio::time::timeout(connect_timeout, messaging_ready.wait_for(|r| *r))
        .await
        .map(|res| res.map(|_ready| ()));
    match armed {
        Ok(Ok(())) => {}
        Ok(Err(_)) => {
            // `messaging_ready` closed before going true: the router task never
            // started or already exited, so there is nothing to federate. Unblock
            // startup and stop.
            fire_gate(&mut ready_tx);
            return;
        }
        Err(_elapsed) => {
            // The router isn't up within the bound: unblock startup now (the
            // daemon proceeds standalone), then keep waiting (unbounded) so the
            // local router still federates once it does come up.
            fire_gate(&mut ready_tx);
            if messaging_ready.wait_for(|r| *r).await.is_err() {
                return;
            }
        }
    }

    // Phase 2 — initial federation. The router was started standalone, so nothing
    // is federated yet. The resolve inside is itself bounded by `connect_timeout`,
    // so this completes (and unblocks startup) within the bound plus a fast local
    // bounce, even if the user is logged in but the backend is unreachable.
    let mut applied: Option<String> = None;
    poll_and_apply(&messenger, &resolver, connect_timeout, &mut applied).await;
    fire_gate(&mut ready_tx);

    // Phase 3 — steady state: periodic keepalive/re-resolve, or an immediate poke
    // from `auth login`/`logout`. A single `select!` keeps sole ownership of
    // `applied`, so a poke and a tick can never apply concurrently.
    loop {
        tokio::select! {
            _ = tokio::time::sleep(POLL_INTERVAL) => {
                poll_and_apply(&messenger, &resolver, connect_timeout, &mut applied).await;
            }
            maybe_req = trigger_rx.recv() => {
                match maybe_req {
                    Some(req) => {
                        let outcome =
                            poll_and_apply(&messenger, &resolver, connect_timeout, &mut applied).await;
                        // The CLI may have already given up (read timeout); ignore.
                        let _ = req.ack.send(outcome);
                    }
                    // All trigger senders dropped (control listener gone, only at
                    // teardown in practice): keep the keepalive poll going without
                    // it rather than busy-spinning on a closed channel.
                    None => break,
                }
            }
        }
    }

    loop {
        tokio::time::sleep(POLL_INTERVAL).await;
        poll_and_apply(&messenger, &resolver, connect_timeout, &mut applied).await;
    }
}

/// One poll: resolve the desired upstream and, if it changed, (re)federate the
/// local router. Updates `*applied` to the upstream now in effect and returns the
/// [`FederationOutcome`] (so a poke can ack the post-apply state).
async fn poll_and_apply(
    messenger: &Arc<Mutex<Messenger>>,
    resolver: &Resolver,
    connect_timeout: Duration,
    applied: &mut Option<String>,
) -> FederationOutcome {
    // The resolver is blocking (HTTP + file I/O); keep it off the async worker. It
    // also performs the cloud router's keepalive re-pull when its cached config has
    // gone stale. Bound the whole resolve by `connect_timeout` so a hung pull can't
    // stall a poll (or the startup gate) past it; the timed-out blocking thread is
    // harmless (its own HTTP timeout ends it) and its result is simply discarded.
    let resolver = resolver.clone();
    let resolved = match tokio::time::timeout(
        connect_timeout,
        tokio::task::spawn_blocking(move || resolver()),
    )
    .await
    {
        Ok(Ok(t)) => t,
        Ok(Err(e)) => {
            warn!(error = %e, "router federation: resolve task panicked; will retry");
            return FederationOutcome::Failed(format!("resolve task panicked: {e}"));
        }
        Err(_elapsed) => {
            warn!("router federation: resolve timed out; local router stays as-is, will retry");
            return FederationOutcome::Failed("resolve timed out".to_string());
        }
    };

    let desired = resolved.as_ref().map(|(ep, _)| ep.clone());
    if desired == *applied {
        // Steady state (including the cache-gated keepalive re-pull): the upstream
        // is unchanged, so there is nothing to re-render or restart. Report it as
        // applied so a poke gets a positive "already in place" ack.
        return FederationOutcome::Applied(applied.clone());
    }

    match refederate_and_restart(messenger, &resolved).await {
        Ok(true) => {
            match &desired {
                Some(ep) => {
                    info!(upstream = %ep, "router federation: (re)federated local router to cloud router")
                }
                None => {
                    info!("router federation: de-federated local router (logged out / no upstream)")
                }
            }
            *applied = desired.clone();
            FederationOutcome::Applied(desired)
        }
        Ok(false) => {
            // An operator-pinned `ZENOH_CONFIG` owns the router config, so the
            // desired change cannot be applied here. Advance `applied` so this is
            // noted once per change (login/logout) rather than every poll, but warn
            // so the operator knows federation is not being auto-managed.
            warn!(
                "router federation: ZENOH_CONFIG pins the router config; the desired \
                 federation change was not applied (the operator owns this router's config)"
            );
            *applied = desired;
            FederationOutcome::Pinned
        }
        Err(e) => {
            // Leave `applied` unchanged so the next poll retries the apply.
            warn!(error = %e, "router federation: failed to apply upstream change; will retry");
            FederationOutcome::Failed(e.to_string())
        }
    }
}

/// Re-renders the local router's config with the (possibly empty) upstream and,
/// if the config actually changed, restarts zenohd so it takes effect. Holds the
/// messenger lock across the whole stop/start so it cannot interleave with the
/// watchdog's own restart.
///
/// Returns whether zenohd was restarted: `false` when [`Messenger::refederate`]
/// was a no-op (an operator-pinned `ZENOH_CONFIG`), so a pointless bounce is
/// skipped; `true` when the config was rewritten and the router bounced.
async fn refederate_and_restart(
    messenger: &Arc<Mutex<Messenger>>,
    target: &Option<(String, pmi::TlsConfig)>,
) -> Result<bool> {
    let (connect_endpoints, tls) = match target {
        Some((ep, tls)) => (vec![ep.clone()], Some(tls.clone())),
        None => (Vec::new(), None),
    };
    let mut messenger = messenger.lock().await;
    let rewrote = messenger
        .refederate(connect_endpoints, tls)
        .map_err(Error::PeppyMessagingInterface)?;
    if !rewrote {
        // The config was not rewritten (operator-pinned), so bouncing zenohd would
        // change nothing. Skip the restart and report it.
        return Ok(false);
    }
    // Apply the new config by bouncing zenohd. The daemon's reconnecting session
    // and the nodes re-establish automatically (same path as a watchdog restart).
    messenger
        .stop_router()
        .await
        .map_err(Error::PeppyMessagingInterface)?;
    messenger
        .start_router()
        .await
        .map_err(Error::PeppyMessagingInterface)?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pmi::{MessengerAdapter, MockAdapter};
    use std::sync::atomic::{AtomicUsize, Ordering};

    const ENDPOINT: &str = "tls/cap.zenoh.localhost:7443";

    fn mock_messenger() -> Arc<Mutex<Messenger>> {
        Arc::new(Mutex::new(Messenger::new(MessengerAdapter::Mock(
            MockAdapter::default(),
        ))))
    }

    /// A resolver returning a fixed value and counting its calls.
    fn counting_resolver(value: Option<(String, pmi::TlsConfig)>) -> (Resolver, Arc<AtomicUsize>) {
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = calls.clone();
        let resolver: Resolver = Arc::new(move || {
            counter.fetch_add(1, Ordering::SeqCst);
            value.clone()
        });
        (resolver, calls)
    }

    fn upstream() -> Option<(String, pmi::TlsConfig)> {
        Some((ENDPOINT.to_string(), pmi::TlsConfig::default()))
    }

    /// A login/logout poke runs a federation poll *immediately* (well within the
    /// 30s `POLL_INTERVAL`) and acks the applied outcome — the whole point of the
    /// control channel.
    #[tokio::test]
    async fn poke_refederates_immediately_and_acks() {
        let messenger = mock_messenger();
        let (resolver, calls) = counting_resolver(upstream());
        // Router already up.
        let (messaging_tx, messaging_rx) = watch::channel(true);
        let (trigger_tx, trigger_rx) = mpsc::channel(8);
        let (ready_tx, ready_rx) = oneshot::channel();

        let task = tokio::spawn(manage_federation(
            messenger,
            resolver,
            messaging_rx,
            trigger_rx,
            ready_tx,
            Duration::from_secs(5),
        ));

        // Startup gate fires after the first (initial) poll; that poll resolved once.
        tokio::time::timeout(Duration::from_secs(1), ready_rx)
            .await
            .expect("startup gate fires promptly")
            .expect("gate sender not dropped");
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "the initial poll resolved once"
        );

        // Poke: must run a second resolve immediately and ack — not wait out the
        // 30s poll interval.
        let (ack_tx, ack_rx) = oneshot::channel();
        trigger_tx
            .send(RefederateRequest { ack: ack_tx })
            .await
            .expect("trigger accepted");
        let outcome = tokio::time::timeout(Duration::from_secs(1), ack_rx)
            .await
            .expect("the poke is serviced immediately, not after POLL_INTERVAL")
            .expect("ack sender not dropped");

        // The upstream is already in effect from the initial poll, so the poke
        // re-resolves and reports it applied.
        assert_eq!(
            outcome,
            FederationOutcome::Applied(Some(ENDPOINT.to_string()))
        );
        assert_eq!(
            calls.load(Ordering::SeqCst),
            2,
            "the poke ran a second resolve"
        );

        drop(messaging_tx);
        task.abort();
    }

    /// The startup gate fires within the timeout even when the backend is slow
    /// enough to blow the bound, so a hung backend can never stall `serve` past
    /// `connect_timeout`. The federation loop then keeps retrying.
    #[tokio::test]
    async fn startup_gate_fires_within_timeout_when_resolve_is_slow() {
        let messenger = mock_messenger();
        // Resolver sleeps past the (short) connect timeout, so the bounded resolve
        // elapses and the first poll completes as a failure — the gate must still
        // fire.
        let resolver: Resolver = Arc::new(|| {
            std::thread::sleep(Duration::from_millis(400));
            None
        });
        let (messaging_tx, messaging_rx) = watch::channel(true);
        let (_trigger_tx, trigger_rx) = mpsc::channel(8);
        let (ready_tx, ready_rx) = oneshot::channel();

        let task = tokio::spawn(manage_federation(
            messenger,
            resolver,
            messaging_rx,
            trigger_rx,
            ready_tx,
            Duration::from_millis(100),
        ));

        // Gate fires close to the 100ms bound, well before the 400ms resolve.
        tokio::time::timeout(Duration::from_secs(1), ready_rx)
            .await
            .expect("gate fires within the timeout despite a slow backend")
            .expect("gate sender not dropped");

        drop(messaging_tx);
        task.abort();
    }

    /// A poll with no upstream (logged out / pull failed) reports `Applied(None)`
    /// when nothing was federated, and the gate still fires.
    #[tokio::test]
    async fn logged_out_initial_poll_is_a_noop_and_fires_the_gate() {
        let messenger = mock_messenger();
        let (resolver, calls) = counting_resolver(None);
        let (messaging_tx, messaging_rx) = watch::channel(true);
        let (_trigger_tx, trigger_rx) = mpsc::channel(8);
        let (ready_tx, ready_rx) = oneshot::channel();

        let task = tokio::spawn(manage_federation(
            messenger,
            resolver,
            messaging_rx,
            trigger_rx,
            ready_tx,
            Duration::from_secs(5),
        ));

        tokio::time::timeout(Duration::from_secs(1), ready_rx)
            .await
            .expect("gate fires")
            .expect("gate sender not dropped");
        assert!(calls.load(Ordering::SeqCst) >= 1);

        drop(messaging_tx);
        task.abort();
    }
}
