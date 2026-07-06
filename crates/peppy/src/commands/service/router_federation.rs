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
//!   place, bounded by `connect_timeout` so a slow/unreachable backend can't
//!   stall startup past it (the daemon then proceeds standalone and keeps
//!   retrying in the background). At the same moments it fires the core node's
//!   *probe gate* (`probe_gate_tx`): the core node delays its boot-time
//!   name-collision self-probe until the initial federation has settled, so the
//!   probe sees the federated mesh (a same-name daemon reachable only through
//!   the cloud router refuses boot) rather than the always-standalone
//!   just-started local router.
//! * **Immediate (re)federation on login/logout.** `peppy auth login`/`logout`
//!   poke the daemon over the control socket
//!   ([`FederationControl`](super::federation_control)); the poke is delivered
//!   here as a [`RefederateRequest`] that runs a poll *now* (not on the next
//!   interval) and acks the resulting [`FederationOutcome`] so the CLI knows
//!   federation is in place before it returns. A login poke additionally
//!   *verifies* the federation link with a real TLS handshake
//!   ([`pmi::probe_tls_reachable`]) so a silent UnknownCA loop is reported as
//!   [`FederationOutcome::Unreachable`] rather than a false success.
//! * **Liveness (backend-driven).** There is no client-side keepalive poll. Once
//!   federated, the local router holds its link to the cloud router open on its
//!   own (`reconnect: true`); the backend actively probes this daemon's `/health`
//!   service over the federated link and tears the cloud router down when the
//!   daemon stops answering. The config pull on startup/login tells the backend
//!   this daemon's `core_node` name so it knows which `/health` service to probe.
//! * **Registration cadence.** Every config pull's POST carries this daemon's
//!   core-node name, upserting it into the backend's per-principal core-node
//!   registry. The POST fires on cache-stale pulls and on login/logout pokes —
//!   login clears the router cache, so every login re-registers — never on a
//!   timer. The backend's `last_seen_at` for a core node therefore means "last
//!   federation config pull", not liveness.
//! * **Live (re)federation.** When the resolved upstream changes (the user logs
//!   in, logs out, or the endpoint moves) the local router's zenohd config is
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
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

/// How long a *verifying* login/logout poke waits for the federation link's TLS
/// handshake to validate. Deliberately small and decoupled from `connect_timeout`
/// (the resolve bound): a healthy handshake is sub-second, so a tight bound keeps
/// the whole verifying poll (resolve + zenohd bounce + probe) inside the daemon's
/// ack budget: `connect_timeout` + [`super::federation_control`]'s
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

/// The future a [`Federator`] returns: `Ok(true)` ⇒ the local router's config was
/// (re)rendered and zenohd bounced, `Ok(false)` ⇒ nothing was applied (an operator
/// `ZENOH_CONFIG` pins the config), `Err` ⇒ the apply failed.
type FederateFuture = Pin<Box<dyn Future<Output = Result<bool>> + Send>>;

/// Applies a desired upstream to the local router (re-render + bounce). A boxed
/// async closure so tests can inject a deterministic federation result:
/// `Ok(true)` (a real rewrite) or `Ok(false)` (operator-pinned), in place of the
/// real [`refederate_and_restart`], whose mock backend can only ever report
/// `Ok(false)` and so cannot exercise the applied/verify path.
type Federator = Arc<dyn Fn(Option<(String, pmi::TlsConfig)>) -> FederateFuture + Send + Sync>;

