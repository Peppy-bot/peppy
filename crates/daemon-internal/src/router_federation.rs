//! Federates the daemon's local zenohd router to the *platform router* (the
//! shared hub run by platform-backend) and keeps it federated for the daemon's
//! lifetime.
//!
//! The local router is always started *standalone* by the builder; resolving the
//! platform endpoint needs a backend round-trip. This task owns the whole
//! federation lifecycle:
//!
//! * **Initial federation (gates startup).** Once the router is up (it waits on
//!   `messaging_ready` so it cannot race [`MessagingRouter`](super::messaging_router)'s
//!   own `start_router`), the first poll resolves the upstream and federates the
//!   local router to it. This task signals a readiness gate to `serve` once that
//!   first poll completes, so `serve` only reports ready *after* federation is in
//!   place. Once the local router is ready, backend resolution is bounded by
//!   `connect_timeout` and router application by [`APPLY_TIMEOUT`], so a slow or
//!   unreachable backend cannot stall startup indefinitely (the daemon then
//!   proceeds standalone and keeps retrying in the background). At the same
//!   moments it fires the core node's
//!   *presence gate* (`presence_gate_tx`): the core node delays its boot-time
//!   presence check and declaration until the initial federation has settled,
//!   so the check sees the federated mesh (a same-name daemon reachable only
//!   through the platform router refuses boot) rather than the always-standalone
//!   just-started local router.
//! * **Immediate (re)federation on login/logout.** `peppy platform login`/`logout`
//!   poke the daemon over the control socket
//!   ([`FederationControl`](super::federation_control)); the poke is delivered
//!   here as a queued refederation request that runs a poll *now* (not on the
//!   next interval) and acks the resulting [`FederationOutcome`] so the CLI
//!   knows federation is in place before it returns. A login poke additionally
//!   *verifies* the federation link with a real TLS handshake
//!   ([`pmi::probe_tls_reachable`]) so a silent UnknownCA loop is reported as a
//!   [`LinkState::Error`] rather than a false success. The loop also publishes
//!   its cached [`FederationStatus`] to a watch channel; the control socket
//!   answers status queries straight from that watch, so they never queue
//!   behind an in-flight apply.
//! * **Liveness (backend-driven).** There is no client-side keepalive poll. Once
//!   federated, the local router holds its link to the platform router open on
//!   its own (`reconnect: true`); the backend actively probes this daemon's
//!   `/health` service over the federated link. The config pull on startup/login
//!   tells the backend this daemon's `core_node` name so it knows which
//!   `/health` service to probe.
//! * **Registration cadence.** Every config pull's POST carries this daemon's
//!   core-node name, upserting it into the backend's per-principal core-node
//!   registry. The POST fires on cache-stale pulls and on login/logout pokes
//!   (login clears the router cache, so every login re-registers), never on a
//!   timer. The backend's `last_seen_at` for a core node therefore means "last
//!   federation config pull", not liveness.
//! * **Live (re)federation.** When the resolved upstream changes (the user logs
//!   in, logs out, or the endpoint moves) the local router's zenohd config is
//!   re-rendered and the router restarted, so the change takes effect without a
//!   full daemon restart.

use crate::control::{FederationStatus, LinkState, PlatformLink};
use crate::error::{Error, Result};
use crate::platform_locator::platform_connect_locator;
use crate::serve::{ServeAsyncCommand, ServeAsyncHandle};
use config::namespace::Namespace;
use daemon_config::consts::PeppyDirs;
use daemon_config::peppy_config::ParsedEndpointBuf;
use pmi::{Messenger, MessengerBackend, RouterLinks};
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
/// `APPLY_ACK_SLACK`, which is sized to cover apply plus probe. An unreachable /
/// firewalled router fails the probe within this bound and surfaces promptly as
/// a [`LinkState::Error`] rather than as a daemon-side ack timeout.
pub(crate) const PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// Post-resolve budget for rewriting the managed router and waiting for zenohd
/// to accept connections again. Kept separate from the backend resolve timeout.
pub(crate) const APPLY_TIMEOUT: Duration = Duration::from_secs(4);

/// Failed resolves or rewrites are retried without turning the federation loop
/// back into a keepalive poll. Once desired state applies, the timer is idle and
/// zenohd owns link reconnection.
const RETRY_DELAY: Duration = Duration::from_secs(5);

/// At most one refederation waits behind the poll currently in progress; a
/// second concurrent poke is rejected as busy by the control handler. Status
/// queries never enter this queue: they are answered from the status watch.
const REFEDERATE_QUEUE_CAPACITY: usize = 1;

/// The platform federation target: the durable endpoint stays separate from the
/// rendered locator (which carries the mTLS material as endpoint fragments), so
/// status never keys off fragment text.
#[derive(Debug, Clone)]
struct DesiredBackend {
    endpoint: ParsedEndpointBuf,
    locator: String,
    tls: pmi::TlsConfig,
}

/// What one poll's resolve produced: the desired platform upstream (`None` is
/// logged out / standalone) plus the namespace the same credentials resolve
/// to, compared against the generation's startup namespace to detect a
/// namespace change.
struct Resolved {
    upstream: Option<DesiredBackend>,
    namespace: Namespace,
}

/// Resolves the desired platform upstream and namespace from the credentials.
type Resolver = Arc<dyn Fn() -> std::result::Result<Resolved, String> + Send + Sync>;

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

async fn probe_with_bound(
    prober: Prober,
    host: String,
    port: u16,
    tls: pmi::TlsConfig,
    timeout: Duration,
) -> std::result::Result<(), String> {
    let address = format!("{host}:{port}");
    match tokio::time::timeout(
        timeout,
        async move { prober(host, port, tls, timeout).await },
    )
    .await
    {
        Ok(result) => result,
        Err(_) => Err(format!(
            "federation probe to {address} timed out after {timeout:?}"
        )),
    }
}

/// The future a [`Federator`] returns: `Ok(true)` means the local router's config
/// was (re)rendered and zenohd bounced, `Ok(false)` means nothing was applied
/// because the managed router uses a pinned `ZENOH_CONFIG`, `Err` means the apply
/// failed.
type FederateFuture = Pin<Box<dyn Future<Output = Result<bool>> + Send>>;

/// Applies a desired upstream to the local router (re-render + bounce). A boxed
/// async closure so tests can inject a deterministic federation result:
/// `Ok(true)` (a real rewrite) or `Ok(false)` (operator-pinned), in place of the
/// real [`refederate_and_restart`], whose mock backend can only ever report
/// `Ok(false)` and so cannot exercise the applied/verify path.
type Federator = Arc<dyn Fn(Option<String>) -> FederateFuture + Send + Sync>;

/// The real federator: re-render the owned router's config with the upstream and,
/// if it changed, bounce zenohd (see [`refederate_and_restart`]).
fn real_federator(messenger: Arc<Mutex<Messenger>>) -> Federator {
    Arc::new(move |upstream| -> FederateFuture {
        let messenger = messenger.clone();
        Box::pin(async move { refederate_and_restart(&messenger, upstream).await })
    })
}

