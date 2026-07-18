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
//!   place. Once the local router is ready, backend resolution is bounded by
//!   `connect_timeout` and router application by [`APPLY_TIMEOUT`], so a slow or
//!   unreachable backend cannot stall startup indefinitely (the daemon then
//!   proceeds standalone and keeps retrying in the background). At the same
//!   moments it fires the core node's
//!   *presence gate* (`presence_gate_tx`): the core node delays its boot-time
//!   presence check and declaration until the initial federation has settled,
//!   so the check sees the federated mesh (a same-name daemon reachable only
//!   through the cloud router refuses boot) rather than the always-standalone
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

use crate::control::{AppliedFederation, FederationStatus, PeerLinkState, PeerReport};
use crate::error::{Error, Result};
use crate::serve::{ServeAsyncCommand, ServeAsyncHandle};
use daemon_config::consts::PeppyDirs;
use daemon_config::peppy_config::{EndpointPurpose, ParsedEndpointBuf};
use federation::PeerLink;
use federation::links::IdentityPaths;
use pmi::{Messenger, MessengerBackend, RouterLinks};
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{Mutex, mpsc, oneshot, watch};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

/// How long a *verifying* login/logout poke waits for the federation link's TLS
/// handshake to validate. Deliberately small and decoupled from `connect_timeout`
/// (the resolve bound): a healthy handshake is sub-second, so a tight bound keeps
/// the whole verifying poll (resolve + zenohd bounce + probe) inside the daemon's
/// ack budget: `connect_timeout` + [`super::federation_control`]'s
/// `APPLY_ACK_SLACK`, which is sized to cover apply plus probe. An unreachable /
/// firewalled router fails the probe within this bound and surfaces promptly as
/// [`FederationOutcome::Unreachable`] rather than as a daemon-side ack timeout.
pub(crate) const PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// Post-resolve budget for rewriting the managed router and waiting for zenohd
/// to accept connections again. Kept separate from the backend resolve timeout.
pub(crate) const APPLY_TIMEOUT: Duration = Duration::from_secs(4);

/// Failed resolves or rewrites are retried without turning the federation loop
/// back into a keepalive poll. Once desired state applies, the timer is idle and
/// zenohd owns link reconnection.
const RETRY_DELAY: Duration = Duration::from_secs(5);

/// Static listener and identity inputs resolved once when the daemon starts.
#[derive(Debug, Clone)]
pub(crate) struct FederationLinksSpec {
    pub(crate) extra_listen_endpoints: Vec<String>,
    pub(crate) identity: IdentityPaths,
    pub(crate) initial_peers: Vec<String>,
    /// Raw inbound listener endpoint (fragment-free) for status reporting.
    pub(crate) listen_endpoint: Option<String>,
    pub(crate) initial_pinned: bool,
}

impl FederationLinksSpec {
    /// A pinned router never opens the user-federation listener, so status
    /// must not report one.
    fn status_listen_endpoint(&self) -> Option<String> {
        if self.initial_pinned {
            return None;
        }
        self.listen_endpoint.clone()
    }
}

#[derive(Debug)]
enum ProbeTarget {
    Backend,
    Peer(String),
}

/// The platform-backend federation target: the durable endpoint stays
/// separate from the rendered locator, mirroring [`PeerLink`].
#[derive(Debug, Clone)]
struct DesiredBackend {
    endpoint: ParsedEndpointBuf,
    locator: String,
    tls: pmi::TlsConfig,
}

/// Complete desired router state from backend credentials plus the user peer
/// registry.
#[derive(Debug, Clone, Default)]
struct DesiredFederation {
    backend: Option<DesiredBackend>,
    peers: Vec<PeerLink>,
    /// Probe TLS settings for every user peer: there is one fleet identity per
    /// daemon, so a single config covers the whole registry.
    peer_probe_tls: pmi::TlsConfig,
}

impl DesiredFederation {
    fn backend_endpoint(&self) -> Option<String> {
        self.backend
            .as_ref()
            .map(|backend| backend.endpoint.as_str().to_string())
    }

    fn peer_endpoints(&self) -> Vec<String> {
        let mut endpoints: Vec<String> = self
            .peers
            .iter()
            .map(|peer| peer.endpoint.as_str().to_string())
            .collect();
        endpoints.sort();
        endpoints
    }

    fn connect_locators(&self) -> Vec<String> {
        self.backend
            .iter()
            .map(|backend| backend.locator.clone())
            .chain(self.peers.iter().map(|peer| peer.locator.clone()))
            .collect()
    }
}

/// Resolves the full desired state. Registry parse or read failures remain
/// explicit so a bad file never silently drops existing links.
type Resolver = Arc<dyn Fn() -> std::result::Result<DesiredFederation, String> + Send + Sync>;

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

/// The future a [`Federator`] returns: `Ok(true)` ⇒ the local router's config was
/// (re)rendered and zenohd bounced, `Ok(false)` ⇒ nothing was applied because the
/// managed router uses a pinned `ZENOH_CONFIG`, `Err` ⇒ the apply failed.
type FederateFuture = Pin<Box<dyn Future<Output = Result<bool>> + Send>>;

/// Applies a desired upstream to the local router (re-render + bounce). A boxed
/// async closure so tests can inject a deterministic federation result:
/// `Ok(true)` (a real rewrite) or `Ok(false)` (operator-pinned), in place of the
/// real [`refederate_and_restart`], whose mock backend can only ever report
/// `Ok(false)` and so cannot exercise the applied/verify path.
type Federator = Arc<dyn Fn(Vec<String>) -> FederateFuture + Send + Sync>;

/// The real federator: re-render the owned router's config with the upstream and,
/// if it changed, bounce zenohd (see [`refederate_and_restart`]).
fn real_federator(
    messenger: Arc<Mutex<Messenger>>,
    extra_listen_endpoints: Vec<String>,
) -> Federator {
    Arc::new(move |connect_endpoints| -> FederateFuture {
        let messenger = messenger.clone();
        let extra_listen_endpoints = extra_listen_endpoints.clone();
        Box::pin(async move {
            refederate_and_restart(&messenger, connect_endpoints, extra_listen_endpoints).await
        })
    })
}

/// Peers not yet checked by a verifying poke, as seeded into the status cache
/// at startup and replayed while no verified report exists.
fn unverified_reports(endpoints: &[String]) -> Vec<PeerReport> {
    endpoints
        .iter()
        .map(|endpoint| PeerReport {
            endpoint: endpoint.clone(),
            state: PeerLinkState::Unverified,
        })
        .collect()
}