/// The real federator: re-render the owned router's config with the upstream and,
/// if it changed, bounce zenohd (see [`refederate_and_restart`]).
fn real_federator(messenger: Arc<Mutex<Messenger>>) -> Federator {
    Arc::new(move |target| -> FederateFuture {
        let messenger = messenger.clone();
        Box::pin(async move { refederate_and_restart(&messenger, &target).await })
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
    /// to the per-user cloud router could not be established/validated, so
    /// federation with platform-backend is NOT actually in effect (e.g. an
    /// UnknownCA handshake loop). Only a verifying poke produces this.
    Unreachable(String),
    /// The credentials changed the daemon's *organization namespace*. A session's
    /// namespace is immutable after open and the core node holds long-lived
    /// declarations, so the change cannot be applied to the live session by a
    /// zenohd bounce; it needs a full daemon-generation restart. This poll does
    /// NOT (de)federate (federating under a namespace that differs from the live
    /// session's would leak across tenants); it just signals the restart. The
    /// control handler owns triggering it (after flushing the ack); the federation
    /// loop only reports it.
    Restart,
}

/// Resolves the daemon's *current* organization namespace from the credentials
/// (after a federation pull has warmed the cache), so the federation loop can
/// compare it to the generation's startup namespace. A boxed closure so tests can
/// inject a deterministic value in place of the real credentials read.
type NamespaceResolver = Arc<dyn Fn() -> String + Send + Sync>;

/// The real namespace resolver: read the cached organization id and resolve it to
/// a namespace (absent -> `local`), matching exactly how the daemon generation
/// resolved its own namespace at startup.
fn real_namespace_resolver() -> NamespaceResolver {
    Arc::new(|| {
        config::org::resolve_session_namespace(
            auth::router::cached_organization_id_default().as_deref(),
        )
        .as_str()
        .to_string()
    })
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
    federator: Federator,
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
    /// This generation's organization namespace, resolved once at startup. A poll
    /// that re-resolves a *different* namespace from fresh creds requests a
    /// restart instead of a live re-federation.
    startup_namespace: String,
    /// Resolves the current namespace from the credentials (post-pull), compared
    /// against `startup_namespace` to detect a namespace change.
    namespace_resolver: NamespaceResolver,
    /// In-process restart signal (the serve coordinator's). The *startup*
    /// federation poll raises it when it discovers the credentials already resolve
    /// to a different namespace than this generation started under, so the live
    /// session (which can't be re-namespaced) is rebuilt rather than left running
    /// un-federated. The steady-state poke path instead acks `Restart` and the
    /// control handler raises this same signal after flushing the ack.
    restart_tx: watch::Sender<bool>,
    /// Fired (`true`) at the same moments as the startup readiness gate: once
    /// the *initial* federation poll has settled (or the bound elapsed / the
    /// router never came up). The core node waits on it before running its
    /// boot-time name-collision self-probe, so the probe sees the federated
    /// mesh instead of the always-standalone just-started router. `None` when
    /// no core node was built (nothing to probe).
    probe_gate_tx: Option<watch::Sender<bool>>,
    /// Shared coordinator token: the task tears down when it is cancelled (an
    /// in-process restart) or on a real OS shutdown signal.
    teardown_token: CancellationToken,
}

impl RouterFederation {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        messenger: Arc<Mutex<Messenger>>,
        api_url: String,
        core_node_name: String,
        messaging_ready: watch::Receiver<bool>,
        trigger_rx: TriggerReceiver,
        connect_timeout: Duration,
        startup_namespace: String,
        restart_tx: watch::Sender<bool>,
        probe_gate_tx: Option<watch::Sender<bool>>,
        teardown_token: CancellationToken,
    ) -> Self {
        let resolver: Resolver = Arc::new(move || {
            auth::router::resolve_federation_target(
                &api_url,
                connect_timeout,
                &core_node_name,
            )
        });
        Self {
            federator: real_federator(messenger),
            resolver,
            prober: real_prober(),
            messaging_ready,
            trigger_rx,
            connect_timeout,
            startup_namespace,
            namespace_resolver: real_namespace_resolver(),
            restart_tx,
            probe_gate_tx,
            teardown_token,
        }
    }
}

impl ServeAsyncCommand for RouterFederation {
    fn run(self: Box<Self>) -> ServeAsyncHandle {
        let RouterFederation {
            federator,
            resolver,
            prober,
            messaging_ready,
            trigger_rx,
            connect_timeout,
            startup_namespace,
            namespace_resolver,
            restart_tx,
            probe_gate_tx,
            teardown_token,
        } = *self;
        // Readiness gate: fired by `manage_federation` once the first federation
        // poll completes (or the timeout elapses), so `serve` blocks on federation
        // being in place but never longer than `connect_timeout`. `serve` treats
        // this gate as non-fatal, so a drop here (e.g. shutdown wins the race
        // below before it fires) degrades to "proceed standalone", never a crash.
        let (ready_tx, ready_rx) = oneshot::channel();
        let future = Box::pin(async move {
            // Race the maintenance loop against shutdown (a real signal or an
            // in-process restart via the shared token) so the daemon can exit
            // promptly (the loop is otherwise infinite).
            tokio::select! {
                _ = manage_federation(
                    federator, resolver, prober, messaging_ready, trigger_rx, ready_tx,
                    connect_timeout, startup_namespace, namespace_resolver, restart_tx,
                    probe_gate_tx,
                ) => {}
                _ = super::shutdown_signal::shutdown_or_token(&teardown_token) => {}
            }
            Ok(())
        });
        ServeAsyncHandle::new_optional_ready(future, ready_rx)
    }
}

/// Fires the startup readiness gate exactly once (idempotent: the `Option` is
/// `take`n, so later calls or a drop are no-ops) and, in lockstep, the core
/// node's probe gate (a `watch`, so re-sends are harmless). The two fire at the
/// same moments so the core node's boot self-probe is delayed exactly as long
/// as `serve`'s own readiness: until the initial federation settled, bounded by
/// `connect_timeout`, and fail-open (a dropped sender ⇒ the waiter proceeds).
fn fire_gate(gate: &mut Option<oneshot::Sender<()>>, probe_gate: &Option<watch::Sender<bool>>) {
    if let Some(tx) = gate.take() {
        let _ = tx.send(());
    }
    if let Some(tx) = probe_gate {
        let _ = tx.send(true);
    }
}

/// What the last completed poll left in effect, cached across polls so an
/// identical repeat (the same desired upstream) is answered from the fast path
/// without re-applying. Richer than the bare endpoint string: it also remembers
/// whether the upstream is *operator-pinned* (an operator `ZENOH_CONFIG` owns the
/// router config, so we did not actually apply it). Without the `pinned` bit a
/// repeat of a pinned target would match on endpoint alone and be misreported as
/// [`FederationOutcome::Applied`] instead of [`FederationOutcome::Pinned`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct AppliedState {
    /// The upstream now in effect: `Some(ep)` federated to `ep`, `None`
    /// de-federated / nothing federated.
    endpoint: Option<String>,
    /// Whether the config is operator-pinned (so the desired change was not
    /// applied here), replayed so identical repeats stay `Pinned`.
    pinned: bool,
}