/// Outcome of one federation poll, reported back to a control-socket poke so the
/// CLI can tell the user the post-apply state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum FederationOutcome {
    /// The poll settled: the platform link now applied (or cleared) and its
    /// verification state. Covers both "just applied" and "already in place" (a
    /// no-op poll where the upstream was unchanged). On a verifying poke a
    /// [`LinkState::Error`] means the config was applied but the TLS link to the
    /// platform router does not actually validate.
    Applied(PlatformLink),
    /// The managed router uses a pinned `ZENOH_CONFIG`, so nothing changed.
    Pinned,
    /// The resolve or apply failed; the loop keeps retrying.
    Failed(String),
    /// The credentials changed the daemon's *namespace*. A session's namespace
    /// is immutable after open and the core node holds long-lived declarations,
    /// so the change cannot be applied to the live session by a zenohd bounce;
    /// it needs a full daemon-generation restart into `target_namespace`. This
    /// poll does NOT (de)federate (federating under a namespace that differs
    /// from the live session's would leak across tenants); it just signals the
    /// restart. The control handler owns triggering it (after flushing the
    /// ack); the federation loop only reports it.
    Restart { target_namespace: Namespace },
}

/// Sends refederation pokes (each carrying its ack) to the federation loop
/// (held by [`FederationControl`](super::federation_control)).
pub(crate) type TriggerSender = mpsc::Sender<oneshot::Sender<FederationOutcome>>;
/// Receives refederation pokes in the federation loop.
pub(crate) type TriggerReceiver = mpsc::Receiver<oneshot::Sender<FederationOutcome>>;

/// The poke channel between the control socket and the federation loop, sized
/// so at most one poke queues behind an in-progress poll (see
/// [`REFEDERATE_QUEUE_CAPACITY`]).
pub(crate) fn trigger_channel() -> (TriggerSender, TriggerReceiver) {
    mpsc::channel(REFEDERATE_QUEUE_CAPACITY)
}

/// The inputs one federation poll needs: the injected effect seams plus the
/// bounds and namespace context shared by every poll. Split from
/// [`RouterFederation`] so the poll engine carries no channel plumbing.
struct FederationPoller {
    federator: Federator,
    resolver: Resolver,
    prober: Prober,
    /// Bound on backend resolution after the local router is ready.
    connect_timeout: Duration,
    /// Bound on the router apply (config re-render + zenohd bounce),
    /// [`APPLY_TIMEOUT`] outside tests.
    apply_timeout: Duration,
    /// This generation's namespace, resolved once at startup. A poll whose
    /// resolve carries a *different* namespace requests a restart instead of a
    /// live re-federation.
    startup_namespace: Namespace,
}

/// Background task (a [`ServeAsyncCommand`]) that federates the local router to
/// the platform router and keeps it federated. See the module docs.
pub(crate) struct RouterFederation {
    poller: FederationPoller,
    /// Goes `true` once the router process is up (MessagingRouter ran
    /// `start_router` + `start_session`). The task waits on this before touching
    /// the router so its initial federation cannot race the router's own startup.
    messaging_ready: watch::Receiver<bool>,
    /// Immediate-refederation pokes from the control socket.
    trigger_rx: TriggerReceiver,
    /// Publishes the cached federation status after every poll; the control
    /// socket answers status queries from the receiving half.
    status_tx: watch::Sender<FederationStatus>,
    /// In-process restart signal (the serve coordinator's). The *startup*
    /// federation poll raises it when it discovers the credentials already resolve
    /// to a different namespace than this generation started under, so the live
    /// session (which can't be re-namespaced) is rebuilt rather than left running
    /// un-federated. The steady-state poke path instead acks `Restart` and the
    /// control handler raises this same signal after flushing the ack.
    restart_tx: watch::Sender<bool>,
    /// Fired (`true`) at the same moments as the startup readiness gate: once
    /// the *initial* federation poll has settled (or the bound elapsed / the
    /// router never came up). The core node waits on it before checking and
    /// declaring its presence, so the check sees the federated mesh instead of
    /// the always-standalone just-started router. `None` when no core node was
    /// built.
    presence_gate_tx: Option<watch::Sender<bool>>,
    /// Shared coordinator token: the task tears down when it is cancelled (an
    /// in-process restart) or on a real OS shutdown signal.
    teardown_token: CancellationToken,
    /// Whether the router was operator-pinned before the federation task began.
    initial_pinned: bool,
}

impl RouterFederation {
    /// Builds the federation task and hands back the receiving half of its
    /// status watch, seeded with the pre-poll state (standalone, with the
    /// operator-pinned bit already correct), for the control socket to answer
    /// status queries from.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        messenger: Arc<Mutex<Messenger>>,
        api_url: String,
        core_node_name: String,
        peppy_dirs: PeppyDirs,
        messaging_ready: watch::Receiver<bool>,
        trigger_rx: TriggerReceiver,
        connect_timeout: Duration,
        startup_namespace: Namespace,
        restart_tx: watch::Sender<bool>,
        presence_gate_tx: Option<watch::Sender<bool>>,
        initial_pinned: bool,
        teardown_token: CancellationToken,
    ) -> (Self, watch::Receiver<FederationStatus>) {
        // The loop's one ambient input, re-read on every poll, derives from the
        // generation's data root: the federation resolve reads the credentials
        // file and the materialized dev TLS under it, and carries the namespace
        // out of the same read.
        let resolver_dirs = peppy_dirs.clone();
        let resolver: Resolver = Arc::new(move || {
            let resolved = auth::router::resolve_federation_target(
                &resolver_dirs,
                &api_url,
                connect_timeout,
                &core_node_name,
            );
            let upstream = resolved
                .upstream
                .map(|(endpoint, tls)| {
                    let locator = platform_connect_locator(&endpoint, &tls)?;
                    Ok::<_, String>(DesiredBackend {
                        endpoint,
                        locator,
                        tls,
                    })
                })
                .transpose()?;
            Ok(Resolved {
                upstream,
                namespace: resolved.namespace,
            })
        });
        let (status_tx, status_rx) = watch::channel(FederationStatus {
            link: PlatformLink::default(),
            pinned: initial_pinned,
        });
        let federation = Self {
            poller: FederationPoller {
                federator: real_federator(messenger),
                resolver,
                prober: real_prober(),
                connect_timeout,
                apply_timeout: APPLY_TIMEOUT,
                startup_namespace,
            },
            messaging_ready,
            trigger_rx,
            status_tx,
            restart_tx,
            presence_gate_tx,
            teardown_token,
            initial_pinned,
        };
        (federation, status_rx)
    }
}

impl ServeAsyncCommand for RouterFederation {
    fn run(self: Box<Self>) -> ServeAsyncHandle {
        let this = *self;
        let teardown_token = this.teardown_token.clone();
        // Readiness gate: fired by `manage` once the first federation poll
        // completes (or one of its bounds elapses), so after the local router
        // is ready `serve` blocks at most `connect_timeout` for resolution
        // plus `APPLY_TIMEOUT` for router application. `serve` treats this gate
        // as non-fatal, so a drop here (e.g. shutdown wins the race below before
        // it fires) degrades to "proceed standalone", never a crash.
        let (ready_tx, ready_rx) = oneshot::channel();
        let future = Box::pin(async move {
            // Race the maintenance loop against shutdown (a real signal or an
            // in-process restart via the shared token) so the daemon can exit
            // promptly (the loop is otherwise infinite).
            tokio::select! {
                _ = this.manage(ready_tx) => {}
                _ = crate::shutdown_signal::shutdown_or_token(&teardown_token) => {}
            }
            Ok(())
        });
        ServeAsyncHandle::new_optional_ready(future, ready_rx)
    }
}

/// Fires the startup readiness gate exactly once (idempotent: the `Option` is
/// `take`n, so later calls or a drop are no-ops) and, in lockstep, the core
/// node's presence gate (a `watch`, so re-sends are harmless). The two fire at
/// the same moments so the core node's boot presence check is delayed exactly
/// as long as `serve`'s own readiness: until the initial federation settled,
/// bounded after local-router readiness by `connect_timeout` plus
/// [`APPLY_TIMEOUT`], and fail-open (a dropped sender lets the waiter proceed).
fn fire_gate(gate: &mut Option<oneshot::Sender<()>>, presence_gate: &Option<watch::Sender<bool>>) {
    if let Some(tx) = gate.take() {
        let _ = tx.send(());
    }
    if let Some(tx) = presence_gate {
        let _ = tx.send(true);
    }
}