/// Outcome of one federation poll, reported back to a control-socket poke so the
/// CLI can tell the user the post-apply state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum FederationOutcome {
    /// Federation is in effect: `Some(ep)` federated to `ep`, `None`
    /// de-federated. Covers both "just applied" and "already in place" (a no-op
    /// poll where the upstream was unchanged). On a login poke this means the TLS
    /// link to the upstream was also verified to validate.
    Applied(AppliedFederation),
    /// The managed router uses a pinned `ZENOH_CONFIG`, so nothing changed.
    Pinned,
    /// The resolve or apply failed; the periodic loop will keep retrying.
    Failed(String),
    /// The config was applied (the local router was federated), but the TLS link
    /// to the per-user cloud router could not be established/validated, so
    /// federation with platform-backend is NOT actually in effect (e.g. an
    /// UnknownCA handshake loop). Only a verifying poke produces this.
    Unreachable {
        reason: String,
        applied: AppliedFederation,
    },
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

/// The real namespace resolver: read the cached organization id from the
/// generation's credentials file and resolve it to a namespace (absent ->
/// `local`), matching exactly how the daemon generation resolved its own
/// namespace at startup (the same [`auth::storage::credentials_path`] derived
/// from the same `PeppyDirs`).
fn real_namespace_resolver(creds_path: PathBuf) -> NamespaceResolver {
    Arc::new(move || {
        config::org::resolve_session_namespace(
            auth::router::cached_organization_id(&creds_path).as_deref(),
        )
        .as_str()
        .to_string()
    })
}

/// Requests accepted from the control socket.
pub(crate) enum FederationRequest {
    Refederate {
        ack: oneshot::Sender<FederationOutcome>,
    },
    Status {
        ack: oneshot::Sender<FederationStatus>,
    },
}

/// Sends pokes to the federation loop (held by [`FederationControl`]).
pub(crate) type TriggerSender = mpsc::Sender<FederationRequest>;
/// Receives pokes in the federation loop.
pub(crate) type TriggerReceiver = mpsc::Receiver<FederationRequest>;

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
    /// This generation's organization namespace, resolved once at startup. A poll
    /// that re-resolves a *different* namespace from fresh creds requests a
    /// restart instead of a live re-federation.
    startup_namespace: String,
    /// Resolves the current namespace from the credentials (post-pull), compared
    /// against `startup_namespace` to detect a namespace change.
    namespace_resolver: NamespaceResolver,
}

/// Background task (a [`ServeAsyncCommand`]) that federates the local router to
/// the per-user cloud router and keeps it federated. See the module docs.
pub(crate) struct RouterFederation {
    poller: FederationPoller,
    /// Goes `true` once the router process is up (MessagingRouter ran
    /// `start_router` + `start_session`). The task waits on this before touching
    /// the router so its initial federation cannot race the router's own startup.
    messaging_ready: watch::Receiver<bool>,
    /// Immediate-refederation pokes from the control socket.
    trigger_rx: TriggerReceiver,
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
    /// Peers already rendered into zenohd's spawn config by the builder.
    initial_peers: Vec<String>,
    /// Raw inbound listener endpoint for status reporting.
    listen_endpoint: Option<String>,
    /// Whether the router was operator-pinned before the federation task began.
    initial_pinned: bool,
}

impl RouterFederation {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        messenger: Arc<Mutex<Messenger>>,
        api_url: String,
        core_node_name: String,
        peppy_dirs: PeppyDirs,
        messaging_ready: watch::Receiver<bool>,
        trigger_rx: TriggerReceiver,
        connect_timeout: Duration,
        startup_namespace: String,
        restart_tx: watch::Sender<bool>,
        presence_gate_tx: Option<watch::Sender<bool>>,
        links: FederationLinksSpec,
        teardown_token: CancellationToken,
    ) -> Self {
        // Both ambient inputs the loop re-reads on every poll derive from the
        // generation's data root: the credentials file (namespace re-resolve)
        // and the federation resolve (credentials + materialized dev TLS).
        let creds_path = auth::storage::credentials_path(&peppy_dirs);
        let resolver_dirs = peppy_dirs.clone();
        let identity = links.identity.clone();
        let resolver: Resolver = Arc::new(move || {
            let backend = auth::router::resolve_federation_target(
                &resolver_dirs,
                &api_url,
                connect_timeout,
                &core_node_name,
            )
            .map(|(endpoint, tls)| {
                let endpoint =
                    ParsedEndpointBuf::parse(endpoint.as_str(), "tls", EndpointPurpose::Dial)
                        .map_err(|error| {
                            format!("invalid backend endpoint {endpoint:?}: {error}")
                        })?;
                let locator = federation::backend_connect_locator(&endpoint, &tls)
                    .map_err(|error| error.to_string())?;
                Ok::<_, String>(DesiredBackend {
                    endpoint,
                    locator,
                    tls,
                })
            })
            .transpose()?;

            let registry = federation::load(&federation::registry_path(&resolver_dirs))
                .map_err(|error| error.to_string())?;
            let peers =
                federation::peer_links(&registry, &identity).map_err(|error| error.to_string())?;

            Ok(DesiredFederation {
                backend,
                peers,
                peer_probe_tls: federation::peer_probe_tls(&identity),
            })
        });
        let listen_endpoint = links.status_listen_endpoint();
        Self {
            poller: FederationPoller {
                federator: real_federator(messenger, links.extra_listen_endpoints),
                resolver,
                prober: real_prober(),
                connect_timeout,
                apply_timeout: APPLY_TIMEOUT,
                startup_namespace,
                namespace_resolver: real_namespace_resolver(creds_path),
            },
            messaging_ready,
            trigger_rx,
            restart_tx,
            presence_gate_tx,
            teardown_token,
            initial_peers: links.initial_peers,
            listen_endpoint,
            initial_pinned: links.initial_pinned,
        }
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
/// [`APPLY_TIMEOUT`], and fail-open (a dropped sender ⇒ the waiter proceeds).
fn fire_gate(gate: &mut Option<oneshot::Sender<()>>, presence_gate: &Option<watch::Sender<bool>>) {
    if let Some(tx) = gate.take() {
        let _ = tx.send(());
    }
    if let Some(tx) = presence_gate {
        let _ = tx.send(true);
    }
}

/// What the last completed poll left in effect, cached across polls so an
/// identical repeat (the same desired upstream) is answered from the fast path
/// without re-applying. Richer than the bare endpoint string: it also remembers
/// whether the managed router uses an operator-pinned `ZENOH_CONFIG`, so we did
/// not actually apply the upstream.
/// Without the `pinned` bit a repeat of such a target would match on endpoint
/// alone and be misreported as [`FederationOutcome::Applied`] instead of
/// [`FederationOutcome::Pinned`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct AppliedState {
    /// Platform backend endpoint now in effect.
    backend: Option<String>,
    /// User peer endpoints now rendered into the managed router config.
    peers: Vec<String>,
    /// Whether the managed router uses a pinned `ZENOH_CONFIG` (so the desired
    /// change was not applied here), replayed so identical repeats stay `Pinned`.
    pinned: bool,
    /// The last verifying poke found a TLS failure. A later verifying poke must
    /// bounce the router even when endpoints are unchanged, because the user may
    /// have replaced local certificate files before re-running the command.
    needs_reapply: bool,
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
            restart_tx,
            presence_gate_tx,
            teardown_token: _,
            mut initial_peers,
            listen_endpoint,
            initial_pinned,
        } = self;
        // Normalized once: seeds both the status cache and the applied-state
        // cache below.
        initial_peers.sort();
        initial_peers.dedup();
        let (status_tx, status_rx) = watch::channel(FederationStatus {
            backend: None,
            peers: unverified_reports(&initial_peers),
            listen_endpoint: listen_endpoint.clone(),
            pinned: initial_pinned,
        });
        let (refederate_tx, refederate_rx) = mpsc::unbounded_channel();

        // Run request dispatch in its own task. Router lifecycle code may spend
        // time in process-management work, and a sibling future in this same
        // task would not be polled while that work is in progress.
        let dispatcher = tokio::spawn(dispatch_federation_requests(
            trigger_rx,
            refederate_tx,
            status_rx,
        ));
        let lifecycle = FederationLoop {
            poller,
            ready_tx: Some(ready_tx),
            restart_tx,
            presence_gate_tx,
            status_tx,
            listen_endpoint,
        };
        // A managed router starts standalone, so `None` is already applied. A
        // pinned `ZENOH_CONFIG` is detected only if applying a later change
        // returns `Ok(false)`.
        let initial = AppliedState {
            backend: None,
            peers: initial_peers,
            pinned: initial_pinned,
            needs_reapply: false,
        };
        lifecycle.run(messaging_ready, refederate_rx, initial).await;
        dispatcher.abort();
    }
}