/// Waits for the router to come up, runs the initial federation (firing the
/// startup gate when it completes or the timeout elapses), then services
/// immediate login/logout pokes for the daemon's lifetime (the caller races it
/// against the shutdown signal). There is no periodic keepalive: once federated,
/// the local router holds its upstream link open on its own and the backend
/// actively health-checks this daemon.
#[allow(clippy::too_many_arguments)]
async fn manage_federation(
    federator: Federator,
    resolver: Resolver,
    prober: Prober,
    mut messaging_ready: watch::Receiver<bool>,
    mut trigger_rx: TriggerReceiver,
    ready_tx: oneshot::Sender<()>,
    connect_timeout: Duration,
    startup_namespace: String,
    namespace_resolver: NamespaceResolver,
    restart_tx: watch::Sender<bool>,
    probe_gate_tx: Option<watch::Sender<bool>>,
) {
    let mut ready_tx = Some(ready_tx);

    // Phase 1: wait for the router, bounded by `connect_timeout`. Don't touch the
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
            fire_gate(&mut ready_tx, &probe_gate_tx);
            return;
        }
        Err(_elapsed) => {
            // The router isn't up within the bound: unblock startup now (the
            // daemon proceeds standalone), then keep waiting (unbounded) so the
            // local router still federates once it does come up.
            fire_gate(&mut ready_tx, &probe_gate_tx);
            if messaging_ready.wait_for(|r| *r).await.is_err() {
                return;
            }
        }
    }

    // Phase 2: initial federation. The router was started standalone, so nothing
    // is federated yet. The resolve inside is itself bounded by `connect_timeout`,
    // so this completes (and unblocks startup) within the bound plus a fast local
    // bounce, even if the user is logged in but the backend is unreachable. The
    // initial poll does not verify (`verify = false`): startup must not block on a
    // TLS handshake, and the verifying check belongs to the login poke.
    let mut applied = AppliedState::default();
    let initial_outcome = poll_and_apply(
        &federator,
        &resolver,
        &prober,
        connect_timeout,
        &mut applied,
        false,
        &startup_namespace,
        &namespace_resolver,
    )
    .await;
    fire_gate(&mut ready_tx, &probe_gate_tx);

    // The initial poll re-pulled the federation config, so the credentials now
    // reflect the current org. If that resolves to a *different* namespace than
    // this generation started under (e.g. the daemon started logged-in but with a
    // cleared/stale router cache, so `startup_namespace` was `local` before the
    // pull discovered the real org), the live session can't be re-namespaced.
    // Request a generation restart now; otherwise the daemon would run
    // un-federated under the wrong namespace until the next login/logout poke. The
    // steady-state poke path leaves the actual restart to the control handler
    // (which flushes its ack first); the startup poll has no ack to flush, so it
    // raises the signal directly. The rebuilt generation resolves the namespace
    // afresh and federates normally.
    if matches!(initial_outcome, FederationOutcome::Restart) {
        info!(
            "router federation: startup resolved a namespace that differs from this generation's; \
             requesting a daemon restart instead of federating under the wrong namespace"
        );
        let _ = restart_tx.send(true);
        return;
    }

    // Phase 3, steady state: react to immediate login/logout pokes from `auth
    // login`/`logout`. There is no periodic keepalive: the local router keeps its
    // upstream link alive on its own (`reconnect: true`), and the backend now
    // probes this daemon's `/health` service for liveness, so re-resolving on a
    // timer is no longer needed. When every trigger sender drops (the control
    // listener is gone, only at teardown in practice) there is nothing left to
    // react to, so the loop ends and the task exits.
    while let Some(req) = trigger_rx.recv().await {
        // A login/logout poke verifies the link (`verify = true`) so the CLI
        // learns whether federation actually validates.
        let outcome = poll_and_apply(
            &federator,
            &resolver,
            &prober,
            connect_timeout,
            &mut applied,
            true,
            &startup_namespace,
            &namespace_resolver,
        )
        .await;
        // The CLI may have already given up (read timeout); ignore. On a namespace
        // change this acks `Restart`; the control handler (`handle_conn`) flushes
        // that ack and only then raises the in-process restart signal, so the
        // restart is never triggered from this loop.
        let _ = req.ack.send(outcome);
    }
}