/// The desired state whose last apply attempt completed (with `Ok(true)` or
/// `Ok(false)`), so an unchanged desired state replays the cached outcome
/// instead of bouncing the router again. Keyed on the rendered locator
/// (endpoint + TLS fragments), so a changed certificate path re-applies even
/// when the endpoint is unchanged.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
enum SettledDesired {
    /// No apply attempt has settled yet (a pinned start), so the first poll
    /// always attempts an apply.
    Unsettled,
    /// Settled with no upstream: the router is standalone by decision.
    #[default]
    Standalone,
    /// Settled with this upstream locator.
    Upstream(String),
}

impl SettledDesired {
    /// The settled form of a completed apply attempt for `desired`.
    fn from_completed(desired: Option<String>) -> Self {
        match desired {
            None => Self::Standalone,
            Some(locator) => Self::Upstream(locator),
        }
    }

    /// Whether `desired` matches what last settled. An unsettled state matches
    /// nothing, so the first poll after a pinned start always applies.
    fn matches(&self, desired: &Option<String>) -> bool {
        match (self, desired) {
            (Self::Standalone, None) => true,
            (Self::Upstream(settled), Some(desired)) => settled == desired,
            _ => false,
        }
    }
}

/// What the last completed poll actually left in effect: the platform link
/// (endpoint + verification state) plus the caches that keep repeat polls
/// cheap and pinned routers honest.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct AppliedState {
    /// The platform endpoint now in effect (`None` is standalone).
    endpoint: Option<String>,
    /// The link's verification state as of the last poll that touched it.
    link_state: LinkState,
    /// Last desired state whose apply attempt completed. Failures leave this
    /// unchanged so the desired state retries.
    last_settled_desired: SettledDesired,
    /// Whether the managed router uses a pinned `ZENOH_CONFIG`, even though its
    /// desired upstream may differ from what is actually in effect.
    pinned: bool,
    /// The last verifying poke found a TLS failure. A later verifying poke must
    /// bounce the router even when the upstream is unchanged, because the user
    /// may have replaced local certificate files before re-running the command.
    needs_reapply: bool,
}

impl AppliedState {
    fn platform_link(&self) -> PlatformLink {
        PlatformLink {
            endpoint: self.endpoint.clone(),
            link_state: self.link_state.clone(),
        }
    }
}

impl RouterFederation {
    /// Waits for the router to come up, runs the initial federation (firing the
    /// startup gate when it completes or the timeout elapses), then services
    /// immediate login/logout pokes for the daemon's lifetime (the caller races
    /// it against the shutdown signal). There is no periodic keepalive: once
    /// federated, the local router holds its upstream link open on its own and
    /// the backend actively health-checks this daemon.
    async fn manage(self, ready_tx: oneshot::Sender<()>) {
        let RouterFederation {
            poller,
            messaging_ready,
            trigger_rx,
            status_tx,
            restart_tx,
            presence_gate_tx,
            teardown_token: _,
            initial_pinned,
        } = self;
        let lifecycle = FederationLoop {
            poller,
            ready_tx: Some(ready_tx),
            restart_tx,
            presence_gate_tx,
            status_tx,
        };
        // The router spawns standalone. For a non-pinned router that standalone
        // state IS the settled desired state (no upstream), so a logged-out
        // first poll never bounces it; a pinned router settled nothing (the
        // rendered config was not consumed), so the first poll always attempts
        // an apply and caches the Pinned rejection.
        let initial = AppliedState {
            last_settled_desired: if initial_pinned {
                SettledDesired::Unsettled
            } else {
                SettledDesired::Standalone
            },
            pinned: initial_pinned,
            ..AppliedState::default()
        };
        lifecycle.run(messaging_ready, trigger_rx, initial).await;
    }
}

/// The federation lifecycle after setup: the poll engine plus the channels its
/// phases share. Split from [`RouterFederation::manage`] so the phases can
/// early-return while the dispatcher abort stays with the caller.
struct FederationLoop {
    poller: FederationPoller,
    /// Startup readiness gate, `take`n by [`fire_gate`] on first fire.
    ready_tx: Option<oneshot::Sender<()>>,
    restart_tx: watch::Sender<bool>,
    presence_gate_tx: Option<watch::Sender<bool>>,
    status_tx: watch::Sender<FederationStatus>,
}

impl FederationLoop {
    async fn run(
        mut self,
        mut messaging_ready: watch::Receiver<bool>,
        mut trigger_rx: TriggerReceiver,
        mut applied: AppliedState,
    ) {
        // Phase 1: wait for the router, bounded by `connect_timeout`. Don't touch the
        // router until it is up, or the initial federation could race MessagingRouter's
        // `start_router`/`start_session`. `wait_for` checks the current value first,
        // then awaits changes. Drop the borrowed `Ref` it returns immediately (map to
        // `()`) so `messaging_ready` is free for the timeout-elapsed arm to reuse.
        let armed = tokio::time::timeout(
            self.poller.connect_timeout,
            messaging_ready.wait_for(|r| *r),
        )
        .await
        .map(|res| res.map(|_ready| ()));
        match armed {
            Ok(Ok(())) => {}
            Ok(Err(_)) => {
                // `messaging_ready` closed before going true: the router task never
                // started or already exited, so there is nothing to federate. Unblock
                // startup and stop.
                fire_gate(&mut self.ready_tx, &self.presence_gate_tx);
                return;
            }
            Err(_elapsed) => {
                // The router isn't up within the bound: unblock startup now (the
                // daemon proceeds standalone), then keep waiting (unbounded) so the
                // local router still federates once it does come up.
                fire_gate(&mut self.ready_tx, &self.presence_gate_tx);
                if messaging_ready.wait_for(|r| *r).await.is_err() {
                    return;
                }
            }
        }

        // Phase 2: initial federation. The router was started standalone, so nothing
        // is federated yet. Resolution is bounded by `connect_timeout` and the local
        // router apply by `APPLY_TIMEOUT`, so this completes within their combined
        // bound even if the user is logged in but the backend is unreachable. The
        // initial poll does not verify (`verify = false`): startup must not block on a
        // TLS handshake, and the verifying check belongs to the login poke.
        let initial_outcome = self.poller.poll_and_apply(&mut applied, false).await;
        self.publish_status(&applied);
        fire_gate(&mut self.ready_tx, &self.presence_gate_tx);

        // The initial poll re-pulled the federation config, so the credentials now
        // reflect the current workspace. If that resolves to a *different* namespace
        // than this generation started under (e.g. the daemon started logged-in but
        // with a cleared/stale router cache, so `startup_namespace` was `local` before
        // the pull discovered the real workspace), the live session can't be
        // re-namespaced. Request a generation restart now; otherwise the daemon would
        // run un-federated under the wrong namespace until the next login/logout poke.
        // The steady-state poke path leaves the actual restart to the control handler
        // (which flushes its ack first); the startup poll has no ack to flush, so it
        // raises the signal directly. The rebuilt generation resolves the namespace
        // afresh and federates normally.
        if matches!(&initial_outcome, FederationOutcome::Restart { .. }) {
            info!(
                "router federation: startup resolved a namespace that differs from this generation's; \
                 requesting a daemon restart instead of federating under the wrong namespace"
            );
            let _ = self.restart_tx.send(true);
            return;
        }

        // Phase 3, steady state: react to immediate CLI pokes. A failed resolve or
        // rewrite is retried on a short timer until desired state applies. This is
        // not a keepalive poll: after success the timer goes idle and zenohd owns
        // link reconnection.
        enum Work {
            Poke(oneshot::Sender<FederationOutcome>),
            Retry,
        }
        let mut retry_pending = matches!(&initial_outcome, FederationOutcome::Failed(_));
        loop {
            let work = if retry_pending {
                tokio::select! {
                    request = trigger_rx.recv() => match request {
                        Some(ack) => Work::Poke(ack),
                        None => break,
                    },
                    _ = tokio::time::sleep(RETRY_DELAY) => Work::Retry,
                }
            } else {
                let Some(ack) = trigger_rx.recv().await else {
                    break;
                };
                Work::Poke(ack)
            };
            let verify = matches!(&work, Work::Poke(_));
            let outcome = self.poller.poll_and_apply(&mut applied, verify).await;
            self.publish_status(&applied);
            retry_pending = matches!(&outcome, FederationOutcome::Failed(_));
            match work {
                Work::Poke(ack) => {
                    // The CLI may have already given up (read timeout); ignore. On
                    // a namespace change the control handler raises the restart
                    // after attempting to flush `Restarting`.
                    let _ = ack.send(outcome);
                }
                Work::Retry if matches!(&outcome, FederationOutcome::Restart { .. }) => {
                    let _ = self.restart_tx.send(true);
                    return;
                }
                Work::Retry => {}
            }
        }
    }