/// Keeps status queries responsive while a refederation is resolving, bouncing
/// zenohd, or probing links. Refederation work remains serialized in the core
/// loop; cached status requests are answered directly from the watch value.
async fn dispatch_federation_requests(
    mut trigger_rx: TriggerReceiver,
    refederate_tx: mpsc::UnboundedSender<oneshot::Sender<FederationOutcome>>,
    status_rx: watch::Receiver<FederationStatus>,
) {
    while let Some(request) = trigger_rx.recv().await {
        match request {
            FederationRequest::Status { ack } => {
                let _ = ack.send(status_rx.borrow().clone());
            }
            FederationRequest::Refederate { ack } => {
                if let Err(error) = refederate_tx.send(ack) {
                    let _ = error.0.send(FederationOutcome::Failed(
                        "federation task not running".to_string(),
                    ));
                }
            }
        }
    }
}

/// The federation lifecycle after setup: the poll engine plus the channels and
/// cached listener state its phases share. Split from
/// [`RouterFederation::manage`] so the phases can early-return while the
/// dispatcher abort stays with the caller.
struct FederationLoop {
    poller: FederationPoller,
    /// Startup readiness gate, `take`n by [`fire_gate`] on first fire.
    ready_tx: Option<oneshot::Sender<()>>,
    restart_tx: watch::Sender<bool>,
    presence_gate_tx: Option<watch::Sender<bool>>,
    status_tx: watch::Sender<FederationStatus>,
    listen_endpoint: Option<String>,
}