/// One poll: resolve the desired upstream and, if it changed, (re)federate the
/// local router. Updates `*applied` to the upstream now in effect and returns the
/// [`FederationOutcome`] (so a poke can ack the post-apply state).
///
/// When `verify` is set (login/logout pokes only, not the initial startup
/// federation), and an upstream is in effect, a real TLS handshake confirms the link
/// actually validates; a failed handshake is reported as
/// [`FederationOutcome::Unreachable`] (and logged loudly) instead of a false
/// `Applied`.
#[allow(clippy::too_many_arguments)]
async fn poll_and_apply(
    federator: &Federator,
    resolver: &Resolver,
    prober: &Prober,
    connect_timeout: Duration,
    applied: &mut AppliedState,
    verify: bool,
    startup_namespace: &str,
    namespace_resolver: &NamespaceResolver,
) -> FederationOutcome {
    // The resolver is blocking (HTTP + file I/O); keep it off the async worker. It
    // also re-pulls the cloud router's config when the cached copy has gone stale
    // (cache freshness only, not a keepalive). Bound the whole resolve by
    // `connect_timeout` so a hung pull can't
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

    // Namespace-change gate. The resolve above re-pulled (and re-cached) the
    // federation config, so the credentials now reflect the current org id. A
    // session's namespace is immutable after open, so if the re-resolved namespace
    // differs from this generation's startup namespace the change cannot be applied
    // by a live zenodh bounce: request a full restart instead, WITHOUT federating
    // (federating under a namespace that differs from the live session's would leak
    // across tenants). The control handler flushes the ack before triggering the
    // restart; the initial (non-poke) poll discards this outcome but, crucially,
    // also does not federate, so it stays fail-closed until the next generation.
    let current_namespace = namespace_resolver();
    if current_namespace != startup_namespace {
        info!(
            from = %startup_namespace,
            to = %current_namespace,
            "router federation: organization namespace changed; requesting a daemon restart \
             (a namespace change cannot be applied to a live session)"
        );
        return FederationOutcome::Restart;
    }

    let desired = resolved.as_ref().map(|(ep, _)| ep.clone());

    // Apply the change (or note the no-op) and derive the base outcome.
    let outcome = if desired == applied.endpoint {
        // Steady state (including the cache-gated re-pull): the upstream is
        // unchanged, so there is nothing to re-render or restart. Replay the last
        // outcome, crucially preserving `Pinned`, so an identical repeat of an
        // operator-pinned target is not misreported as a positive `Applied`.
        if applied.pinned {
            FederationOutcome::Pinned
        } else {
            FederationOutcome::Applied(applied.endpoint.clone())
        }
    } else {
        match federator(resolved.clone()).await {
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
                *applied = AppliedState {
                    endpoint: desired.clone(),
                    pinned: false,
                };
                FederationOutcome::Applied(desired.clone())
            }
            Ok(false) => {
                // An operator-pinned `ZENOH_CONFIG` owns the router config, so the
                // desired change cannot be applied here. Advance `applied` (endpoint
                // *and* the pinned bit) so this is noted once per change
                // (login/logout) rather than every poll, and so an identical repeat
                // replays `Pinned`; warn so the operator knows federation is not
                // being auto-managed.
                warn!(
                    "router federation: ZENOH_CONFIG pins the router config; the desired \
                     federation change was not applied (the operator owns this router's config)"
                );
                *applied = AppliedState {
                    endpoint: desired,
                    pinned: true,
                };
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
    // actually in effect. The non-verifying initial federation never reaches here.
    // A failed handshake means the local router was federated but the link to
    // platform-backend does not validate (e.g. UnknownCA), so federation is not
    // really in effect.
    if verify
        && let (FederationOutcome::Applied(Some(ep)), Some((_, tls))) =
            (&outcome, resolved.as_ref())
    {
        match auth::client::split_locator(ep).ok() {
            Some((host, port)) => {
                // Bound the probe by the small dedicated PROBE_TIMEOUT (NOT
                // connect_timeout) so resolve + bounce + probe stays within the
                // daemon's ack budget; otherwise a slow/unreachable router would
                // blow the budget and surface as a generic ack timeout instead of
                // the actionable Unreachable.
                if let Err(reason) = prober(host, port, tls.clone(), PROBE_TIMEOUT).await {
                    warn!(
                        upstream = %ep, reason = %reason,
                        "router federation: the local router was (re)federated, but the TLS \
                         link to the per-user cloud router on platform-backend could not be \
                         established; federation with platform-backend is NOT in effect \
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
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

    const ENDPOINT: &str = "tls/cap.zenoh.localhost:7443";

    /// A federator simulating a real (non-pinned) rewrite: it reports the config
    /// was rewritten (`Ok(true)`), so the poll treats the upstream as actually
    /// applied, the path the verify/probe logic exercises. (The mock messenger's
    /// real `refederate` can only ever report `Ok(false)`, i.e. pinned, so the
    /// applied path is reachable in tests only via an injected federator.)
    fn applying_federator() -> Federator {
        Arc::new(|_target| -> FederateFuture { Box::pin(async { Ok(true) }) })
    }

    /// A federator simulating an operator-pinned config: `refederate` reports no
    /// rewrite (`Ok(false)`), so the poll classifies the outcome as `Pinned`.
    fn pinned_federator() -> Federator {
        Arc::new(|_target| -> FederateFuture { Box::pin(async { Ok(false) }) })
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

    /// A namespace resolver that always returns `local`, matching the `local`
    /// startup namespace these tests pass, so no namespace change is detected and
    /// the existing federation behavior (Applied/Pinned/...) is exercised.
    fn local_ns_resolver() -> NamespaceResolver {
        Arc::new(|| "local".to_string())
    }

    /// A namespace resolver returning a fixed value and counting its calls, for a
    /// test that exercises the namespace-change restart path.
    fn counting_ns_resolver(value: &str) -> (NamespaceResolver, Arc<AtomicUsize>) {
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = calls.clone();
        let value = value.to_string();
        let resolver: NamespaceResolver = Arc::new(move || {
            counter.fetch_add(1, Ordering::SeqCst);
            value.clone()
        });
        (resolver, calls)
    }

    /// A namespace resolver that returns `first` on its first call and `rest`
    /// after, so the *startup* poll sees the unchanged namespace (no startup
    /// restart) and a later *poke* sees the change, exercising the steady-state
    /// `Restart` ack distinctly from the startup restart path.
    fn switching_ns_resolver(first: &str, rest: &str) -> NamespaceResolver {
        let calls = AtomicUsize::new(0);
        let first = first.to_string();
        let rest = rest.to_string();
        Arc::new(move || {
            if calls.fetch_add(1, Ordering::SeqCst) == 0 {
                first.clone()
            } else {
                rest.clone()
            }
        })
    }

    /// A login/logout poke runs a federation poll *immediately*, verifies the
    /// link, and acks the applied outcome, the whole point of the control
    /// channel. The initial (non-poke) poll does NOT probe; only the verifying
    /// poke does.
    #[tokio::test]
    async fn poke_refederates_immediately_verifies_and_acks() {
        let (resolver, calls) = counting_resolver(upstream());
        let (prober, probe_calls) = counting_prober(Ok(()));
        // Router already up.
        let (messaging_tx, messaging_rx) = watch::channel(true);
        let (trigger_tx, trigger_rx) = mpsc::channel(8);
        let (ready_tx, ready_rx) = oneshot::channel();

        let task = tokio::spawn(manage_federation(
            applying_federator(),
            resolver,
            prober,
            messaging_rx,
            trigger_rx,
            ready_tx,
            Duration::from_secs(5),
            "local".to_string(),
            local_ns_resolver(),
            watch::channel(false).0,
            None,
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

        // Poke: must run a second resolve immediately, probe the link, and ack.
        let (ack_tx, ack_rx) = oneshot::channel();
        trigger_tx
            .send(RefederateRequest { ack: ack_tx })
            .await
            .expect("trigger accepted");
        let outcome = tokio::time::timeout(Duration::from_secs(1), ack_rx)
            .await
            .expect("the poke is serviced immediately")
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
        let (resolver, _) = counting_resolver(upstream());
        let (prober, seen) = timeout_capturing_prober();
        let (messaging_tx, messaging_rx) = watch::channel(true);
        let (trigger_tx, trigger_rx) = mpsc::channel(8);
        let (ready_tx, ready_rx) = oneshot::channel();

        let task = tokio::spawn(manage_federation(
            applying_federator(),
            resolver,
            prober,
            messaging_rx,
            trigger_rx,
            ready_tx,
            // A deliberately large connect_timeout: the probe must NOT inherit it.
            Duration::from_secs(45),
            "local".to_string(),
            local_ns_resolver(),
            watch::channel(false).0,
            None,
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
    /// reported as `Unreachable(reason)`, not a false `Applied`, even though the
    /// config was applied.
    #[tokio::test]
    async fn poke_with_failing_probe_reports_unreachable() {
        let (resolver, _calls) = counting_resolver(upstream());
        let reason = "received fatal alert: UnknownCA";
        let (prober, probe_calls) = counting_prober(Err(reason.to_string()));
        let (messaging_tx, messaging_rx) = watch::channel(true);
        let (trigger_tx, trigger_rx) = mpsc::channel(8);
        let (ready_tx, ready_rx) = oneshot::channel();

        let task = tokio::spawn(manage_federation(
            applying_federator(),
            resolver,
            prober,
            messaging_rx,
            trigger_rx,
            ready_tx,
            Duration::from_secs(5),
            "local".to_string(),
            local_ns_resolver(),
            watch::channel(false).0,
            None,
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

    /// An operator-pinned config (`refederate` reports no rewrite) must keep
    /// reporting `Pinned` on an *identical* repeat, not flip to `Applied`. The
    /// cached state has to remember the pinned bit alongside the endpoint;
    /// otherwise the fast path matches on endpoint alone and misreports `Applied`
    /// (and would then needlessly probe). Regression guard for that cache.
    #[tokio::test]
    async fn poke_on_pinned_config_stays_pinned_and_does_not_probe() {
        let (resolver, _calls) = counting_resolver(upstream());
        let (prober, probe_calls) = counting_prober(Ok(()));
        let (messaging_tx, messaging_rx) = watch::channel(true);
        let (trigger_tx, trigger_rx) = mpsc::channel(8);
        let (ready_tx, ready_rx) = oneshot::channel();

        let task = tokio::spawn(manage_federation(
            pinned_federator(),
            resolver,
            prober,
            messaging_rx,
            trigger_rx,
            ready_tx,
            Duration::from_secs(5),
            "local".to_string(),
            local_ns_resolver(),
            watch::channel(false).0,
            None,
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
            FederationOutcome::Pinned,
            "an identical repeat of a pinned target must stay Pinned, not Applied"
        );
        assert_eq!(
            probe_calls.load(Ordering::SeqCst),
            0,
            "a pinned outcome is never probed"
        );

        drop(messaging_tx);
        task.abort();
    }

    /// The startup gate fires within the timeout even when the backend is slow
    /// enough to blow the bound, so a hung backend can never stall `serve` past
    /// `connect_timeout`. The federation loop then keeps retrying. The core
    /// node's probe gate fires in the same breath, so a slow backend cannot
    /// stall the boot self-probe (and thus listener binding) either.
    #[tokio::test]
    async fn startup_gate_fires_within_timeout_when_resolve_is_slow() {
        // Resolver sleeps past the (short) connect timeout, so the bounded resolve
        // elapses and the first poll completes as a failure; the gate must still
        // fire.
        let resolver: Resolver = Arc::new(|| {
            std::thread::sleep(Duration::from_millis(400));
            None
        });
        let (prober, _) = counting_prober(Ok(()));
        let (messaging_tx, messaging_rx) = watch::channel(true);
        let (_trigger_tx, trigger_rx) = mpsc::channel(8);
        let (ready_tx, ready_rx) = oneshot::channel();
        let (probe_gate_tx, mut probe_gate_rx) = watch::channel(false);

        let task = tokio::spawn(manage_federation(
            applying_federator(),
            resolver,
            prober,
            messaging_rx,
            trigger_rx,
            ready_tx,
            Duration::from_millis(100),
            "local".to_string(),
            local_ns_resolver(),
            watch::channel(false).0,
            Some(probe_gate_tx),
        ));

        // Gate fires close to the 100ms bound, well before the 400ms resolve.
        tokio::time::timeout(Duration::from_secs(1), ready_rx)
            .await
            .expect("gate fires within the timeout despite a slow backend")
            .expect("gate sender not dropped");
        // The probe gate fires in lockstep, so the core node boots (standalone)
        // rather than waiting on the hung backend.
        tokio::time::timeout(Duration::from_secs(1), probe_gate_rx.wait_for(|g| *g))
            .await
            .expect("probe gate fires within the timeout despite a slow backend")
            .expect("probe gate sender not dropped");

        drop(messaging_tx);
        task.abort();
    }

    /// The core node's probe gate opens only after the *initial* federation poll
    /// has settled (in lockstep with the startup gate), so the boot name
    /// self-probe runs against the federated mesh rather than the
    /// always-standalone just-started router.
    #[tokio::test]
    async fn probe_gate_fires_once_the_initial_federation_settled() {
        let (resolver, calls) = counting_resolver(upstream());
        let (prober, _) = counting_prober(Ok(()));
        let (messaging_tx, messaging_rx) = watch::channel(true);
        let (_trigger_tx, trigger_rx) = mpsc::channel(8);
        let (ready_tx, ready_rx) = oneshot::channel();
        let (probe_gate_tx, mut probe_gate_rx) = watch::channel(false);

        let task = tokio::spawn(manage_federation(
            applying_federator(),
            resolver,
            prober,
            messaging_rx,
            trigger_rx,
            ready_tx,
            Duration::from_secs(5),
            "local".to_string(),
            local_ns_resolver(),
            watch::channel(false).0,
            Some(probe_gate_tx),
        ));

        tokio::time::timeout(Duration::from_secs(1), probe_gate_rx.wait_for(|g| *g))
            .await
            .expect("probe gate fires promptly")
            .expect("probe gate sender not dropped");
        // The generous timeout means the gate fired via the initial-poll path,
        // so the federation had already been resolved (and applied) when the
        // gate opened.
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "the initial federation poll settled before the probe gate opened"
        );
        // The startup gate fired in the same breath.
        tokio::time::timeout(Duration::from_secs(1), ready_rx)
            .await
            .expect("startup gate fires with the probe gate")
            .expect("gate sender not dropped");

        drop(messaging_tx);
        task.abort();
    }

    /// A poll with no upstream (logged out / pull failed) reports `Applied(None)`
    /// when nothing was federated, the gate still fires, and nothing is probed.
    #[tokio::test]
    async fn logged_out_initial_poll_is_a_noop_and_fires_the_gate() {
        let (resolver, calls) = counting_resolver(None);
        let (prober, probe_calls) = counting_prober(Ok(()));
        let (messaging_tx, messaging_rx) = watch::channel(true);
        let (_trigger_tx, trigger_rx) = mpsc::channel(8);
        let (ready_tx, ready_rx) = oneshot::channel();

        let task = tokio::spawn(manage_federation(
            applying_federator(),
            resolver,
            prober,
            messaging_rx,
            trigger_rx,
            ready_tx,
            Duration::from_secs(5),
            "local".to_string(),
            local_ns_resolver(),
            watch::channel(false).0,
            None,
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

    /// A poke after the credentials change the daemon's namespace acks `Restart`
    /// (the control handler then triggers a generation restart). The loop must NOT
    /// federate or probe on a namespace change; a restart is fail-closed. The
    /// change appears only at the poke (the startup poll still sees `local`), so the
    /// startup-restart path stays dormant and the steady-state ack is exercised.
    #[tokio::test]
    async fn poke_acks_restart_on_a_namespace_change() {
        let (resolver, _calls) = counting_resolver(upstream());
        let (prober, probe_calls) = counting_prober(Ok(()));
        // Startup resolves `local` (matches the startup namespace ⇒ no startup
        // restart); the poke resolves the changed org id ⇒ a steady-state Restart.
        let ns_resolver = switching_ns_resolver("local", "550e8400-e29b-41d4-a716-446655440000");
        let (messaging_tx, messaging_rx) = watch::channel(true);
        let (trigger_tx, trigger_rx) = mpsc::channel(8);
        let (ready_tx, ready_rx) = oneshot::channel();
        // The startup poll must NOT raise the restart signal in this scenario.
        let (restart_tx, restart_rx) = watch::channel(false);

        let task = tokio::spawn(manage_federation(
            applying_federator(),
            resolver,
            prober,
            messaging_rx,
            trigger_rx,
            ready_tx,
            Duration::from_secs(5),
            "local".to_string(),
            ns_resolver,
            restart_tx,
            None,
        ));

        tokio::time::timeout(Duration::from_secs(1), ready_rx)
            .await
            .expect("startup gate fires")
            .expect("gate sender not dropped");
        assert!(
            !*restart_rx.borrow(),
            "the startup poll saw an unchanged namespace ⇒ no startup restart"
        );

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
            FederationOutcome::Restart,
            "a namespace change must ack Restart, not Applied"
        );
        assert_eq!(
            probe_calls.load(Ordering::SeqCst),
            0,
            "a restart never probes the link"
        );

        drop(messaging_tx);
        task.abort();
    }

    /// The *startup* federation poll, on resolving a namespace that differs from
    /// the one this generation started under (e.g. logged in but the router cache
    /// was empty at build time so the startup namespace was `local`), must raise
    /// the in-process restart signal itself (there is no poke to ack) rather than
    /// run on un-federated under the wrong namespace. It also must not federate or
    /// probe on that drift (a restart is fail-closed).
    #[tokio::test]
    async fn startup_poll_requests_restart_on_namespace_drift() {
        let (resolver, _calls) = counting_resolver(upstream());
        let (prober, probe_calls) = counting_prober(Ok(()));
        // Every resolve returns an org id that differs from the `local` startup
        // namespace, so the very first (startup) poll detects the drift.
        let (ns_resolver, ns_calls) = counting_ns_resolver("550e8400-e29b-41d4-a716-446655440000");
        let (messaging_tx, messaging_rx) = watch::channel(true);
        let (_trigger_tx, trigger_rx) = mpsc::channel(8);
        let (ready_tx, ready_rx) = oneshot::channel();
        let (restart_tx, mut restart_rx) = watch::channel(false);

        let task = tokio::spawn(manage_federation(
            applying_federator(),
            resolver,
            prober,
            messaging_rx,
            trigger_rx,
            ready_tx,
            Duration::from_secs(5),
            "local".to_string(),
            ns_resolver,
            restart_tx,
            None,
        ));

        // Startup still unblocks `serve` (the gate fires) ...
        tokio::time::timeout(Duration::from_secs(1), ready_rx)
            .await
            .expect("startup gate fires even when a restart is requested")
            .expect("gate sender not dropped");
        // ... and then the startup poll raises the restart signal on its own.
        tokio::time::timeout(Duration::from_secs(1), restart_rx.changed())
            .await
            .expect("the startup poll raises the restart signal")
            .expect("restart sender not dropped");
        assert!(*restart_rx.borrow(), "the restart signal is set");
        assert_eq!(
            probe_calls.load(Ordering::SeqCst),
            0,
            "a startup restart never probes the link"
        );
        assert!(
            ns_calls.load(Ordering::SeqCst) >= 1,
            "the namespace was re-resolved to detect the drift"
        );

        drop(messaging_tx);
        task.abort();
    }
}