    /// Publishes the cached status the control socket answers status queries
    /// from: the platform link now in effect plus router-config ownership.
    fn publish_status(&self, applied: &AppliedState) {
        self.status_tx.send_replace(FederationStatus {
            link: applied.platform_link(),
            pinned: applied.pinned,
        });
    }
}

impl FederationPoller {
    /// One poll: resolve the desired upstream and, if it changed, (re)federate the
    /// local router. Updates `*applied` to the upstream now in effect and returns the
    /// [`FederationOutcome`] (so a poke can ack the post-apply state).
    ///
    /// When `verify` is set (login/logout pokes only, not the initial startup
    /// federation), and an upstream is desired, a real TLS handshake confirms the
    /// link actually validates; a failed handshake is reported (and logged loudly)
    /// as a [`LinkState::Error`] instead of a false verified success.
    async fn poll_and_apply(&self, applied: &mut AppliedState, verify: bool) -> FederationOutcome {
        // Backend resolution receives the configured resolve deadline. Router
        // process work receives its own bounded budget.
        let resolve_deadline = tokio::time::Instant::now() + self.connect_timeout;

        // The resolver is blocking (HTTP + file I/O); keep it off the async worker. It
        // also re-pulls the platform router's config when the cached copy has gone
        // stale (cache freshness only, not a keepalive). Bound the whole resolve by
        // `connect_timeout` so a hung pull can't stall a poll (or the startup gate)
        // past it; the timed-out blocking thread is harmless (its own HTTP timeout
        // ends it) and its result is simply discarded.
        let resolver = self.resolver.clone();
        let resolved = match tokio::time::timeout_at(
            resolve_deadline,
            tokio::task::spawn_blocking(move || resolver()),
        )
        .await
        {
            Ok(Ok(Ok(t))) => t,
            Ok(Ok(Err(message))) => {
                warn!(error = %message, "router federation: desired-state resolve failed; will retry");
                return FederationOutcome::Failed(message);
            }
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
        // federation config and carried out the namespace those credentials now
        // resolve to. A session's namespace is immutable after open, so if it
        // differs from this generation's startup namespace the change cannot be
        // applied by a live zenohd bounce: request a full restart instead, WITHOUT
        // federating (federating under a namespace that differs from the live
        // session's would leak across tenants). The control handler flushes the ack
        // before triggering the restart; the initial (non-poke) poll discards this
        // outcome but, crucially, also does not federate, so it stays fail-closed
        // until the next generation.
        if resolved.namespace != self.startup_namespace {
            info!(
                from = %self.startup_namespace,
                to = %resolved.namespace,
                "router federation: namespace changed; requesting a daemon restart \
                 (a namespace change cannot be applied to a live session)"
            );
            return FederationOutcome::Restart {
                target_namespace: resolved.namespace,
            };
        }

        let desired_endpoint = resolved
            .upstream
            .as_ref()
            .map(|backend| backend.endpoint.as_str().to_string());
        let desired_locator = resolved
            .upstream
            .as_ref()
            .map(|backend| backend.locator.clone());
        let unchanged = applied.last_settled_desired.matches(&desired_locator);

        // Apply the desired upstream, or replay the cached outcome when the
        // rendered locator (endpoint + TLS material) is unchanged.
        let should_apply = !unchanged || (verify && applied.needs_reapply);
        if !should_apply {
            if applied.pinned {
                return FederationOutcome::Pinned;
            }
        } else {
            // Give the apply (config re-render + zenohd bounce) its own bound after
            // resolution: it awaits the messenger lock and stops/starts the router, so a
            // wedged holder (e.g. a stuck watchdog restart) would otherwise keep the
            // startup readiness gate closed and the poke loop stuck indefinitely.
            // On timeout `applied` is left unchanged so the next poll retries. If
            // the timeout lands between the router stop and start, the watchdog
            // notices the dead router and respawns it with the already-rewritten
            // config, so the router cannot stay down.
            let apply_deadline = tokio::time::Instant::now() + self.apply_timeout;
            match tokio::time::timeout_at(apply_deadline, (self.federator)(desired_locator.clone()))
                .await
            {
                Err(_elapsed) => {
                    warn!(
                        "router federation: applying the upstream change timed out, so federation \
                         with the platform router is NOT in effect; will retry"
                    );
                    return FederationOutcome::Failed("apply timed out".to_string());
                }
                Ok(Ok(true)) => {
                    info!(
                        endpoint = ?desired_endpoint,
                        "router federation: applied the desired platform upstream"
                    );
                    *applied = AppliedState {
                        endpoint: desired_endpoint.clone(),
                        link_state: if desired_endpoint.is_some() {
                            LinkState::Unverified
                        } else {
                            LinkState::NotConfigured
                        },
                        last_settled_desired: SettledDesired::from_completed(
                            desired_locator.clone(),
                        ),
                        pinned: false,
                        needs_reapply: false,
                    };
                }
                Ok(Ok(false)) => {
                    // A managed router with a pinned `ZENOH_CONFIG` cannot be changed
                    // here. Preserve the endpoint known to be in effect: the desired
                    // upstream was not applied and must not leak into status as if
                    // it were. Remember the pinned bit so an unchanged desired target
                    // is still reported as pinned; warn so the operator knows
                    // federation is not being auto-managed.
                    warn!(
                        "router federation: the managed router uses an operator-pinned \
                         ZENOH_CONFIG; the desired federation change was not applied"
                    );
                    applied.last_settled_desired =
                        SettledDesired::from_completed(desired_locator.clone());
                    applied.pinned = true;
                    applied.needs_reapply = false;
                    return FederationOutcome::Pinned;
                }
                Ok(Err(e)) => {
                    // Leave `applied` unchanged so the next poll retries the apply.
                    warn!(
                        error = %e,
                        "router federation: failed to apply the upstream change, so federation \
                         with the platform router is NOT in effect; will retry"
                    );
                    return FederationOutcome::Failed(e.to_string());
                }
            }
        }

        // Verify the link with a real, bounded TLS handshake (login/logout pokes
        // only). A failed handshake marks the link errored and requests a bounce on
        // the next verifying poke (the user may replace certificate files between
        // attempts).
        if verify && let Some(backend) = &resolved.upstream {
            let result = probe_with_bound(
                self.prober.clone(),
                backend.endpoint.host().to_string(),
                backend.endpoint.port(),
                backend.tls.clone(),
                PROBE_TIMEOUT,
            )
            .await;
            match result {
                Ok(()) => {
                    applied.link_state = LinkState::Verified;
                    applied.needs_reapply = false;
                }
                Err(reason) => {
                    warn!(
                        reason = %reason,
                        "router federation: the platform link did not validate"
                    );
                    applied.link_state = LinkState::Error(reason);
                    applied.needs_reapply = true;
                }
            }
        }

        FederationOutcome::Applied(applied.platform_link())
    }
}

/// Re-renders the local router's config with the (possibly absent) upstream and,
/// if the config actually changed, restarts zenohd so it takes effect. Holds the
/// messenger lock across the whole stop/start so it cannot interleave with the
/// watchdog's own restart.
///
/// Returns whether zenohd was restarted: `false` when [`Messenger::refederate`]
/// was a no-op because the managed router uses a pinned `ZENOH_CONFIG`, so a
/// pointless bounce is skipped; `true` when the config was rewritten and the
/// router bounced.
async fn refederate_and_restart(
    messenger: &Arc<Mutex<Messenger>>,
    upstream: Option<String>,
) -> Result<bool> {
    let mut messenger = messenger.lock().await;
    let rewrote = messenger
        .refederate(RouterLinks {
            upstream,
            tls: None,
        })
        .map_err(Error::PeppyMessagingInterface)?;
    if !rewrote {
        // The managed router uses a pinned `ZENOH_CONFIG`, so bouncing zenohd
        // would not apply the requested change. Skip the restart and report it.
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
    use crate::test_util::dial;
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

    const ENDPOINT: &str = "tls/cap.zenoh.localhost:7443";
    const WORKSPACE: &str = "550e8400-e29b-41d4-a716-446655440000";

    fn workspace_namespace() -> Namespace {
        Namespace::parse(WORKSPACE).expect("valid test namespace")
    }

    /// Wraps an upstream into the resolver's return under the `local`
    /// namespace (matching the default startup namespace, so no namespace
    /// change is detected).
    fn resolved(upstream: Option<DesiredBackend>) -> Resolved {
        Resolved {
            upstream,
            namespace: Namespace::local(),
        }
    }

    /// A poll engine with injected seams and test defaults. Tests override
    /// fields (timeouts, startup namespace) before calling `poll_and_apply`.
    fn poller_under_test(
        federator: Federator,
        resolver: Resolver,
        prober: Prober,
    ) -> FederationPoller {
        FederationPoller {
            federator,
            resolver,
            prober,
            connect_timeout: Duration::from_secs(1),
            apply_timeout: APPLY_TIMEOUT,
            startup_namespace: Namespace::local(),
        }
    }

    /// A `RouterFederation` with injected seams and test defaults, plus the
    /// receiving half of its status watch. Tests override fields (gates,
    /// restart signal, poller bounds) before calling `manage`.
    fn federation_under_test(
        federator: Federator,
        resolver: Resolver,
        prober: Prober,
        messaging_ready: watch::Receiver<bool>,
        trigger_rx: TriggerReceiver,
    ) -> (RouterFederation, watch::Receiver<FederationStatus>) {
        let (status_tx, status_rx) = watch::channel(FederationStatus::default());
        let federation = RouterFederation {
            poller: poller_under_test(federator, resolver, prober),
            messaging_ready,
            trigger_rx,
            status_tx,
            restart_tx: watch::channel(false).0,
            presence_gate_tx: None,
            teardown_token: CancellationToken::new(),
            initial_pinned: false,
        };
        (federation, status_rx)
    }

    /// A federator simulating a real (non-pinned) rewrite: it reports the config
    /// was rewritten (`Ok(true)`), so the poll treats the upstream as actually
    /// applied, the path the verify/probe logic exercises. (The mock messenger's
    /// real `refederate` can only ever report `Ok(false)`, i.e. pinned, so the
    /// applied path is reachable in tests only via an injected federator.)
    fn applying_federator() -> Federator {
        Arc::new(|_upstream| -> FederateFuture { Box::pin(async { Ok(true) }) })
    }

    /// A federator simulating an operator-pinned config and recording attempts:
    /// `refederate` reports no rewrite (`Ok(false)`), so the poll is `Pinned`.
    fn counting_pinned_federator() -> (Federator, Arc<AtomicUsize>) {
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = calls.clone();
        let federator: Federator = Arc::new(move |_upstream| {
            counter.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { Ok(false) })
        });
        (federator, calls)
    }

    /// A federator that never completes, simulating a wedged apply (e.g. the
    /// messenger lock held forever by a stuck watchdog restart).
    fn wedged_federator() -> Federator {
        Arc::new(|_upstream| -> FederateFuture { Box::pin(std::future::pending()) })
    }

    /// A resolver returning a fixed upstream (under the `local` namespace)
    /// and counting its calls.
    fn counting_resolver(upstream: Option<DesiredBackend>) -> (Resolver, Arc<AtomicUsize>) {
        counting_resolver_with_namespace(upstream, Namespace::local())
    }

    /// A resolver returning a fixed upstream and namespace, counting its
    /// calls, for tests that exercise the namespace-change restart path.
    fn counting_resolver_with_namespace(
        upstream: Option<DesiredBackend>,
        namespace: Namespace,
    ) -> (Resolver, Arc<AtomicUsize>) {
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = calls.clone();
        let resolver: Resolver = Arc::new(move || {
            counter.fetch_add(1, Ordering::SeqCst);
            Ok(Resolved {
                upstream: upstream.clone(),
                namespace: namespace.clone(),
            })
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

    fn upstream() -> Option<DesiredBackend> {
        Some(DesiredBackend {
            endpoint: dial(ENDPOINT),
            locator: ENDPOINT.to_string(),
            tls: pmi::TlsConfig::default(),
        })
    }

    fn recording_federator() -> (Federator, Arc<std::sync::Mutex<Vec<Option<String>>>>) {
        let calls = Arc::new(std::sync::Mutex::new(Vec::new()));
        let recorded = calls.clone();
        let federator: Federator = Arc::new(move |upstream| {
            recorded.lock().unwrap().push(upstream);
            Box::pin(async { Ok(true) })
        });
        (federator, calls)
    }

    /// A resolver whose namespace is `first` on the first call and `rest`
    /// after, so the *startup* poll sees the unchanged namespace (no startup
    /// restart) and a later *poke* sees the change, exercising the steady-state
    /// `Restart` ack distinctly from the startup restart path.
    fn namespace_switching_resolver(
        upstream: Option<DesiredBackend>,
        first: Namespace,
        rest: Namespace,
    ) -> Resolver {
        let calls = AtomicUsize::new(0);
        Arc::new(move || {
            let namespace = if calls.fetch_add(1, Ordering::SeqCst) == 0 {
                first.clone()
            } else {
                rest.clone()
            };
            Ok(Resolved {
                upstream: upstream.clone(),
                namespace,
            })
        })
    }

    #[tokio::test]
    async fn daemon_enforces_the_probe_bound_for_a_noncompliant_prober() {
        let prober: Prober =
            Arc::new(|_host, _port, _tls, _timeout| Box::pin(std::future::pending()));
        let started = tokio::time::Instant::now();
        let error = probe_with_bound(
            prober,
            "hub.example".to_string(),
            7447,
            pmi::TlsConfig::default(),
            Duration::from_millis(50),
        )
        .await
        .expect_err("the daemon-side deadline must end a stuck probe");

        assert!(error.contains("timed out"));
        assert!(started.elapsed() < Duration::from_millis(500));
    }

    #[tokio::test]
    async fn an_upstream_apply_renders_the_single_platform_locator() {
        let (resolver, _) = counting_resolver(upstream());
        let (federator, calls) = recording_federator();
        let (prober, probe_calls) = counting_prober(Ok(()));
        let mut applied = AppliedState::default();

        let outcome = poller_under_test(federator, resolver, prober)
            .poll_and_apply(&mut applied, false)
            .await;

        assert_eq!(
            outcome,
            FederationOutcome::Applied(PlatformLink {
                endpoint: Some(ENDPOINT.to_string()),
                link_state: LinkState::Unverified,
            })
        );
        assert_eq!(*calls.lock().unwrap(), vec![Some(ENDPOINT.to_string())]);
        assert_eq!(
            probe_calls.load(Ordering::SeqCst),
            0,
            "a non-verifying poll never probes"
        );
        assert_eq!(applied.endpoint.as_deref(), Some(ENDPOINT));
    }

    #[tokio::test]
    async fn a_standalone_start_with_no_upstream_never_bounces_the_router() {
        // Logged out: the router spawned standalone, and the resolve agrees
        // (no upstream), so the first poll must not bounce zenohd.
        let (resolver, _) = counting_resolver(None);
        let (federator, calls) = recording_federator();
        let (prober, probe_calls) = counting_prober(Ok(()));
        let mut applied = AppliedState::default();

        let outcome = poller_under_test(federator, resolver, prober)
            .poll_and_apply(&mut applied, false)
            .await;

        assert_eq!(
            outcome,
            FederationOutcome::Applied(PlatformLink {
                endpoint: None,
                link_state: LinkState::NotConfigured,
            })
        );
        assert!(
            calls.lock().unwrap().is_empty(),
            "an unchanged standalone state must not bounce the router"
        );
        assert_eq!(probe_calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn a_changed_locator_reapplies_the_unchanged_endpoint() {
        // Same endpoint, refreshed TLS material (a re-issued certificate changes
        // the fragment paths): the rendered locator differs, so the poll must
        // re-apply even though the endpoint string is identical.
        let refreshed = Some(DesiredBackend {
            endpoint: dial(ENDPOINT),
            locator: format!("{ENDPOINT}#connect_certificate_file=/certs/generation-2/cert.pem"),
            tls: pmi::TlsConfig::default(),
        });
        let (resolver, _) = counting_resolver(refreshed.clone());
        let (federator, calls) = recording_federator();
        let (prober, _) = counting_prober(Ok(()));
        let mut applied = AppliedState {
            endpoint: Some(ENDPOINT.to_string()),
            link_state: LinkState::Unverified,
            last_settled_desired: SettledDesired::Upstream(format!(
                "{ENDPOINT}#connect_certificate_file=/certs/generation-1/cert.pem"
            )),
            pinned: false,
            needs_reapply: false,
        };

        let outcome = poller_under_test(federator, resolver, prober)
            .poll_and_apply(&mut applied, false)
            .await;

        assert!(matches!(outcome, FederationOutcome::Applied(_)));
        assert_eq!(
            *calls.lock().unwrap(),
            vec![refreshed.map(|backend| backend.locator)],
            "a changed locator must re-render the router config"
        );
    }

    #[tokio::test]
    async fn cached_desired_state_from_a_pinned_router_is_not_reapplied() {
        let (resolver, _) = counting_resolver(upstream());
        let (federator, apply_calls) = counting_pinned_federator();
        let (prober, probe_calls) = counting_prober(Ok(()));
        let mut applied = AppliedState::default();

        let poller = poller_under_test(federator, resolver, prober);
        let first = poller.poll_and_apply(&mut applied, false).await;
        let second = poller.poll_and_apply(&mut applied, false).await;

        assert_eq!(first, FederationOutcome::Pinned);
        assert_eq!(
            second,
            FederationOutcome::Pinned,
            "an identical desired state must replay the cached pinned outcome"
        );
        assert_eq!(
            apply_calls.load(Ordering::SeqCst),
            1,
            "the second poll must not re-attempt the rejected apply"
        );
        assert_eq!(probe_calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn pinned_apply_preserves_actual_state_and_caches_the_rejected_target() {
        let (resolver, _) = counting_resolver(upstream());
        let (federator, _) = counting_pinned_federator();
        let (prober, _) = counting_prober(Ok(()));
        let mut applied = AppliedState::default();

        let outcome = poller_under_test(federator, resolver, prober)
            .poll_and_apply(&mut applied, false)
            .await;

        assert_eq!(outcome, FederationOutcome::Pinned);
        assert_eq!(
            applied.endpoint, None,
            "the rejected desired endpoint must not leak into applied state"
        );
        assert_eq!(applied.link_state, LinkState::NotConfigured);
        assert!(applied.pinned);
        assert_eq!(
            applied.last_settled_desired,
            SettledDesired::Upstream(ENDPOINT.to_string()),
            "the rejected target is cached so an identical repeat stays Pinned"
        );
    }

    #[tokio::test]
    async fn a_resolver_error_is_failed_and_preserves_applied_state() {
        let resolver: Resolver = Arc::new(|| Err("credentials file unreadable".to_string()));
        let (federator, calls) = recording_federator();
        let (prober, _) = counting_prober(Ok(()));
        let mut applied = AppliedState {
            endpoint: Some(ENDPOINT.to_string()),
            link_state: LinkState::Verified,
            last_settled_desired: SettledDesired::Upstream(ENDPOINT.to_string()),
            pinned: false,
            needs_reapply: false,
        };
        let before = applied.clone();

        let outcome = poller_under_test(federator, resolver, prober)
            .poll_and_apply(&mut applied, false)
            .await;

        assert!(matches!(outcome, FederationOutcome::Failed(_)));
        assert_eq!(
            applied, before,
            "a failed resolve must never drop the applied link"
        );
        assert!(calls.lock().unwrap().is_empty());
    }

    /// A verifying re-run after a probe failure must bounce the router even
    /// though the desired locator is unchanged: the user may have replaced the
    /// certificate files between attempts.
    #[tokio::test]
    async fn a_verifying_rerun_after_probe_failure_rebounces_the_unchanged_upstream() {
        let (resolver, _) = counting_resolver(upstream());
        let (federator, calls) = recording_federator();
        let failures = Arc::new(AtomicUsize::new(0));
        let fail_once = failures.clone();
        let prober: Prober = Arc::new(move |_host, _port, _tls, _timeout| -> ProbeFuture {
            let first = fail_once.fetch_add(1, Ordering::SeqCst) == 0;
            Box::pin(async move {
                if first {
                    Err("received fatal alert: UnknownCA".to_string())
                } else {
                    Ok(())
                }
            })
        });
        let poller = poller_under_test(federator, resolver, prober);
        let mut applied = AppliedState::default();

        let first = poller.poll_and_apply(&mut applied, true).await;
        assert_eq!(
            first,
            FederationOutcome::Applied(PlatformLink {
                endpoint: Some(ENDPOINT.to_string()),
                link_state: LinkState::Error("received fatal alert: UnknownCA".to_string()),
            })
        );
        assert!(applied.needs_reapply);

        let second = poller.poll_and_apply(&mut applied, true).await;
        assert_eq!(
            second,
            FederationOutcome::Applied(PlatformLink {
                endpoint: Some(ENDPOINT.to_string()),
                link_state: LinkState::Verified,
            })
        );
        assert!(!applied.needs_reapply);
        assert_eq!(
            calls.lock().unwrap().len(),
            2,
            "the verifying re-run must bounce the router again"
        );
    }

    #[tokio::test]
    async fn a_wedged_apply_times_out_and_reports_failed() {
        let (resolver, _) = counting_resolver(upstream());
        let (prober, probe_calls) = counting_prober(Ok(()));
        let mut applied = AppliedState::default();

        let mut poller = poller_under_test(wedged_federator(), resolver, prober);
        poller.connect_timeout = Duration::from_millis(50);
        poller.apply_timeout = Duration::from_millis(50);
        let outcome = poller.poll_and_apply(&mut applied, true).await;

        assert!(
            matches!(outcome, FederationOutcome::Failed(_)),
            "a wedged apply must time out into Failed, got {outcome:?}"
        );
        assert_eq!(
            applied,
            AppliedState::default(),
            "a timed-out apply must leave `applied` unchanged so the next poll retries"
        );
        assert_eq!(
            probe_calls.load(Ordering::SeqCst),
            0,
            "no probe after a failed apply"
        );
    }

    #[tokio::test]
    async fn apply_receives_a_fresh_budget_after_a_slow_resolve() {
        let resolver: Resolver = Arc::new(|| {
            std::thread::sleep(Duration::from_millis(160));
            Ok(resolved(upstream()))
        });
        let federator: Federator = Arc::new(|_upstream| {
            Box::pin(async {
                tokio::time::sleep(Duration::from_millis(100)).await;
                Ok(true)
            })
        });
        let (prober, _) = counting_prober(Ok(()));
        let mut applied = AppliedState::default();

        let mut poller = poller_under_test(federator, resolver, prober);
        poller.connect_timeout = Duration::from_millis(200);
        let outcome = poller.poll_and_apply(&mut applied, false).await;

        assert!(
            matches!(outcome, FederationOutcome::Applied(_)),
            "the apply must not inherit only the resolve deadline's remaining 40ms"
        );
    }

    #[tokio::test]
    async fn the_upstream_probe_receives_an_unbracketed_ipv6_host() {
        let endpoint = "tls/[2001:db8::1]:7447";
        let (resolver, _) = counting_resolver(Some(DesiredBackend {
            endpoint: dial(endpoint),
            locator: endpoint.to_string(),
            tls: pmi::TlsConfig::default(),
        }));
        let (federator, _) = recording_federator();
        let seen = Arc::new(std::sync::Mutex::new(None));
        let capture = seen.clone();
        let prober: Prober = Arc::new(move |host, port, _tls, _timeout| {
            *capture.lock().unwrap() = Some((host, port));
            Box::pin(async { Ok(()) })
        });
        let mut applied = AppliedState::default();

        let outcome = poller_under_test(federator, resolver, prober)
            .poll_and_apply(&mut applied, true)
            .await;

        assert!(matches!(outcome, FederationOutcome::Applied(_)));
        assert_eq!(
            *seen.lock().unwrap(),
            Some(("2001:db8::1".to_string(), 7447))
        );
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

        let (mut federation, _status_rx) = federation_under_test(
            applying_federator(),
            resolver,
            prober,
            messaging_rx,
            trigger_rx,
        );
        federation.poller.connect_timeout = Duration::from_secs(5);
        let task = tokio::spawn(federation.manage(ready_tx));

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
        trigger_tx.send(ack_tx).await.expect("trigger accepted");
        let outcome = tokio::time::timeout(Duration::from_secs(1), ack_rx)
            .await
            .expect("the poke is serviced immediately")
            .expect("ack sender not dropped");

        // The upstream is already in effect from the initial poll, so the poke
        // re-resolves, the probe succeeds, and it reports the verified link.
        assert_eq!(
            outcome,
            FederationOutcome::Applied(PlatformLink {
                endpoint: Some(ENDPOINT.to_string()),
                link_state: LinkState::Verified,
            })
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

        let (mut federation, _status_rx) = federation_under_test(
            applying_federator(),
            resolver,
            prober,
            messaging_rx,
            trigger_rx,
        );
        // A deliberately large connect_timeout: the probe must NOT inherit it.
        federation.poller.connect_timeout = Duration::from_secs(45);
        let task = tokio::spawn(federation.manage(ready_tx));
        tokio::time::timeout(Duration::from_secs(1), ready_rx)
            .await
            .expect("startup gate fires")
            .expect("gate sender not dropped");

        let (ack_tx, ack_rx) = oneshot::channel();
        trigger_tx.send(ack_tx).await.expect("trigger accepted");
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
    /// reported as an errored link, not a false verified success, even though
    /// the config was applied, and the cached status carries the same error.
    #[tokio::test]
    async fn poke_with_failing_probe_reports_a_link_error() {
        let (resolver, _calls) = counting_resolver(upstream());
        let reason = "received fatal alert: UnknownCA";
        let (prober, probe_calls) = counting_prober(Err(reason.to_string()));
        let (messaging_tx, messaging_rx) = watch::channel(true);
        let (trigger_tx, trigger_rx) = mpsc::channel(8);
        let (ready_tx, ready_rx) = oneshot::channel();

        let (mut federation, status_rx) = federation_under_test(
            applying_federator(),
            resolver,
            prober,
            messaging_rx,
            trigger_rx,
        );
        federation.poller.connect_timeout = Duration::from_secs(5);
        let task = tokio::spawn(federation.manage(ready_tx));

        tokio::time::timeout(Duration::from_secs(1), ready_rx)
            .await
            .expect("startup gate fires")
            .expect("gate sender not dropped");

        let (ack_tx, ack_rx) = oneshot::channel();
        trigger_tx.send(ack_tx).await.expect("trigger accepted");
        let outcome = tokio::time::timeout(Duration::from_secs(1), ack_rx)
            .await
            .expect("poke serviced immediately")
            .expect("ack sender not dropped");

        assert_eq!(
            outcome,
            FederationOutcome::Applied(PlatformLink {
                endpoint: Some(ENDPOINT.to_string()),
                link_state: LinkState::Error(reason.to_string()),
            }),
            "a failing probe reports the applied endpoint with an errored link"
        );
        assert_eq!(
            probe_calls.load(Ordering::SeqCst),
            1,
            "the poke probed once"
        );

        // The published status carries the same errored link for
        // `platform federations` (the watch was updated before the ack).
        let status = status_rx.borrow().clone();
        assert_eq!(status.link.endpoint.as_deref(), Some(ENDPOINT));
        assert_eq!(status.link.link_state, LinkState::Error(reason.to_string()));

        drop(messaging_tx);
        task.abort();
    }

    /// An operator-pinned config (`refederate` reports no rewrite) must keep
    /// reporting `Pinned` on a repeat and must not publish the desired endpoint
    /// as applied or probe it.
    #[tokio::test]
    async fn poke_on_pinned_config_stays_pinned_and_does_not_probe() {
        let (resolver, _) = counting_resolver(upstream());
        let (prober, probe_calls) = counting_prober(Ok(()));
        let (messaging_tx, messaging_rx) = watch::channel(true);
        let (trigger_tx, trigger_rx) = mpsc::channel(8);
        let (ready_tx, ready_rx) = oneshot::channel();
        let (federator, apply_calls) = counting_pinned_federator();

        let (mut federation, status_rx) =
            federation_under_test(federator, resolver, prober, messaging_rx, trigger_rx);
        federation.poller.connect_timeout = Duration::from_secs(5);
        federation.initial_pinned = true;
        let task = tokio::spawn(federation.manage(ready_tx));

        tokio::time::timeout(Duration::from_secs(1), ready_rx)
            .await
            .expect("startup gate fires")
            .expect("gate sender not dropped");

        let status = status_rx.borrow().clone();
        assert!(status.link.endpoint.is_none());
        assert_eq!(status.link.link_state, LinkState::NotConfigured);
        assert!(status.pinned);

        let (ack_tx, ack_rx) = oneshot::channel();
        trigger_tx.send(ack_tx).await.expect("trigger accepted");
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
        assert_eq!(
            apply_calls.load(Ordering::SeqCst),
            1,
            "the startup rejection must cache the desired state for an identical poke"
        );

        drop(messaging_tx);
        task.abort();
    }

    /// Once the local router is ready, the startup gate fires within the resolve
    /// timeout even when the backend is slow enough to blow the bound. The
    /// federation loop then keeps retrying.
    #[tokio::test]
    async fn startup_gate_fires_within_timeout_when_resolve_is_slow() {
        let resolver: Resolver = Arc::new(|| {
            std::thread::sleep(Duration::from_secs(5));
            Ok(resolved(None))
        });
        let (prober, _) = counting_prober(Ok(()));
        let (messaging_tx, messaging_rx) = watch::channel(true);
        let (_trigger_tx, trigger_rx) = mpsc::channel(8);
        let (ready_tx, ready_rx) = oneshot::channel();

        let (mut federation, _status_rx) = federation_under_test(
            applying_federator(),
            resolver,
            prober,
            messaging_rx,
            trigger_rx,
        );
        federation.poller.connect_timeout = Duration::from_millis(100);
        let task = tokio::spawn(federation.manage(ready_tx));

        let fired = tokio::time::timeout(Duration::from_secs(2), ready_rx).await;
        assert!(
            fired.is_ok(),
            "the startup gate must fire within the bound even when the resolve is slow"
        );

        drop(messaging_tx);
        task.abort();
    }

    /// The core node's presence gate fires in lockstep with the startup gate,
    /// once the initial federation has settled.
    #[tokio::test]
    async fn presence_gate_fires_once_the_initial_federation_settled() {
        let (resolver, _) = counting_resolver(None);
        let (prober, _) = counting_prober(Ok(()));
        let (messaging_tx, messaging_rx) = watch::channel(true);
        let (_trigger_tx, trigger_rx) = mpsc::channel(8);
        let (ready_tx, ready_rx) = oneshot::channel();
        let (presence_tx, mut presence_rx) = watch::channel(false);

        let (mut federation, _status_rx) = federation_under_test(
            applying_federator(),
            resolver,
            prober,
            messaging_rx,
            trigger_rx,
        );
        federation.poller.connect_timeout = Duration::from_secs(5);
        federation.presence_gate_tx = Some(presence_tx);
        let task = tokio::spawn(federation.manage(ready_tx));

        tokio::time::timeout(Duration::from_secs(1), ready_rx)
            .await
            .expect("startup gate fires")
            .expect("gate sender not dropped");
        tokio::time::timeout(Duration::from_secs(1), presence_rx.wait_for(|fired| *fired))
            .await
            .expect("presence gate fires with the startup gate")
            .expect("presence sender not dropped");

        drop(messaging_tx);
        task.abort();
    }

    #[tokio::test]
    async fn logged_out_initial_poll_is_a_noop_and_fires_the_gate() {
        let (resolver, calls) = counting_resolver(None);
        let (prober, probe_calls) = counting_prober(Ok(()));
        let (messaging_tx, messaging_rx) = watch::channel(true);
        let (_trigger_tx, trigger_rx) = mpsc::channel(8);
        let (ready_tx, ready_rx) = oneshot::channel();

        let (mut federation, _status_rx) = federation_under_test(
            applying_federator(),
            resolver,
            prober,
            messaging_rx,
            trigger_rx,
        );
        federation.poller.connect_timeout = Duration::from_secs(5);
        let task = tokio::spawn(federation.manage(ready_tx));

        tokio::time::timeout(Duration::from_secs(1), ready_rx)
            .await
            .expect("gate fires")
            .expect("gate sender not dropped");
        assert!(calls.load(Ordering::SeqCst) >= 1);
        assert_eq!(
            probe_calls.load(Ordering::SeqCst),
            0,
            "no upstream means nothing to probe"
        );

        drop(messaging_tx);
        task.abort();
    }

    /// A poke after the credentials change the daemon's namespace acks `Restart`
    /// carrying the target namespace (the control handler then triggers a
    /// generation restart). The loop must NOT federate or probe on a namespace
    /// change; a restart is fail-closed. The change appears only at the poke (the
    /// startup poll still sees `local`), so the startup-restart path stays dormant
    /// and the steady-state ack is exercised.
    #[tokio::test]
    async fn poke_acks_restart_on_a_namespace_change() {
        // Startup resolves `local` (matches the startup namespace, so no startup
        // restart); the poke resolves the changed workspace, a steady-state Restart.
        let resolver =
            namespace_switching_resolver(upstream(), Namespace::local(), workspace_namespace());
        let (prober, probe_calls) = counting_prober(Ok(()));
        let (messaging_tx, messaging_rx) = watch::channel(true);
        let (trigger_tx, trigger_rx) = mpsc::channel(8);
        let (ready_tx, ready_rx) = oneshot::channel();
        // The startup poll must NOT raise the restart signal in this scenario.
        let (restart_tx, restart_rx) = watch::channel(false);

        let (mut federation, _status_rx) = federation_under_test(
            applying_federator(),
            resolver,
            prober,
            messaging_rx,
            trigger_rx,
        );
        federation.poller.connect_timeout = Duration::from_secs(5);
        federation.restart_tx = restart_tx;
        let task = tokio::spawn(federation.manage(ready_tx));

        tokio::time::timeout(Duration::from_secs(1), ready_rx)
            .await
            .expect("startup gate fires")
            .expect("gate sender not dropped");
        assert!(
            !*restart_rx.borrow(),
            "the startup poll saw an unchanged namespace, so no startup restart"
        );

        let (ack_tx, ack_rx) = oneshot::channel();
        trigger_tx.send(ack_tx).await.expect("trigger accepted");
        let outcome = tokio::time::timeout(Duration::from_secs(1), ack_rx)
            .await
            .expect("poke serviced immediately")
            .expect("ack sender not dropped");

        assert_eq!(
            outcome,
            FederationOutcome::Restart {
                target_namespace: workspace_namespace()
            },
            "a namespace change must ack Restart with the target namespace"
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
        // Every resolve returns a workspace that differs from the `local` startup
        // namespace, so the very first (startup) poll detects the drift.
        let (resolver, resolve_calls) =
            counting_resolver_with_namespace(upstream(), workspace_namespace());
        let (prober, probe_calls) = counting_prober(Ok(()));
        let (messaging_tx, messaging_rx) = watch::channel(true);
        let (_trigger_tx, trigger_rx) = mpsc::channel(8);
        let (ready_tx, ready_rx) = oneshot::channel();
        let (restart_tx, mut restart_rx) = watch::channel(false);

        let (mut federation, _status_rx) = federation_under_test(
            applying_federator(),
            resolver,
            prober,
            messaging_rx,
            trigger_rx,
        );
        federation.poller.connect_timeout = Duration::from_secs(5);
        federation.restart_tx = restart_tx;
        let task = tokio::spawn(federation.manage(ready_tx));

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
            resolve_calls.load(Ordering::SeqCst) >= 1,
            "the resolve ran and carried the namespace that revealed the drift"
        );

        drop(messaging_tx);
        task.abort();
    }
}