impl FederationLoop {
    async fn run(
        mut self,
        mut messaging_ready: watch::Receiver<bool>,
        mut refederate_rx: mpsc::UnboundedReceiver<oneshot::Sender<FederationOutcome>>,
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
        let mut last_peer_reports = match &initial_outcome {
            FederationOutcome::Applied(report)
            | FederationOutcome::Unreachable {
                applied: report, ..
            } => report.peers.clone(),
            _ => unverified_reports(&applied.peers),
        };
        self.publish_status(&applied, &last_peer_reports);
        fire_gate(&mut self.ready_tx, &self.presence_gate_tx);

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
        if matches!(&initial_outcome, FederationOutcome::Restart) {
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
                    request = refederate_rx.recv() => match request {
                        Some(ack) => Work::Poke(ack),
                        None => break,
                    },
                    _ = tokio::time::sleep(RETRY_DELAY) => Work::Retry,
                }
            } else {
                let Some(ack) = refederate_rx.recv().await else {
                    break;
                };
                Work::Poke(ack)
            };
            let verify = matches!(&work, Work::Poke(_));
            let outcome = self.poller.poll_and_apply(&mut applied, verify).await;
            match &outcome {
                FederationOutcome::Applied(report)
                | FederationOutcome::Unreachable {
                    applied: report, ..
                } => last_peer_reports = report.peers.clone(),
                _ => {}
            }
            self.publish_status(&applied, &last_peer_reports);
            retry_pending = matches!(&outcome, FederationOutcome::Failed(_));
            match work {
                Work::Poke(ack) => {
                    // The CLI may have already given up (read timeout); ignore. On
                    // a namespace change the control handler raises the restart
                    // after attempting to flush `Restarting`.
                    let _ = ack.send(outcome);
                }
                Work::Retry if matches!(&outcome, FederationOutcome::Restart) => {
                    let _ = self.restart_tx.send(true);
                    return;
                }
                Work::Retry => {}
            }
        }
    }

    /// Publishes the cached status answered to [`FederationRequest::Status`]
    /// queries: the applied endpoints, each peer carrying its last reported
    /// link state (peers never reported on stay at their cached state's
    /// default of verified, matching what the applied cache asserts).
    fn publish_status(&self, applied: &AppliedState, last_peer_reports: &[PeerReport]) {
        let states: std::collections::BTreeMap<&str, &PeerLinkState> = last_peer_reports
            .iter()
            .map(|report| (report.endpoint.as_str(), &report.state))
            .collect();
        self.status_tx.send_replace(FederationStatus {
            backend: applied.backend.clone(),
            peers: applied
                .peers
                .iter()
                .map(|endpoint| PeerReport {
                    endpoint: endpoint.clone(),
                    state: states
                        .get(endpoint.as_str())
                        .map(|state| (*state).clone())
                        .unwrap_or(PeerLinkState::Verified),
                })
                .collect(),
            listen_endpoint: self.listen_endpoint.clone(),
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
    /// federation), and an upstream is in effect, a real TLS handshake confirms the link
    /// actually validates; a failed handshake is reported as
    /// [`FederationOutcome::Unreachable`] (and logged loudly) instead of a false
    /// `Applied`.
    async fn poll_and_apply(&self, applied: &mut AppliedState, verify: bool) -> FederationOutcome {
        // Backend resolution and the namespace refresh share the configured
        // resolve deadline. Router process work receives its own bounded budget.
        let resolve_deadline = tokio::time::Instant::now() + self.connect_timeout;

        // The resolver is blocking (HTTP + file I/O); keep it off the async worker. It
        // also re-pulls the cloud router's config when the cached copy has gone stale
        // (cache freshness only, not a keepalive). Bound the whole resolve by
        // `connect_timeout` so a hung pull can't
        // stall a poll (or the startup gate) past it; the timed-out blocking thread is
        // harmless (its own HTTP timeout ends it) and its result is simply discarded.
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
        // federation config, so the credentials now reflect the current org id. A
        // session's namespace is immutable after open, so if the re-resolved namespace
        // differs from this generation's startup namespace the change cannot be applied
        // by a live zenodh bounce: request a full restart instead, WITHOUT federating
        // (federating under a namespace that differs from the live session's would leak
        // across tenants). The control handler flushes the ack before triggering the
        // restart; the initial (non-poke) poll discards this outcome but, crucially,
        // also does not federate, so it stays fail-closed until the next generation.
        // Like the resolve above, the namespace re-resolve is blocking (a file-backed
        // credentials read); keep it off the async worker and inside the same poll
        // deadline so unusual filesystem stalls cannot consume the control ack.
        let namespace_resolver = self.namespace_resolver.clone();
        let current_namespace = match tokio::time::timeout_at(
            resolve_deadline,
            tokio::task::spawn_blocking(move || namespace_resolver()),
        )
        .await
        {
            Ok(Ok(ns)) => ns,
            Ok(Err(e)) => {
                warn!(error = %e, "router federation: namespace resolve task panicked; will retry");
                return FederationOutcome::Failed(format!("namespace resolve task panicked: {e}"));
            }
            Err(_) => {
                warn!("router federation: namespace resolve timed out; will retry");
                return FederationOutcome::Failed("namespace resolve timed out".to_string());
            }
        };
        if current_namespace != self.startup_namespace {
            info!(
                from = %self.startup_namespace,
                to = %current_namespace,
                "router federation: organization namespace changed; requesting a daemon restart \
                 (a namespace change cannot be applied to a live session)"
            );
            return FederationOutcome::Restart;
        }

        let desired_backend = resolved.backend_endpoint();
        let desired_peers = resolved.peer_endpoints();
        let unchanged = desired_backend == applied.backend && desired_peers == applied.peers;

        // Apply the full backend plus peer union, or replay the cached outcome when
        // the endpoint-keyed set is unchanged.
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
            match tokio::time::timeout_at(
                apply_deadline,
                (self.federator)(resolved.connect_locators()),
            )
            .await
            {
                Err(_elapsed) => {
                    warn!(
                        "router federation: applying the upstream change timed out, so federation \
                         with the per-user cloud router on platform-backend is NOT in effect; will \
                         retry"
                    );
                    return FederationOutcome::Failed("apply timed out".to_string());
                }
                Ok(Ok(true)) => {
                    info!(
                        backend = ?desired_backend,
                        peers = ?desired_peers,
                        "router federation: applied desired backend and user-peer links"
                    );
                    *applied = AppliedState {
                        backend: desired_backend.clone(),
                        peers: desired_peers.clone(),
                        pinned: false,
                        needs_reapply: false,
                    };
                }
                Ok(Ok(false)) => {
                    // A managed router with a pinned `ZENOH_CONFIG` cannot be changed
                    // here. Advance `applied` (endpoint *and* the pinned bit) so this
                    // is noted once per change (login/logout) rather than every poll,
                    // and so an identical repeat replays `Pinned`; warn so the
                    // operator knows federation is not being auto-managed.
                    warn!(
                        "router federation: the managed router uses an operator-pinned \
                         ZENOH_CONFIG; the desired federation change was not applied"
                    );
                    *applied = AppliedState {
                        backend: desired_backend,
                        peers: desired_peers,
                        pinned: true,
                        needs_reapply: false,
                    };
                    return FederationOutcome::Pinned;
                }
                Ok(Err(e)) => {
                    // Leave `applied` unchanged so the next poll retries the apply.
                    warn!(
                        error = %e,
                        "router federation: failed to apply the upstream change, so federation with \
                         the per-user cloud router on platform-backend is NOT in effect; will retry"
                    );
                    return FederationOutcome::Failed(e.to_string());
                }
            }
        }

        let mut peer_errors = std::collections::BTreeMap::new();
        let mut backend_error = None;
        if verify {
            // Every probe is individually bounded and all probes run concurrently,
            // keeping total verification inside the control-socket ack budget.
            let mut probes = JoinSet::new();
            if let Some(backend) = &resolved.backend {
                self.spawn_probe(
                    &mut probes,
                    ProbeTarget::Backend,
                    &backend.endpoint,
                    backend.tls.clone(),
                );
            }
            for peer in &resolved.peers {
                self.spawn_probe(
                    &mut probes,
                    ProbeTarget::Peer(peer.endpoint.as_str().to_string()),
                    &peer.endpoint,
                    resolved.peer_probe_tls.clone(),
                );
            }

            while let Some(joined) = probes.join_next().await {
                match joined {
                    Ok((ProbeTarget::Backend, Err(reason))) => backend_error = Some(reason),
                    Ok((ProbeTarget::Peer(endpoint), Err(reason))) => {
                        peer_errors.insert(endpoint, reason);
                    }
                    Ok((_, Ok(()))) => {}
                    Err(error) => {
                        return FederationOutcome::Failed(format!(
                            "federation probe task failed: {error}"
                        ));
                    }
                }
            }
        }

        let peer_failed = !peer_errors.is_empty();
        let mut peers: Vec<PeerReport> = resolved
            .peers
            .iter()
            .map(|peer| PeerReport {
                endpoint: peer.endpoint.as_str().to_string(),
                state: if !verify {
                    PeerLinkState::Unverified
                } else {
                    match peer_errors.remove(peer.endpoint.as_str()) {
                        Some(reason) => PeerLinkState::Error(reason),
                        None => PeerLinkState::Verified,
                    }
                },
            })
            .collect();
        peers.sort_by(|left, right| left.endpoint.cmp(&right.endpoint));
        applied.needs_reapply = backend_error.is_some() || peer_failed;
        let applied_report = AppliedFederation {
            backend: resolved.backend_endpoint(),
            peers,
        };
        if let Some(reason) = backend_error {
            warn!(
                reason = %reason,
                "router federation: platform-backend link did not validate"
            );
            return FederationOutcome::Unreachable {
                reason,
                applied: applied_report,
            };
        }
        FederationOutcome::Applied(applied_report)
    }

    /// Starts one bounded reachability probe against a federation endpoint.
    fn spawn_probe(
        &self,
        probes: &mut JoinSet<(ProbeTarget, std::result::Result<(), String>)>,
        target: ProbeTarget,
        endpoint: &ParsedEndpointBuf,
        tls: pmi::TlsConfig,
    ) {
        let prober = self.prober.clone();
        let host = endpoint.host().to_string();
        let port = endpoint.port();
        probes.spawn(async move {
            let result = probe_with_bound(prober, host, port, tls, PROBE_TIMEOUT).await;
            (target, result)
        });
    }
}

/// Re-renders the local router's config with the (possibly empty) upstream and,
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
    connect_endpoints: Vec<String>,
    extra_listen_endpoints: Vec<String>,
) -> Result<bool> {
    let mut messenger = messenger.lock().await;
    let rewrote = messenger
        .refederate(RouterLinks {
            connect_endpoints,
            extra_listen_endpoints,
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
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

    const ENDPOINT: &str = "tls/cap.zenoh.localhost:7443";

    /// A poll engine with injected seams and test defaults. Tests override
    /// fields (timeouts, namespace resolver) before calling `poll_and_apply`.
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
            startup_namespace: "local".to_string(),
            namespace_resolver: local_ns_resolver(),
        }
    }

    /// A `RouterFederation` with injected seams, no links, and test defaults.
    /// Tests override fields (gates, restart signal, poller bounds) before
    /// calling `manage`.
    fn federation_under_test(
        federator: Federator,
        resolver: Resolver,
        prober: Prober,
        messaging_ready: watch::Receiver<bool>,
        trigger_rx: TriggerReceiver,
    ) -> RouterFederation {
        RouterFederation {
            poller: poller_under_test(federator, resolver, prober),
            messaging_ready,
            trigger_rx,
            restart_tx: watch::channel(false).0,
            presence_gate_tx: None,
            teardown_token: CancellationToken::new(),
            initial_peers: Vec::new(),
            listen_endpoint: None,
            initial_pinned: false,
        }
    }

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

    /// A federator that never completes, simulating a wedged apply (e.g. the
    /// messenger lock held forever by a stuck watchdog restart).
    fn wedged_federator() -> Federator {
        Arc::new(|_target| -> FederateFuture { Box::pin(std::future::pending()) })
    }

    /// A resolver returning a fixed value and counting its calls.
    fn counting_resolver(value: Option<DesiredBackend>) -> (Resolver, Arc<AtomicUsize>) {
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = calls.clone();
        let resolver: Resolver = Arc::new(move || {
            counter.fetch_add(1, Ordering::SeqCst);
            Ok(DesiredFederation {
                backend: value.clone(),
                ..DesiredFederation::default()
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

    fn dial(endpoint: &str) -> ParsedEndpointBuf {
        ParsedEndpointBuf::parse(endpoint, "tls", EndpointPurpose::Dial).unwrap()
    }

    fn upstream() -> Option<DesiredBackend> {
        Some(DesiredBackend {
            endpoint: dial(ENDPOINT),
            locator: ENDPOINT.to_string(),
            tls: pmi::TlsConfig::default(),
        })
    }

    fn peer(endpoint: &str) -> PeerLink {
        PeerLink {
            endpoint: dial(endpoint),
            locator: endpoint.to_string(),
        }
    }

    fn fixed_resolver(desired: DesiredFederation) -> Resolver {
        Arc::new(move || Ok(desired.clone()))
    }

    fn recording_federator() -> (Federator, Arc<std::sync::Mutex<Vec<Vec<String>>>>) {
        let calls = Arc::new(std::sync::Mutex::new(Vec::new()));
        let recorded = calls.clone();
        let federator: Federator = Arc::new(move |endpoints| {
            recorded.lock().unwrap().push(endpoints);
            Box::pin(async { Ok(true) })
        });
        (federator, calls)
    }

    /// A namespace resolver that always returns `local`, matching the `local`
    /// startup namespace these tests pass, so no namespace change is detected and
    /// the existing federation behavior (Applied/Pinned/...) is exercised.
    fn local_ns_resolver() -> NamespaceResolver {
        Arc::new(|| "local".to_string())
    }

    #[tokio::test]
    async fn daemon_enforces_the_probe_bound_for_a_noncompliant_prober() {
        let prober: Prober =
            Arc::new(|_host, _port, _tls, _timeout| Box::pin(std::future::pending()));
        let started = tokio::time::Instant::now();
        let error = probe_with_bound(
            prober,
            "peer.example".to_string(),
            7449,
            pmi::TlsConfig::default(),
            Duration::from_millis(50),
        )
        .await
        .expect_err("the daemon-side deadline must end a stuck probe");

        assert!(error.contains("timed out"));
        assert!(started.elapsed() < Duration::from_millis(500));
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

    #[tokio::test]
    async fn peers_only_apply_renders_the_peer_connect_set() {
        let endpoint = "tls/peer-a.example:7449";
        let resolver = fixed_resolver(DesiredFederation {
            backend: None,
            peers: vec![peer(endpoint)],
            ..DesiredFederation::default()
        });
        let (federator, calls) = recording_federator();
        let (prober, probe_calls) = counting_prober(Ok(()));
        let mut applied = AppliedState::default();

        let outcome = poller_under_test(federator, resolver, prober)
            .poll_and_apply(&mut applied, false)
            .await;

        assert_eq!(
            outcome,
            FederationOutcome::Applied(AppliedFederation {
                backend: None,
                peers: vec![PeerReport {
                    endpoint: endpoint.to_string(),
                    state: PeerLinkState::Unverified,
                }],
            })
        );
        assert_eq!(*calls.lock().unwrap(), vec![vec![endpoint.to_string()]]);
        assert_eq!(probe_calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn backend_and_peers_are_applied_as_one_union() {
        let peer_endpoint = "tls/peer-a.example:7449";
        let resolver = fixed_resolver(DesiredFederation {
            backend: upstream(),
            peers: vec![peer(peer_endpoint)],
            ..DesiredFederation::default()
        });
        let (federator, calls) = recording_federator();
        let (prober, _) = counting_prober(Ok(()));
        let mut applied = AppliedState::default();

        let outcome = poller_under_test(federator, resolver, prober)
            .poll_and_apply(&mut applied, false)
            .await;

        assert!(matches!(outcome, FederationOutcome::Applied(_)));
        assert_eq!(
            *calls.lock().unwrap(),
            vec![vec![ENDPOINT.to_string(), peer_endpoint.to_string()]]
        );
        assert_eq!(applied.backend.as_deref(), Some(ENDPOINT));
        assert_eq!(applied.peers, vec![peer_endpoint]);
    }

    #[tokio::test]
    async fn peers_seeded_into_spawn_config_do_not_bounce_on_first_poll() {
        let endpoint = "tls/peer-a.example:7449";
        let resolver = fixed_resolver(DesiredFederation {
            backend: None,
            peers: vec![peer(endpoint)],
            ..DesiredFederation::default()
        });
        let (federator, calls) = recording_federator();
        let (prober, _) = counting_prober(Ok(()));
        let mut applied = AppliedState {
            backend: None,
            peers: vec![endpoint.to_string()],
            pinned: false,
            needs_reapply: false,
        };

        let outcome = poller_under_test(federator, resolver, prober)
            .poll_and_apply(&mut applied, false)
            .await;

        let FederationOutcome::Applied(report) = outcome else {
            panic!("seeded peers must remain applied without a startup bounce");
        };
        assert_eq!(report.peers[0].state, PeerLinkState::Unverified);
        assert!(calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn seeded_state_from_a_pinned_router_is_never_reported_as_applied() {
        let endpoint = "tls/peer-a.example:7449";
        let resolver = fixed_resolver(DesiredFederation {
            backend: None,
            peers: vec![peer(endpoint)],
            ..DesiredFederation::default()
        });
        let (federator, calls) = recording_federator();
        let (prober, probe_calls) = counting_prober(Ok(()));
        let mut applied = AppliedState {
            backend: None,
            peers: vec![endpoint.to_string()],
            pinned: true,
            needs_reapply: false,
        };

        let outcome = poller_under_test(federator, resolver, prober)
            .poll_and_apply(&mut applied, true)
            .await;

        assert_eq!(outcome, FederationOutcome::Pinned);
        assert!(calls.lock().unwrap().is_empty());
        assert_eq!(probe_calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn pinned_router_does_not_claim_the_configured_extra_listener() {
        let links = FederationLinksSpec {
            extra_listen_endpoints: vec!["tls/0.0.0.0:7449#enable_mtls=true".to_string()],
            identity: IdentityPaths {
                cert: "cert.pem".into(),
                key: "key.pem".into(),
                ca: "ca.pem".into(),
            },
            initial_peers: Vec::new(),
            listen_endpoint: Some("tls/0.0.0.0:7449".to_string()),
            initial_pinned: true,
        };

        assert_eq!(links.status_listen_endpoint(), None);
    }

    #[tokio::test]
    async fn a_peer_probe_failure_is_reported_without_backend_unreachable() {
        let good = "tls/good.example:7449";
        let bad = "tls/bad.example:7449";
        let resolver = fixed_resolver(DesiredFederation {
            backend: upstream(),
            peers: vec![peer(good), peer(bad)],
            ..DesiredFederation::default()
        });
        let (federator, _) = recording_federator();
        let prober: Prober = Arc::new(|host, _port, _tls, _timeout| {
            Box::pin(async move {
                if host == "bad.example" {
                    Err("UnknownIssuer".to_string())
                } else {
                    Ok(())
                }
            })
        });
        let mut applied = AppliedState::default();

        let outcome = poller_under_test(federator, resolver, prober)
            .poll_and_apply(&mut applied, true)
            .await;

        let FederationOutcome::Applied(report) = outcome else {
            panic!("a peer-only probe error must keep the applied backend outcome");
        };
        assert_eq!(report.backend.as_deref(), Some(ENDPOINT));
        assert_eq!(report.peers.len(), 2);
        assert_eq!(
            report
                .peers
                .iter()
                .find(|report| report.endpoint == bad)
                .map(|report| &report.state),
            Some(&PeerLinkState::Error("UnknownIssuer".to_string()))
        );
        assert!(
            report
                .peers
                .iter()
                .find(|report| report.endpoint == good)
                .is_some_and(|report| report.state == PeerLinkState::Verified)
        );
    }

    #[tokio::test]
    async fn backend_and_peer_probes_run_concurrently() {
        let resolver = fixed_resolver(DesiredFederation {
            backend: upstream(),
            peers: vec![
                peer("tls/peer-a.example:7449"),
                peer("tls/peer-b.example:7449"),
            ],
            ..DesiredFederation::default()
        });
        let (federator, _) = recording_federator();
        let barrier = Arc::new(tokio::sync::Barrier::new(3));
        let in_flight = Arc::new(AtomicUsize::new(0));
        let max_in_flight = Arc::new(AtomicUsize::new(0));
        let prober: Prober = Arc::new({
            let barrier = barrier.clone();
            let in_flight = in_flight.clone();
            let max_in_flight = max_in_flight.clone();
            move |_host, _port, _tls, _timeout| {
                let barrier = barrier.clone();
                let in_flight = in_flight.clone();
                let max_in_flight = max_in_flight.clone();
                Box::pin(async move {
                    let running = in_flight.fetch_add(1, Ordering::SeqCst) + 1;
                    max_in_flight.fetch_max(running, Ordering::SeqCst);
                    barrier.wait().await;
                    in_flight.fetch_sub(1, Ordering::SeqCst);
                    Ok(())
                })
            }
        });
        let mut applied = AppliedState::default();

        let poller = poller_under_test(federator, resolver, prober);
        let outcome = tokio::time::timeout(
            Duration::from_secs(1),
            poller.poll_and_apply(&mut applied, true),
        )
        .await
        .expect("both probes must enter before either can finish");

        assert!(matches!(outcome, FederationOutcome::Applied(_)));
        assert_eq!(max_in_flight.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn rerun_after_probe_failure_reloads_unchanged_certificate_paths() {
        let endpoint = "tls/peer-a.example:7449";
        let resolver = fixed_resolver(DesiredFederation {
            backend: None,
            peers: vec![peer(endpoint)],
            ..DesiredFederation::default()
        });
        let (federator, applies) = recording_federator();
        let probe_calls = Arc::new(AtomicUsize::new(0));
        let probe_counter = probe_calls.clone();
        let prober: Prober = Arc::new(move |_host, _port, _tls, _timeout| {
            let fails = probe_counter.fetch_add(1, Ordering::SeqCst) == 0;
            Box::pin(async move {
                if fails {
                    Err("UnknownIssuer".to_string())
                } else {
                    Ok(())
                }
            })
        });
        let mut applied = AppliedState::default();
        let poller = poller_under_test(federator, resolver, prober);

        let first = poller.poll_and_apply(&mut applied, true).await;
        let FederationOutcome::Applied(first) = first else {
            panic!("peer probe errors stay in the applied peer report");
        };
        assert_eq!(
            first.peers[0].state,
            PeerLinkState::Error("UnknownIssuer".to_string())
        );

        let second = poller.poll_and_apply(&mut applied, true).await;
        let FederationOutcome::Applied(second) = second else {
            panic!("the corrected peer must verify on retry");
        };
        assert_eq!(second.peers[0].state, PeerLinkState::Verified);
        assert_eq!(
            applies.lock().unwrap().len(),
            2,
            "the retry must bounce zenohd so replaced certificate files are reloaded"
        );
    }

    #[tokio::test]
    async fn registry_read_failure_is_failed_and_never_drops_applied_links() {
        let resolver: Resolver = Arc::new(|| Err("malformed federations.json5".to_string()));
        let (federator, calls) = recording_federator();
        let (prober, _) = counting_prober(Ok(()));
        let mut applied = AppliedState {
            backend: None,
            peers: vec!["tls/existing.example:7449".to_string()],
            pinned: false,
            needs_reapply: false,
        };

        let outcome = poller_under_test(federator, resolver, prober)
            .poll_and_apply(&mut applied, true)
            .await;

        assert_eq!(
            outcome,
            FederationOutcome::Failed("malformed federations.json5".to_string())
        );
        assert_eq!(applied.peers, vec!["tls/existing.example:7449"]);
        assert!(calls.lock().unwrap().is_empty());
    }

    /// A wedged apply (the federator never completes) must surface as `Failed`
    /// within the apply-timeout bound instead of hanging the poll, which at
    /// startup would keep the readiness gate closed indefinitely. `applied`
    /// stays unchanged so the next poll retries, and the link is never probed
    /// (there is no applied upstream to verify).
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
            Ok(DesiredFederation {
                backend: upstream(),
                ..DesiredFederation::default()
            })
        });
        let federator: Federator = Arc::new(|_endpoints| {
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
    async fn ipv6_peer_probe_receives_an_unbracketed_host() {
        let endpoint = "tls/[2001:db8::1]:7449";
        let resolver = fixed_resolver(DesiredFederation {
            backend: None,
            peers: vec![peer(endpoint)],
            ..DesiredFederation::default()
        });
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
            Some(("2001:db8::1".to_string(), 7449))
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

        let mut federation = federation_under_test(
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
        trigger_tx
            .send(FederationRequest::Refederate { ack: ack_tx })
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
            FederationOutcome::Applied(AppliedFederation {
                backend: Some(ENDPOINT.to_string()),
                peers: Vec::new(),
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

    #[tokio::test]
    async fn cached_status_is_answered_while_a_refederation_is_slow() {
        let calls = Arc::new(AtomicUsize::new(0));
        let call_counter = calls.clone();
        let resolver: Resolver = Arc::new(move || {
            if call_counter.fetch_add(1, Ordering::SeqCst) > 0 {
                std::thread::sleep(Duration::from_millis(1500));
            }
            Ok(DesiredFederation::default())
        });
        let (prober, _) = counting_prober(Ok(()));
        let (messaging_tx, messaging_rx) = watch::channel(true);
        let (trigger_tx, trigger_rx) = mpsc::channel(8);
        let (ready_tx, ready_rx) = oneshot::channel();
        let mut federation = federation_under_test(
            applying_federator(),
            resolver,
            prober,
            messaging_rx,
            trigger_rx,
        );
        federation.poller.connect_timeout = Duration::from_secs(2);
        let task = tokio::spawn(federation.manage(ready_tx));
        ready_rx.await.unwrap();

        let (apply_tx, _apply_rx) = oneshot::channel();
        trigger_tx
            .send(FederationRequest::Refederate { ack: apply_tx })
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(1), async {
            while calls.load(Ordering::SeqCst) < 2 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the slow refederation started");

        let (status_tx, status_rx) = oneshot::channel();
        trigger_tx
            .send(FederationRequest::Status { ack: status_tx })
            .await
            .unwrap();
        let status = tokio::time::timeout(Duration::from_millis(500), status_rx)
            .await
            .expect("cached status must not queue behind refederation")
            .unwrap();
        assert!(status.backend.is_none());

        drop(messaging_tx);
        task.abort();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cached_status_has_an_independent_dispatch_task_during_blocking_apply() {
        let resolver_calls = Arc::new(AtomicUsize::new(0));
        let resolver_counter = resolver_calls.clone();
        let resolver: Resolver = Arc::new(move || {
            let backend = if resolver_counter.fetch_add(1, Ordering::SeqCst) == 0 {
                None
            } else {
                upstream()
            };
            Ok(DesiredFederation {
                backend,
                ..DesiredFederation::default()
            })
        });
        let apply_started = Arc::new(AtomicUsize::new(0));
        let started_flag = apply_started.clone();
        let blocking_federator: Federator = Arc::new(move |_endpoints| {
            let started_flag = started_flag.clone();
            Box::pin(async move {
                started_flag.store(1, Ordering::SeqCst);
                std::thread::sleep(Duration::from_millis(1500));
                Ok(true)
            })
        });
        let (prober, _) = counting_prober(Ok(()));
        let (messaging_tx, messaging_rx) = watch::channel(true);
        let (trigger_tx, trigger_rx) = mpsc::channel(8);
        let (ready_tx, ready_rx) = oneshot::channel();
        let mut federation = federation_under_test(
            blocking_federator,
            resolver,
            prober,
            messaging_rx,
            trigger_rx,
        );
        federation.poller.connect_timeout = Duration::from_secs(3);
        let task = tokio::spawn(federation.manage(ready_tx));
        ready_rx.await.unwrap();

        let (apply_tx, _apply_rx) = oneshot::channel();
        trigger_tx
            .send(FederationRequest::Refederate { ack: apply_tx })
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(1), async {
            while apply_started.load(Ordering::SeqCst) == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the blocking apply started");

        let (status_tx, status_rx) = oneshot::channel();
        trigger_tx
            .send(FederationRequest::Status { ack: status_tx })
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_millis(500), status_rx)
            .await
            .expect("status dispatch must run independently of router lifecycle work")
            .unwrap();

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

        let mut federation = federation_under_test(
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
        trigger_tx
            .send(FederationRequest::Refederate { ack: ack_tx })
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

        let mut federation = federation_under_test(
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
        trigger_tx
            .send(FederationRequest::Refederate { ack: ack_tx })
            .await
            .expect("trigger accepted");
        let outcome = tokio::time::timeout(Duration::from_secs(1), ack_rx)
            .await
            .expect("poke serviced immediately")
            .expect("ack sender not dropped");

        assert_eq!(
            outcome,
            FederationOutcome::Unreachable {
                reason: reason.to_string(),
                applied: AppliedFederation {
                    backend: Some(ENDPOINT.to_string()),
                    peers: Vec::new(),
                },
            },
            "a failing probe returns Unreachable with the applied peer report"
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

        let mut federation = federation_under_test(
            pinned_federator(),
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
        trigger_tx
            .send(FederationRequest::Refederate { ack: ack_tx })
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

    /// Once the local router is ready, the startup gate fires within the resolve
    /// timeout even when the backend is slow enough to blow the bound. The
    /// federation loop then keeps retrying. The core node's presence gate fires
    /// in the same breath, so a slow backend cannot stall the boot presence check
    /// (and thus listener binding) either.
    #[tokio::test]
    async fn startup_gate_fires_within_timeout_when_resolve_is_slow() {
        // Resolver sleeps past the (short) connect timeout, so the bounded resolve
        // elapses and the first poll completes as a failure; the gate must still
        // fire.
        let resolver: Resolver = Arc::new(|| {
            std::thread::sleep(Duration::from_millis(400));
            Ok(DesiredFederation::default())
        });
        let (prober, _) = counting_prober(Ok(()));
        let (messaging_tx, messaging_rx) = watch::channel(true);
        let (_trigger_tx, trigger_rx) = mpsc::channel(8);
        let (ready_tx, ready_rx) = oneshot::channel();
        let (presence_gate_tx, mut presence_gate_rx) = watch::channel(false);

        let mut federation = federation_under_test(
            applying_federator(),
            resolver,
            prober,
            messaging_rx,
            trigger_rx,
        );
        federation.poller.connect_timeout = Duration::from_millis(100);
        federation.presence_gate_tx = Some(presence_gate_tx);
        let task = tokio::spawn(federation.manage(ready_tx));

        // Gate fires close to the 100ms bound, well before the 400ms resolve.
        tokio::time::timeout(Duration::from_secs(1), ready_rx)
            .await
            .expect("gate fires within the timeout despite a slow backend")
            .expect("gate sender not dropped");
        // The presence gate fires in lockstep, so the core node boots (standalone)
        // rather than waiting on the hung backend.
        tokio::time::timeout(Duration::from_secs(1), presence_gate_rx.wait_for(|g| *g))
            .await
            .expect("presence gate fires within the timeout despite a slow backend")
            .expect("presence gate sender not dropped");

        drop(messaging_tx);
        task.abort();
    }

    /// The core node's presence gate opens only after the *initial* federation
    /// poll has settled (in lockstep with the startup gate), so the boot-time
    /// presence check runs against the federated mesh rather than the
    /// always-standalone just-started router.
    #[tokio::test]
    async fn presence_gate_fires_once_the_initial_federation_settled() {
        let (resolver, calls) = counting_resolver(upstream());
        let (prober, _) = counting_prober(Ok(()));
        let (messaging_tx, messaging_rx) = watch::channel(true);
        let (_trigger_tx, trigger_rx) = mpsc::channel(8);
        let (ready_tx, ready_rx) = oneshot::channel();
        let (presence_gate_tx, mut presence_gate_rx) = watch::channel(false);

        let mut federation = federation_under_test(
            applying_federator(),
            resolver,
            prober,
            messaging_rx,
            trigger_rx,
        );
        federation.poller.connect_timeout = Duration::from_secs(5);
        federation.presence_gate_tx = Some(presence_gate_tx);
        let task = tokio::spawn(federation.manage(ready_tx));

        tokio::time::timeout(Duration::from_secs(1), presence_gate_rx.wait_for(|g| *g))
            .await
            .expect("presence gate fires promptly")
            .expect("presence gate sender not dropped");
        // The generous timeout means the gate fired via the initial-poll path,
        // so the federation had already been resolved (and applied) when the
        // gate opened.
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "the initial federation poll settled before the presence gate opened"
        );
        // The startup gate fired in the same breath.
        tokio::time::timeout(Duration::from_secs(1), ready_rx)
            .await
            .expect("startup gate fires with the presence gate")
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

        let mut federation = federation_under_test(
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

        let mut federation = federation_under_test(
            applying_federator(),
            resolver,
            prober,
            messaging_rx,
            trigger_rx,
        );
        federation.poller.connect_timeout = Duration::from_secs(5);
        federation.poller.namespace_resolver = ns_resolver;
        federation.restart_tx = restart_tx;
        let task = tokio::spawn(federation.manage(ready_tx));

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
            .send(FederationRequest::Refederate { ack: ack_tx })
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

        let mut federation = federation_under_test(
            applying_federator(),
            resolver,
            prober,
            messaging_rx,
            trigger_rx,
        );
        federation.poller.connect_timeout = Duration::from_secs(5);
        federation.poller.namespace_resolver = ns_resolver;
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
            ns_calls.load(Ordering::SeqCst) >= 1,
            "the namespace was re-resolved to detect the drift"
        );

        drop(messaging_tx);
        task.abort();
    }
}
