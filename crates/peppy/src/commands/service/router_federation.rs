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
//!   federation is in place before it returns. A login poke additionally
//!   *verifies* the federation link with a real TLS handshake
//!   ([`pmi::probe_tls_reachable`]) so a silent UnknownCA loop is reported as
//!   [`FederationOutcome::Unreachable`] rather than a false success.
//! * **Keepalive.** The cloud router is idle-reaped by the backend unless its
//!   config is re-pulled within the idle window. The periodic re-resolve re-pulls
//!   when the cached config goes stale, refreshing the server-side `last_seen_at`
//!   (and re-provisioning a router that was already reaped). The keepalive does
//!   *not* probe — it is kept cheap.
//! * **Live (re)federation.** When the resolved upstream changes — the user logs
//!   in, logs out, or the endpoint moves — the local router's zenohd config is
//!   re-rendered and the router restarted, so the change takes effect without a
//!   full daemon restart.

use super::serve::{ServeAsyncCommand, ServeAsyncHandle};
use crate::error::{Error, Result};
use pmi::{Messenger, MessengerBackend};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{Mutex, mpsc, oneshot, watch};
use tracing::{info, warn};

/// How often the manager re-resolves the caller's cloud-router config. Frequent
/// enough to (de)federate within ~a poll even if a login/logout poke is missed
/// (no daemon control socket); the config-pull keepalive is itself cache-gated,
/// so a steady state mostly hits the local cache and does no network I/O.
const POLL_INTERVAL: Duration = Duration::from_secs(30);

/// How long a *verifying* login/logout poke waits for the federation link's TLS
/// handshake to validate. Deliberately small and decoupled from `connect_timeout`
/// (the resolve bound): a healthy handshake is sub-second, so a tight bound keeps
/// the whole verifying poll (resolve + zenohd bounce + probe) inside the daemon's
/// ack budget — `connect_timeout` + [`super::federation_control`]'s
/// `APPLY_ACK_SLACK`, which is sized to cover this probe. An unreachable /
/// firewalled router fails the probe within this bound and surfaces promptly as
/// [`FederationOutcome::Unreachable`] rather than as a daemon-side ack timeout.
pub(crate) const PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// Resolves the desired upstream `(endpoint, tls)` for the local router, or
/// `None` when there is nothing to federate to (logged out / unreachable). A
/// boxed closure so tests can inject a deterministic resolver in place of the
/// real (blocking, networked) `resolve_federation_target`.
type Resolver = Arc<dyn Fn() -> Option<(String, pmi::TlsConfig)> + Send + Sync>;

/// The future a [`Prober`] returns: `Ok(())` if the upstream's TLS link
/// validates, `Err(reason)` (human-readable) otherwise.
type ProbeFuture = Pin<Box<dyn Future<Output = std::result::Result<(), String>> + Send>>;

/// Verifies that the federation link to `host:port` actually validates with a
/// real TLS handshake. A boxed async closure so tests can inject a deterministic
/// probe (success/failure + a call counter) in place of the real
/// [`pmi::probe_tls_reachable`], which does network I/O.
type Prober = Arc<dyn Fn(String, u16, pmi::TlsConfig, Duration) -> ProbeFuture + Send + Sync>;

/// The real prober: a raw TLS handshake against the upstream (see
/// [`pmi::probe_tls_reachable`]).
fn real_prober() -> Prober {
    Arc::new(|host, port, tls, timeout| -> ProbeFuture {
        Box::pin(async move { pmi::probe_tls_reachable(&host, port, &tls, timeout).await })
    })
}

/// Outcome of one federation poll, reported back to a control-socket poke so the
/// CLI can tell the user the post-apply state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum FederationOutcome {
    /// Federation is in effect: `Some(ep)` federated to `ep`, `None`
    /// de-federated. Covers both "just applied" and "already in place" (a no-op
    /// poll where the upstream was unchanged). On a login poke this means the TLS
    /// link to the upstream was also verified to validate.
    Applied(Option<String>),
    /// An operator-pinned `ZENOH_CONFIG` owns the router config; nothing changed.
    Pinned,
    /// The resolve or apply failed; the periodic loop will keep retrying.
    Failed(String),
    /// The config was applied (the local router was federated), but the TLS link
    /// to the per-user cloud router could not be established/validated — so
    /// federation with platform-backend is NOT actually in effect (e.g. an
    /// UnknownCA handshake loop). Only a verifying poke produces this.
    Unreachable(String),
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
    prober: Prober,
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
            prober: real_prober(),
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
            prober,
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
                    messenger, resolver, prober, messaging_ready, trigger_rx, ready_tx,
                    connect_timeout,
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
    prober: Prober,
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
    // bounce, even if the user is logged in but the backend is unreachable. The
    // initial poll does not verify (`verify = false`): startup must not block on a
    // TLS handshake, and the verifying check belongs to the login poke.
    let mut applied: Option<String> = None;
    poll_and_apply(
        &messenger,
        &resolver,
        &prober,
        connect_timeout,
        &mut applied,
        false,
    )
    .await;
    fire_gate(&mut ready_tx);

    // Phase 3 — steady state: periodic keepalive/re-resolve, or an immediate poke
    // from `auth login`/`logout`. A single `select!` keeps sole ownership of
    // `applied`, so a poke and a tick can never apply concurrently.
    loop {
        tokio::select! {
            _ = tokio::time::sleep(POLL_INTERVAL) => {
                // Keepalive: cheap, never probes (`verify = false`).
                poll_and_apply(
                    &messenger, &resolver, &prober, connect_timeout, &mut applied, false,
                ).await;
            }
            maybe_req = trigger_rx.recv() => {
                match maybe_req {
                    Some(req) => {
                        // A login/logout poke verifies the link (`verify = true`)
                        // so the CLI learns whether federation actually validates.
                        let outcome = poll_and_apply(
                            &messenger, &resolver, &prober, connect_timeout, &mut applied, true,
                        ).await;
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
        poll_and_apply(
            &messenger,
            &resolver,
            &prober,
            connect_timeout,
            &mut applied,
            false,
        )
        .await;
    }
}

/// One poll: resolve the desired upstream and, if it changed, (re)federate the
/// local router. Updates `*applied` to the upstream now in effect and returns the
/// [`FederationOutcome`] (so a poke can ack the post-apply state).
///
/// When `verify` is set (login/logout pokes only — never the keepalive), and an
/// upstream is in effect, a real TLS handshake confirms the federation link
/// actually validates; a failed handshake is reported as
/// [`FederationOutcome::Unreachable`] (and logged loudly) instead of a false
/// `Applied`.
async fn poll_and_apply(
    messenger: &Arc<Mutex<Messenger>>,
    resolver: &Resolver,
    prober: &Prober,
    connect_timeout: Duration,
    applied: &mut Option<String>,
    verify: bool,
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

    // Apply the change (or note the no-op) and derive the base outcome.
    let outcome = if desired == *applied {
        // Steady state (including the cache-gated keepalive re-pull): the upstream
        // is unchanged, so there is nothing to re-render or restart. Report it as
        // applied so a poke gets a positive "already in place" ack.
        FederationOutcome::Applied(applied.clone())
    } else {
        match refederate_and_restart(messenger, &resolved).await {
            Ok(true) => {
                match &desired {
                    Some(ep) => {
                        info!(upstream = %ep, "router federation: (re)federated local router to cloud router")
                    }
                    None => {
                        info!(
                            "router federation: de-federated local router (logged out / no upstream)"
                        )
                    }
                }
                *applied = desired.clone();
                FederationOutcome::Applied(desired.clone())
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
                warn!(
                    error = %e,
                    "router federation: failed to apply the upstream change, so federation with \
                     the per-user cloud router on platform-backend is NOT in effect; will retry"
                );
                return FederationOutcome::Failed(e.to_string());
            }
        }
    };

    // Verify reachability only on a poke (`verify`), and only when an upstream is
    // actually in effect. The keepalive never reaches here. A failed handshake
    // means the local router was federated but the link to platform-backend does
    // not validate (e.g. UnknownCA) — federation is not really in effect.
    if verify
        && let (FederationOutcome::Applied(Some(ep)), Some((_, tls))) =
            (&outcome, resolved.as_ref())
    {
        match split_endpoint_host_port(ep) {
            Some((host, port)) => {
                // Bound the probe by the small dedicated PROBE_TIMEOUT (NOT
                // connect_timeout) so resolve + bounce + probe stays within the
                // daemon's ack budget; otherwise a slow/unreachable router would
                // blow the budget and surface as a generic ack timeout instead of
                // the actionable Unreachable.
                if let Err(reason) =
                    prober(host.to_string(), port, tls.clone(), PROBE_TIMEOUT).await
                {
                    warn!(
                        upstream = %ep, reason = %reason,
                        "router federation: the local router was (re)federated, but the TLS \
                         link to the per-user cloud router on platform-backend could not be \
                         established — federation with platform-backend is NOT in effect \
                         (check the router certificate / dev CA); will keep retrying"
                    );
                    return FederationOutcome::Unreachable(reason);
                }
            }
            None => {
                warn!(
                    upstream = %ep,
                    "router federation: could not parse the upstream endpoint to verify the \
                     federation link to platform-backend"
                );
                return FederationOutcome::Unreachable(format!(
                    "could not parse upstream endpoint `{ep}`"
                ));
            }
        }
    }

    outcome
}

/// Splits a `<scheme>/<host>:<port>` connect endpoint (e.g.
/// `tls/cap.zenoh.localhost:7443`) into `(host, port)` for the reachability
/// probe. Returns `None` if it isn't in that shape. Hostnames only (the upstream
/// is always a capability/router DNS name, never a bracketed IPv6 literal).
fn split_endpoint_host_port(endpoint: &str) -> Option<(&str, u16)> {
    let after_scheme = endpoint
        .split_once('/')
        .map_or(endpoint, |(_scheme, rest)| rest);
    let (host, port) = after_scheme.rsplit_once(':')?;
    if host.is_empty() {
        return None;
    }
    let port: u16 = port.parse().ok()?;
    Some((host, port))
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
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

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

    /// A prober returning a fixed result and counting its calls, so a test can
    /// assert both *whether* the link was probed and the resulting outcome.
    fn counting_prober(result: std::result::Result<(), String>) -> (Prober, Arc<AtomicUsize>) {
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = calls.clone();
        let prober: Prober = Arc::new(move |_host, _port, _tls, _timeout| -> ProbeFuture {
            counter.fetch_add(1, Ordering::SeqCst);
            let result = result.clone();
            Box::pin(async move { result })
        });
        (prober, calls)
    }

    /// A prober that records (in millis) the timeout it was invoked with, so a
    /// test can assert the probe is bounded by `PROBE_TIMEOUT`, not the larger
    /// `connect_timeout`.
    fn timeout_capturing_prober() -> (Prober, Arc<AtomicU64>) {
        let seen = Arc::new(AtomicU64::new(0));
        let rec = seen.clone();
        let prober: Prober = Arc::new(move |_h, _p, _tls, timeout| -> ProbeFuture {
            rec.store(timeout.as_millis() as u64, Ordering::SeqCst);
            Box::pin(async { Ok(()) })
        });
        (prober, seen)
    }

    fn upstream() -> Option<(String, pmi::TlsConfig)> {
        Some((ENDPOINT.to_string(), pmi::TlsConfig::default()))
    }

    #[test]
    fn splits_scheme_host_and_port() {
        assert_eq!(
            split_endpoint_host_port("tls/cap.zenoh.localhost:7443"),
            Some(("cap.zenoh.localhost", 7443))
        );
        assert_eq!(split_endpoint_host_port("host:1"), Some(("host", 1)));
        assert_eq!(split_endpoint_host_port("tls/host"), None);
        assert_eq!(split_endpoint_host_port("tls/:7443"), None);
        assert_eq!(split_endpoint_host_port("tls/host:notaport"), None);
    }

    /// A login/logout poke runs a federation poll *immediately* (well within the
    /// 30s `POLL_INTERVAL`), verifies the link, and acks the applied outcome —
    /// the whole point of the control channel. The initial (non-poke) poll does
    /// NOT probe; only the verifying poke does.
    #[tokio::test]
    async fn poke_refederates_immediately_verifies_and_acks() {
        let messenger = mock_messenger();
        let (resolver, calls) = counting_resolver(upstream());
        let (prober, probe_calls) = counting_prober(Ok(()));
        // Router already up.
        let (messaging_tx, messaging_rx) = watch::channel(true);
        let (trigger_tx, trigger_rx) = mpsc::channel(8);
        let (ready_tx, ready_rx) = oneshot::channel();

        let task = tokio::spawn(manage_federation(
            messenger,
            resolver,
            prober,
            messaging_rx,
            trigger_rx,
            ready_tx,
            Duration::from_secs(5),
        ));

        // Startup gate fires after the first (initial) poll; that poll resolved
        // once and, being a non-poke poll, did NOT probe the link.
        tokio::time::timeout(Duration::from_secs(1), ready_rx)
            .await
            .expect("startup gate fires promptly")
            .expect("gate sender not dropped");
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "the initial poll resolved once"
        );
        assert_eq!(
            probe_calls.load(Ordering::SeqCst),
            0,
            "the initial (non-poke) poll must not probe the link"
        );

        // Poke: must run a second resolve immediately, probe the link, and ack —
        // not wait out the 30s poll interval.
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
        // re-resolves, the probe succeeds, and it reports applied.
        assert_eq!(
            outcome,
            FederationOutcome::Applied(Some(ENDPOINT.to_string()))
        );
        assert_eq!(
            calls.load(Ordering::SeqCst),
            2,
            "the poke ran a second resolve"
        );
        assert_eq!(
            probe_calls.load(Ordering::SeqCst),
            1,
            "the poke probed the link exactly once"
        );

        drop(messaging_tx);
        task.abort();
    }

    /// The verifying poke bounds the probe by the small `PROBE_TIMEOUT`, not the
    /// (potentially large) `connect_timeout`, so resolve + bounce + probe stays
    /// within the daemon's ack budget. Regression guard for the latency-budget bug.
    #[tokio::test]
    async fn poke_probes_with_the_bounded_probe_timeout() {
        let messenger = mock_messenger();
        let (resolver, _) = counting_resolver(upstream());
        let (prober, seen) = timeout_capturing_prober();
        let (messaging_tx, messaging_rx) = watch::channel(true);
        let (trigger_tx, trigger_rx) = mpsc::channel(8);
        let (ready_tx, ready_rx) = oneshot::channel();

        let task = tokio::spawn(manage_federation(
            messenger,
            resolver,
            prober,
            messaging_rx,
            trigger_rx,
            ready_tx,
            // A deliberately large connect_timeout: the probe must NOT inherit it.
            Duration::from_secs(45),
        ));
        tokio::time::timeout(Duration::from_secs(1), ready_rx)
            .await
            .expect("startup gate fires")
            .expect("gate sender not dropped");

        let (ack_tx, ack_rx) = oneshot::channel();
        trigger_tx
            .send(RefederateRequest { ack: ack_tx })
            .await
            .expect("trigger accepted");
        tokio::time::timeout(Duration::from_secs(1), ack_rx)
            .await
            .expect("poke serviced")
            .expect("ack sender not dropped");

        assert_eq!(
            seen.load(Ordering::SeqCst),
            PROBE_TIMEOUT.as_millis() as u64,
            "the probe must be bounded by PROBE_TIMEOUT, not connect_timeout"
        );

        drop(messaging_tx);
        task.abort();
    }

    /// A login poke whose TLS link does not validate (e.g. UnknownCA loop) is
    /// reported as `Unreachable(reason)`, not a false `Applied` — even though the
    /// config was applied.
    #[tokio::test]
    async fn poke_with_failing_probe_reports_unreachable() {
        let messenger = mock_messenger();
        let (resolver, _calls) = counting_resolver(upstream());
        let reason = "received fatal alert: UnknownCA";
        let (prober, probe_calls) = counting_prober(Err(reason.to_string()));
        let (messaging_tx, messaging_rx) = watch::channel(true);
        let (trigger_tx, trigger_rx) = mpsc::channel(8);
        let (ready_tx, ready_rx) = oneshot::channel();

        let task = tokio::spawn(manage_federation(
            messenger,
            resolver,
            prober,
            messaging_rx,
            trigger_rx,
            ready_tx,
            Duration::from_secs(5),
        ));

        tokio::time::timeout(Duration::from_secs(1), ready_rx)
            .await
            .expect("startup gate fires")
            .expect("gate sender not dropped");

        let (ack_tx, ack_rx) = oneshot::channel();
        trigger_tx
            .send(RefederateRequest { ack: ack_tx })
            .await
            .expect("trigger accepted");
        let outcome = tokio::time::timeout(Duration::from_secs(1), ack_rx)
            .await
            .expect("poke serviced immediately")
            .expect("ack sender not dropped");

        assert_eq!(
            outcome,
            FederationOutcome::Unreachable(reason.to_string()),
            "a failing probe ⇒ Unreachable"
        );
        assert_eq!(
            probe_calls.load(Ordering::SeqCst),
            1,
            "the poke probed once"
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
        let (prober, _) = counting_prober(Ok(()));
        let (messaging_tx, messaging_rx) = watch::channel(true);
        let (_trigger_tx, trigger_rx) = mpsc::channel(8);
        let (ready_tx, ready_rx) = oneshot::channel();

        let task = tokio::spawn(manage_federation(
            messenger,
            resolver,
            prober,
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
    /// when nothing was federated, the gate still fires, and nothing is probed.
    #[tokio::test]
    async fn logged_out_initial_poll_is_a_noop_and_fires_the_gate() {
        let messenger = mock_messenger();
        let (resolver, calls) = counting_resolver(None);
        let (prober, probe_calls) = counting_prober(Ok(()));
        let (messaging_tx, messaging_rx) = watch::channel(true);
        let (_trigger_tx, trigger_rx) = mpsc::channel(8);
        let (ready_tx, ready_rx) = oneshot::channel();

        let task = tokio::spawn(manage_federation(
            messenger,
            resolver,
            prober,
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
        assert_eq!(
            probe_calls.load(Ordering::SeqCst),
            0,
            "no upstream ⇒ nothing to probe"
        );

        drop(messaging_tx);
        task.abort();
    }
}
