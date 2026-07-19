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
//!   *verifies* the federation link by querying the managed router until its
//!   configured outbound link is established, so transport-level TLS or client
//!   authentication failures are reported as a [`LinkState::Error`] rather than
//!   a false success. The loop also publishes
//!   its cached [`FederationStatus`] to a watch channel; the control socket
//!   answers status queries straight from that watch, so they never queue
//!   behind an in-flight apply.
//! * **Scheduled maintenance.** The loop wakes at the earlier of the cached
//!   router-config deadline and the active certificate's jittered renewal
//!   deadline. Certificate rotation always reloads the managed router and waits
//!   for its actual outbound link; failures restore a still-valid previous generation and
//!   retry with bounded exponential backoff. This is maintenance, not a link
//!   keepalive: zenoh still owns ordinary reconnects and the backend actively
//!   probes the daemon's `/health` service.
//! * **Registration cadence.** Every config pull's POST carries this daemon's
//!   core-node name, upserting it into the backend's per-principal core-node
//!   registry. Pulls happen at startup/login and at the server-provided cache
//!   deadline. The backend's `last_seen_at` therefore means "last federation
//!   config pull", not liveness.
//! * **Live (re)federation.** When the resolved upstream changes (the user logs
//!   in, logs out, or the endpoint moves) the local router's zenohd config is
//!   re-rendered and the router restarted, so the change takes effect without a
//!   full daemon restart.

use crate::control::{FederationStatus, LinkState, PlatformLink};
use crate::error::{Error, Result};
use crate::serve::{ServeAsyncCommand, ServeAsyncHandle};
use config::namespace::Namespace;
use daemon_config::consts::PeppyDirs;
use daemon_config::peppy_config::ParsedEndpointBuf;
use pmi::{Messenger, RouterLinks, UpstreamLink};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{Mutex, mpsc, oneshot, watch};
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

/// How long a *verifying* login/logout poke waits for the managed router's
/// federation link to establish. Deliberately small and decoupled from
/// `connect_timeout` (the resolve bound), so a tight bound keeps
/// the whole verifying poll (resolve + zenohd bounce + probe) inside the daemon's
/// ack budget: `connect_timeout` + [`super::federation_control`]'s
/// `APPLY_ACK_SLACK`, which is sized to cover apply plus verification. An
/// unreachable / firewalled router fails verification within this bound and surfaces promptly as
/// a [`LinkState::Error`] rather than as a daemon-side ack timeout.
pub(crate) const PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// Post-resolve budget for rewriting the managed router and waiting for zenohd
/// to accept connections again. Kept separate from the backend resolve timeout.
pub(crate) const APPLY_TIMEOUT: Duration = Duration::from_secs(4);

/// Within one bounded router apply, wait for the daemon's retained Zenoh
/// session to observe the reload and replay its declarations. This prevents a
/// logout immediately after certificate rotation from dropping a presence
/// token that has not yet rebound to the replacement router.
const SESSION_RECONNECT_TIMEOUT: Duration = Duration::from_secs(3);

/// Failed resolves or rewrites are retried independently of scheduled router
/// and certificate maintenance. Zenohd still owns link reconnection.
const RETRY_DELAY: Duration = Duration::from_secs(5);

const RENEWAL_RETRY_BASE: Duration = Duration::from_secs(30);
const RENEWAL_RETRY_MAX: Duration = Duration::from_secs(30 * 60);

/// At most one refederation waits behind the poll currently in progress; a
/// second concurrent poke is rejected as busy by the control handler. Status
/// queries never enter this queue: they are answered from the status watch.
const REFEDERATE_QUEUE_CAPACITY: usize = 1;

/// The platform federation target: the parsed dial endpoint plus the
/// connect-side mTLS material for that link. Typed end to end: change
/// detection compares this pair, and pmi renders the locator (endpoint plus
/// TLS fragments) at apply time.
#[derive(Debug, Clone, PartialEq, Eq)]
struct DesiredBackend {
    endpoint: ParsedEndpointBuf,
    tls: pmi::TlsConfig,
}

impl DesiredBackend {
    /// The upstream link pmi applies for this target.
    fn upstream_link(&self) -> UpstreamLink {
        UpstreamLink {
            endpoint: self.endpoint.as_str().to_string(),
            tls: self.tls.clone(),
        }
    }
}

/// What one poll's resolve produced: the desired platform upstream (`None` is
/// logged out / standalone) plus the namespace the same credentials resolve
/// to, compared against the generation's startup namespace to detect a
/// namespace change.
struct Resolved {
    upstream: Option<DesiredBackend>,
    namespace: Namespace,
    rotation: Option<auth::IdentityRotation>,
    maintenance_after: Option<Duration>,
    certificate_expires_after: Option<Duration>,
    renewal_error: Option<String>,
    resolve_error: Option<String>,
    pat_active: bool,
}

/// Resolves the desired platform upstream and namespace from the credentials.
type Resolver = Arc<dyn Fn() -> std::result::Result<Resolved, String> + Send + Sync>;

/// Owns a blocking resolver after its caller-facing deadline. A late successful
/// resolve may already have atomically published a certificate generation, so
/// explicitly reject its receipt instead of merely detaching and forgetting the
/// task. If this future itself is cancelled during runtime shutdown, dropping
/// the eventual `Resolved` still invokes `IdentityRotation`'s armed guard.
async fn cleanup_late_resolve(
    resolve_task: tokio::task::JoinHandle<std::result::Result<Resolved, String>>,
) {
    match resolve_task.await {
        Ok(Ok(mut late)) => {
            if let Some(rotation) = late.rotation.take()
                && let Err(error) = rotation.rollback()
            {
                warn!(
                    error = %error,
                    "router federation: timed-out resolve later activated an identity and rollback failed"
                );
            }
        }
        Ok(Err(error)) => {
            warn!(
                error = %error,
                "router federation: timed-out resolve later failed"
            );
        }
        Err(error) => {
            warn!(
                error = %error,
                "router federation: timed-out resolve task later panicked"
            );
        }
    }
}

/// The future a [`Prober`] returns: `Ok(())` if the managed router reports its
/// configured upstream link established, `Err(reason)` (human-readable) otherwise.
type ProbeFuture = Pin<Box<dyn Future<Output = std::result::Result<(), String>> + Send>>;

/// Verifies that the federation link to `host:port` is actually established by
/// the managed router. A boxed async closure so tests can inject a deterministic
/// probe (success/failure + a call counter) in place of the real admin-space
/// query, which does network I/O.
type Prober = Arc<dyn Fn(String, u16, pmi::TlsConfig, Duration) -> ProbeFuture + Send + Sync>;

/// The real prober: wait for the managed zenohd instance to report its
/// configured outbound link established. This observes the same TLS stack and
/// client identity used by the data plane instead of approximating it with a
/// separate raw TLS client.
fn real_prober(messenger: Arc<Mutex<Messenger>>) -> Prober {
    Arc::new(move |host, port, _tls, timeout| -> ProbeFuture {
        let messenger = messenger.clone();
        Box::pin(async move {
            let probe = {
                let messenger = messenger.lock().await;
                messenger.router_links_probe()
            }
            .ok_or_else(|| {
                format!(
                    "managed zenohd exposes no configured link to {host}:{port}; federation cannot be verified"
                )
            })?;

            if probe.wait_established(timeout).await {
                Ok(())
            } else {
                Err(format!(
                    "managed zenohd did not establish its configured link to {host}:{port} within {timeout:?}"
                ))
            }
        })
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
type Federator = Arc<dyn Fn(Option<UpstreamLink>) -> FederateFuture + Send + Sync>;

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

/// One control request delivered to the federation loop. Ordinary login/logout
/// asks for a full re-resolve (including namespace-change detection). Only the
/// fail-closed recovery path asks for unconditional standalone, so retained
/// credentials or an older same-subject certificate cannot be reused.
pub(crate) enum FederationTrigger {
    Refederate(oneshot::Sender<FederationOutcome>),
    Defederate(oneshot::Sender<FederationOutcome>),
}

/// Sends federation control requests (each carrying its ack) to the loop held
/// by [`FederationControl`](super::federation_control).
pub(crate) type TriggerSender = mpsc::Sender<FederationTrigger>;
/// Receives federation control requests in the federation loop.
pub(crate) type TriggerReceiver = mpsc::Receiver<FederationTrigger>;

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
    /// Real daemon status publisher for observable in-progress renewal state;
    /// omitted by pure poller unit tests.
    status_tx: Option<watch::Sender<FederationStatus>>,
    /// Cleanup for a resolver that crossed its caller-facing deadline. A new
    /// poll cannot start another identity mutation until this task has consumed
    /// the late result and rejected any unverified rotation it carried.
    late_resolve_cleanup: Mutex<Option<tokio::task::JoinHandle<()>>>,
    /// Test-only stand-in for slow blocking receipt validation/fsync/pruning in
    /// `IdentityRotation::commit_after_probe`, placed in the same deadline gap.
    #[cfg(test)]
    finalization_delay: Duration,
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
    /// Captured before the first poll so a resolver timeout/error cannot
    /// overwrite a true service-environment PAT status with `false`.
    initial_pat_active: bool,
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
            Ok(Resolved {
                upstream: resolved
                    .upstream
                    .map(|(endpoint, tls)| DesiredBackend { endpoint, tls }),
                namespace: resolved.namespace,
                rotation: resolved.rotation,
                maintenance_after: resolved.maintenance_after,
                certificate_expires_after: resolved.certificate_expires_after,
                renewal_error: resolved.renewal_error,
                resolve_error: resolved.resolve_error,
                pat_active: resolved.pat_active,
            })
        });
        let initial_pat_active = auth::resolver::pat_from_env().is_some();
        let (status_tx, status_rx) = watch::channel(FederationStatus {
            link: PlatformLink::default(),
            pinned: initial_pinned,
            pat_active: initial_pat_active,
            certificate_error: None,
            certificate_renewing: false,
        });
        let federation = Self {
            poller: FederationPoller {
                federator: real_federator(messenger.clone()),
                resolver,
                prober: real_prober(messenger),
                connect_timeout,
                apply_timeout: APPLY_TIMEOUT,
                startup_namespace,
                status_tx: Some(status_tx.clone()),
                late_resolve_cleanup: Mutex::new(None),
                #[cfg(test)]
                finalization_delay: Duration::ZERO,
            },
            messaging_ready,
            trigger_rx,
            status_tx,
            restart_tx,
            presence_gate_tx,
            teardown_token,
            initial_pinned,
            initial_pat_active,
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
/// instead of bouncing the router again. Keyed on the endpoint plus its TLS
/// material, so a changed certificate path re-applies even when the endpoint
/// is unchanged.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
enum SettledDesired {
    /// No apply attempt has settled yet (a pinned start), so the first poll
    /// always attempts an apply.
    Unsettled,
    /// Settled with no upstream: the router is standalone by decision.
    #[default]
    Standalone,
    /// Settled with this upstream target.
    Upstream(DesiredBackend),
}

impl SettledDesired {
    /// The settled form of a completed apply attempt for `desired`.
    fn from_completed(desired: Option<DesiredBackend>) -> Self {
        match desired {
            None => Self::Standalone,
            Some(backend) => Self::Upstream(backend),
        }
    }

    /// Whether `desired` matches what last settled. An unsettled state matches
    /// nothing, so the first poll after a pinned start always applies.
    fn matches(&self, desired: &Option<DesiredBackend>) -> bool {
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
    /// Next server/config or certificate maintenance wake after the last
    /// resolve. `None` allows a genuinely logged-out loop to remain idle.
    next_maintenance: Option<Duration>,
    /// Independent monotonic hard deadline for the currently applied client
    /// certificate. It survives resolver errors/timeouts so transient control-
    /// plane failure can never keep an upstream configured past leaf expiry.
    certificate_expires_at: Option<tokio::time::Instant>,
    /// The certificate expired, but the bounded attempt to render standalone
    /// did not settle. While this is set, the elapsed hard deadline must not
    /// force a zero-delay timer loop; retry the fail-closed apply on backoff.
    expiry_defederation_pending: bool,
    /// Consecutive certificate-maintenance failures, used for exponential
    /// retry backoff while the previous generation remains valid.
    renewal_failures: u32,
    /// Non-secret auth-source signal used by logout to detect a PAT present in
    /// the daemon service environment even when the invoking shell lacks it.
    pat_active: bool,
    /// Latest safe renewal/rebinding failure exposed through daemon status.
    certificate_error: Option<String>,
    certificate_renewing: bool,
}

impl AppliedState {
    fn platform_link(&self) -> PlatformLink {
        PlatformLink {
            endpoint: self.endpoint.clone(),
            link_state: self.link_state.clone(),
        }
    }

    fn renewal_retry_delay(&self) -> Duration {
        if self.renewal_failures == 0 {
            return RENEWAL_RETRY_BASE;
        }
        let exponent = self.renewal_failures.saturating_sub(1).min(6);
        let base_secs = RENEWAL_RETRY_BASE
            .as_secs()
            .saturating_mul(1_u64 << exponent)
            .min(RENEWAL_RETRY_MAX.as_secs());
        // A small deterministic jitter prevents exact retry harmonics while
        // retaining reproducible tests. Initial renewal scheduling is already
        // distributed by the per-generation hash in auth::router.
        let jitter_span = (base_secs / 5).max(1);
        let jitter = (u64::from(self.renewal_failures).wrapping_mul(37)) % jitter_span;
        Duration::from_secs(
            base_secs
                .saturating_add(jitter)
                .min(RENEWAL_RETRY_MAX.as_secs()),
        )
    }

    fn timer_after(&self, retry_pending: bool) -> Option<Duration> {
        if self.expiry_defederation_pending {
            return Some(self.renewal_retry_delay());
        }
        let retry = if self.renewal_failures > 0 {
            let mut retry = self.renewal_retry_delay();
            if let Some(hard_deadline) = self.next_maintenance {
                // Increase urgency near certificate expiry: retry no later
                // than halfway through the remaining hard window, with a
                // one-second floor to avoid a busy loop.
                let half_remaining = Duration::from_secs((hard_deadline.as_secs() / 2).max(1));
                retry = retry.min(half_remaining);
            }
            Some(retry)
        } else if retry_pending {
            Some(RETRY_DELAY)
        } else {
            None
        };
        let scheduled = match (self.next_maintenance, retry) {
            (Some(maintenance), Some(retry)) => Some(maintenance.min(retry)),
            (Some(maintenance), None) => Some(maintenance),
            (None, Some(retry)) => Some(retry),
            (None, None) => None,
        };
        let expiry = self
            .certificate_expires_at
            .map(|deadline| deadline.saturating_duration_since(tokio::time::Instant::now()));
        match (scheduled, expiry) {
            (Some(scheduled), Some(expiry)) => Some(scheduled.min(expiry)),
            (Some(scheduled), None) => Some(scheduled),
            (None, Some(expiry)) => Some(expiry),
            (None, None) => None,
        }
    }

    fn certificate_expired(&self) -> bool {
        self.certificate_expires_at
            .is_some_and(|deadline| tokio::time::Instant::now() >= deadline)
    }
}

impl RouterFederation {
    /// Waits for the router to come up, runs the initial federation (firing the
    /// startup gate when it completes or the timeout elapses), then services
    /// immediate login/logout pokes and scheduled certificate/config
    /// maintenance for the daemon's lifetime (the caller races it against the
    /// shutdown signal). This is not a periodic keepalive: the wakeup follows
    /// server deadlines and zenoh owns ordinary reconnects.
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
            initial_pat_active,
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
            pat_active: initial_pat_active,
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
        // initial poll does not verify (`verify = false`): startup must not block on
        // outbound-link establishment, and the verifying check belongs to the login poke.
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

        // Phase 3, steady state: react to immediate CLI pokes, retry failures,
        // and wake at the next router/certificate maintenance deadline. This is
        // not a link keepalive: after maintenance succeeds, the next timer is
        // derived from server state and zenohd owns ordinary reconnection.
        enum Work {
            Poke(FederationTrigger),
            Timer,
        }
        let mut retry_pending = matches!(&initial_outcome, FederationOutcome::Failed(_));
        loop {
            let timer = applied.timer_after(retry_pending);
            let work = if let Some(delay) = timer {
                tokio::select! {
                    request = trigger_rx.recv() => match request {
                        Some(ack) => Work::Poke(ack),
                        None => break,
                    },
                    _ = tokio::time::sleep(delay) => Work::Timer,
                }
            } else {
                let Some(ack) = trigger_rx.recv().await else {
                    break;
                };
                Work::Poke(ack)
            };
            let timer_work = matches!(&work, Work::Timer);
            let (outcome, ack) = match work {
                Work::Poke(FederationTrigger::Refederate(ack)) => (
                    self.poller.poll_and_apply(&mut applied, true).await,
                    Some(ack),
                ),
                Work::Poke(FederationTrigger::Defederate(ack)) => {
                    (self.poller.force_standalone(&mut applied).await, Some(ack))
                }
                Work::Timer => (self.poller.poll_and_apply(&mut applied, false).await, None),
            };
            self.publish_status(&applied);
            retry_pending = matches!(&outcome, FederationOutcome::Failed(_));
            if let Some(ack) = ack {
                // The CLI may have already given up (read timeout); ignore. On
                // a namespace change the control handler raises the restart
                // after attempting to flush `Restarting`.
                let _ = ack.send(outcome);
            } else if timer_work && matches!(&outcome, FederationOutcome::Restart { .. }) {
                let _ = self.restart_tx.send(true);
                return;
            }
        }
    }

    /// Publishes the cached status the control socket answers status queries
    /// from: the platform link now in effect plus router-config ownership.
    fn publish_status(&self, applied: &AppliedState) {
        self.status_tx.send_replace(FederationStatus {
            link: applied.platform_link(),
            pinned: applied.pinned,
            pat_active: applied.pat_active,
            certificate_error: applied.certificate_error.clone(),
            certificate_renewing: applied.certificate_renewing,
        });
    }
}

impl FederationPoller {
    /// Applies intentional standalone without consulting credentials or the
    /// identity resolver. This is distinct from a normal refederation poke:
    /// post-login failure deliberately retains OAuth/PAT state for retry, and
    /// re-resolving it could otherwise reuse a same-subject prior certificate.
    async fn force_standalone(&self, applied: &mut AppliedState) -> FederationOutcome {
        match tokio::time::timeout(self.apply_timeout, (self.federator)(None)).await {
            Ok(Ok(true)) => {
                applied.endpoint = None;
                applied.link_state = LinkState::NotConfigured;
                applied.last_settled_desired = SettledDesired::Standalone;
                applied.pinned = false;
                applied.needs_reapply = false;
                applied.next_maintenance = None;
                applied.certificate_expires_at = None;
                applied.expiry_defederation_pending = false;
                applied.renewal_failures = 0;
                applied.certificate_error = None;
                applied.certificate_renewing = false;
                FederationOutcome::Applied(applied.platform_link())
            }
            Ok(Ok(false)) => {
                applied.last_settled_desired = SettledDesired::Unsettled;
                applied.pinned = true;
                applied.needs_reapply = true;
                FederationOutcome::Pinned
            }
            Ok(Err(error)) => {
                applied.last_settled_desired = SettledDesired::Unsettled;
                applied.needs_reapply = true;
                FederationOutcome::Failed(format!("forced standalone router apply failed: {error}"))
            }
            Err(_) => {
                applied.last_settled_desired = SettledDesired::Unsettled;
                applied.needs_reapply = true;
                FederationOutcome::Failed("forced standalone router apply timed out".into())
            }
        }
    }

    /// One poll: resolve the desired upstream and, if it changed, (re)federate the
    /// local router. Updates `*applied` to the upstream now in effect and returns the
    /// [`FederationOutcome`] (so a poke can ack the post-apply state).
    ///
    /// When `verify` is set (login/logout pokes only, not the initial startup
    /// federation), and an upstream is desired, the managed router must report
    /// its configured outbound link established. A failed wait is reported (and
    /// logged loudly) as a [`LinkState::Error`] instead of a false verified success.
    async fn poll_and_apply(&self, applied: &mut AppliedState, verify: bool) -> FederationOutcome {
        let cleanup_task = {
            let mut cleanup = self.late_resolve_cleanup.lock().await;
            if cleanup.as_ref().is_some_and(|task| !task.is_finished()) {
                drop(cleanup);
                return self
                    .fail_resolve_or_expire(
                        applied,
                        "previous timed-out resolve is still being cleaned up".to_string(),
                    )
                    .await;
            }
            // A finished cleanup has already rolled back any receipt. Await it
            // to observe a wrapper panic before allowing another resolver to
            // recover or mutate identity state.
            cleanup.take()
        };
        if let Some(task) = cleanup_task
            && let Err(error) = task.await
        {
            warn!(
                error = %error,
                "router federation: late-resolve cleanup task panicked"
            );
            return self
                .fail_resolve_or_expire(
                    applied,
                    "late-resolve cleanup task panicked; refusing concurrent identity maintenance"
                        .to_string(),
                )
                .await;
        }

        // Backend resolution receives the configured resolve deadline. Router
        // process work receives its own bounded budget.
        let resolve_deadline = tokio::time::Instant::now() + self.connect_timeout;

        // The resolver is blocking (HTTP + file I/O); keep it off the async worker. It
        // also re-pulls the platform router's config when the cached copy has gone
        // stale (cache freshness only, not a keepalive). Bound the whole resolve by
        // `connect_timeout` so a hung pull can't stall a poll (or the startup gate)
        // past it. A blocking task cannot be cancelled safely: on timeout retain
        // its JoinHandle in an async cleanup task, and explicitly roll back any
        // unverified identity it eventually returns. The rotation's armed Drop
        // guard remains a final fallback if runtime shutdown aborts that cleanup.
        let resolver = self.resolver.clone();
        let mut resolve_task = tokio::task::spawn_blocking(move || resolver());
        let mut resolved = match tokio::time::timeout_at(resolve_deadline, &mut resolve_task).await
        {
            Ok(Ok(Ok(t))) => t,
            Ok(Ok(Err(message))) => {
                warn!(error = %message, "router federation: desired-state resolve failed; will retry");
                return self.fail_resolve_or_expire(applied, message).await;
            }
            Ok(Err(e)) => {
                warn!(error = %e, "router federation: resolve task panicked; will retry");
                return self
                    .fail_resolve_or_expire(applied, format!("resolve task panicked: {e}"))
                    .await;
            }
            Err(_elapsed) => {
                // Detach only the async cleanup wrapper; it retains and awaits
                // the otherwise non-cancellable blocking JoinHandle.
                let cleanup = tokio::spawn(cleanup_late_resolve(resolve_task));
                self.late_resolve_cleanup.lock().await.replace(cleanup);
                warn!("router federation: resolve timed out; local router stays as-is, will retry");
                return self
                    .fail_resolve_or_expire(applied, "resolve timed out".to_string())
                    .await;
            }
        };
        let resolved_certificate_deadline = resolved
            .certificate_expires_after
            .map(|remaining| tokio::time::Instant::now() + remaining);
        applied.next_maintenance = resolved.maintenance_after;
        if let Some(error) = resolved.renewal_error.take() {
            applied.renewal_failures = applied.renewal_failures.saturating_add(1);
            applied.certificate_error = Some(error.clone());
            warn!(
                error = %error,
                retry_after = ?applied.renewal_retry_delay(),
                "router federation: certificate maintenance failed; a still-valid generation remains active while renewal backs off"
            );
        } else {
            applied.renewal_failures = 0;
            applied.certificate_error = None;
        }
        let mut rotation = resolved.rotation.take();
        applied.pat_active = resolved.pat_active;
        if resolved.upstream.is_some()
            && resolved_certificate_deadline
                .is_some_and(|deadline| deadline <= tokio::time::Instant::now())
        {
            let prior_applied = applied.clone();
            if rotation.is_some() {
                return self
                    .reject_rotation_and_restore(
                        rotation.take(),
                        applied,
                        &prior_applied,
                        "resolved core-node certificate expired before router apply".to_string(),
                    )
                    .await;
            }
            // The exact validated desired generation is already expired. Mark
            // the currently configured identity terminal and reuse the common
            // bounded standalone apply path; never announce/apply this target.
            applied.certificate_expires_at = Some(tokio::time::Instant::now());
            return self
                .fail_resolve_or_expire(
                    applied,
                    "resolved core-node certificate expired before router apply".to_string(),
                )
                .await;
        }
        if rotation.is_some() {
            applied.certificate_renewing = true;
            self.publish_progress(applied);
        }
        if let Some(error) = resolved.resolve_error.take() {
            // A transient control-plane failure is not a desired standalone
            // state. Preserve the currently applied valid link and retry; at
            // startup the applied state is already standalone, so this remains
            // fail closed without needlessly tearing down a healthy old link.
            return self.fail_resolve_or_expire(applied, error).await;
        }

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
            if let Some(rotation) = rotation.take() {
                // Keep the durable unverified marker across the generation
                // restart. The next daemon recovers the receipt, forces a real
                // probe on its initial apply, then commits/prunes.
                if let Err(error) = rotation.retain_for_restart() {
                    applied.certificate_renewing = false;
                    return FederationOutcome::Failed(format!(
                        "could not hand off the unverified identity across the namespace restart: {error}"
                    ));
                }
            }
            applied.certificate_renewing = false;
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
        let unchanged = applied.last_settled_desired.matches(&resolved.upstream);
        let prior_applied = applied.clone();

        // Apply the desired upstream, or replay the cached outcome when the
        // rendered locator (endpoint + TLS material) is unchanged.
        let should_apply = !unchanged || (verify && applied.needs_reapply);
        if !should_apply {
            if applied.pinned {
                return FederationOutcome::Pinned;
            }
            // Equality includes the immutable generation-specific TLS paths,
            // so only an actually unchanged applied generation may refresh its
            // validated absolute-expiry projection.
            applied.certificate_expires_at = resolved_certificate_deadline;
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
            let desired_link = resolved
                .upstream
                .as_ref()
                .map(DesiredBackend::upstream_link);
            match tokio::time::timeout_at(apply_deadline, (self.federator)(desired_link)).await {
                Err(_elapsed) => {
                    warn!(
                        "router federation: applying the upstream change timed out, so federation \
                         with the platform router is NOT in effect; will retry"
                    );
                    return self
                        .reject_rotation_and_restore(
                            rotation.take(),
                            applied,
                            &prior_applied,
                            "apply timed out".to_string(),
                        )
                        .await;
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
                            resolved.upstream.clone(),
                        ),
                        pinned: false,
                        needs_reapply: false,
                        next_maintenance: applied.next_maintenance,
                        certificate_expires_at: resolved_certificate_deadline,
                        expiry_defederation_pending: false,
                        renewal_failures: applied.renewal_failures,
                        pat_active: applied.pat_active,
                        certificate_error: applied.certificate_error.clone(),
                        certificate_renewing: applied.certificate_renewing,
                    };
                    if resolved_certificate_deadline
                        .is_some_and(|deadline| deadline <= tokio::time::Instant::now())
                    {
                        return self
                            .reject_expired_resolved_identity(
                                &mut rotation,
                                applied,
                                &prior_applied,
                                "while the router apply was completing",
                            )
                            .await;
                    }
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
                    if rotation.is_some() {
                        return self
                            .reject_rotation_and_restore(
                                rotation.take(),
                                applied,
                                &prior_applied,
                                "the managed router is pinned and could not load the renewed certificate"
                                    .to_string(),
                            )
                            .await;
                    }
                    applied.last_settled_desired =
                        SettledDesired::from_completed(resolved.upstream.clone());
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
                    return self
                        .reject_rotation_and_restore(
                            rotation.take(),
                            applied,
                            &prior_applied,
                            e.to_string(),
                        )
                        .await;
                }
            }
        }

        // Verify the actual managed-router link (login/logout pokes only). A failed
        // wait marks the link errored and requests a bounce on the next verifying
        // poke (the user may replace certificate files between attempts).
        let verify = verify || rotation.is_some();
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
                    if resolved_certificate_deadline
                        .is_some_and(|deadline| deadline <= tokio::time::Instant::now())
                    {
                        return self
                            .reject_expired_resolved_identity(
                                &mut rotation,
                                applied,
                                &prior_applied,
                                "while managed Zenoh link verification was completing",
                            )
                            .await;
                    }
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
                    if rotation.is_some() {
                        return self
                            .reject_rotation_and_restore(
                                rotation.take(),
                                applied,
                                &prior_applied,
                                "the renewed core-node certificate failed managed Zenoh link verification"
                                    .to_string(),
                            )
                            .await;
                    }
                }
            }
        }

        // Cover unchanged targets, failed non-rotation probes, and the small
        // interval between the phase-specific check and receipt commit. Never
        // report Applied or commit a generation whose deadline is now elapsed.
        if resolved_certificate_deadline
            .is_some_and(|deadline| deadline <= tokio::time::Instant::now())
        {
            return self
                .reject_expired_resolved_identity(
                    &mut rotation,
                    applied,
                    &prior_applied,
                    "before the federation result was committed",
                )
                .await;
        }

        #[cfg(test)]
        if !self.finalization_delay.is_zero() {
            // The real commit below performs blocking protected-file reads,
            // durable unlink/fsync, and generation pruning. Sleeping here gives
            // the unit test a deterministic model of that elapsed I/O without
            // exposing identity internals across crates.
            std::thread::sleep(self.finalization_delay);
        }
        if let Some(rotation) = rotation.take() {
            if let Err(error) = rotation.commit_after_probe() {
                // The new link is already verified. Failure to prune old files
                // is recoverable cleanup debt, not a reason to tear down a good
                // federation link or restore the old certificate.
                warn!(
                    error = %error,
                    "router federation: renewed certificate verified, but superseded generation cleanup failed"
                );
            }
            applied.renewal_failures = 0;
            applied.certificate_error = None;
        }
        // `commit_after_probe` is blocking durable I/O and can cross the exact
        // monotonic deadline even when the pre-commit check passed. At this
        // point the receipt may be gone and the prior generation pruned, so
        // rollback is no longer available: render standalone with the common
        // bounded hard-expiry path before any Applied/Verified result escapes.
        if resolved_certificate_deadline
            .is_some_and(|deadline| deadline <= tokio::time::Instant::now())
        {
            return self
                .reject_expired_resolved_identity(
                    &mut rotation,
                    applied,
                    &prior_applied,
                    "while durable identity finalization was completing",
                )
                .await;
        }
        applied.certificate_renewing = false;

        FederationOutcome::Applied(applied.platform_link())
    }

    /// A resolved generation may be valid before apply yet expire while Zenoh
    /// restarts or its managed-link verification runs. A rotated generation restores its
    /// still-valid prior receipt (or intentional standalone); an already-active
    /// generation uses the common bounded standalone path.
    async fn reject_expired_resolved_identity(
        &self,
        rotation: &mut Option<auth::IdentityRotation>,
        applied: &mut AppliedState,
        prior: &AppliedState,
        stage: &str,
    ) -> FederationOutcome {
        let reason = format!("resolved core-node certificate expired {stage}");
        if rotation.is_some() {
            return self
                .reject_rotation_and_restore(rotation.take(), applied, prior, reason)
                .await;
        }
        applied.certificate_expires_at = Some(tokio::time::Instant::now());
        self.fail_resolve_or_expire(applied, reason).await
    }

    /// A transient control-plane failure may preserve an already-applied link
    /// only while its client certificate remains valid. At the independent
    /// hard deadline, explicitly render/apply standalone even though the
    /// resolver still cannot provide fresh desired state.
    async fn fail_resolve_or_expire(
        &self,
        applied: &mut AppliedState,
        reason: String,
    ) -> FederationOutcome {
        if !applied.certificate_expired() {
            return FederationOutcome::Failed(reason);
        }

        let mut failure = format!(
            "{reason}; active core-node certificate reached its hard expiry, de-federating fail closed"
        );
        applied.certificate_renewing = false;
        applied.certificate_error = Some(failure.clone());
        applied.renewal_failures = applied.renewal_failures.saturating_add(1);
        applied.expiry_defederation_pending = true;
        let defederated = match tokio::time::timeout(self.apply_timeout, (self.federator)(None))
            .await
        {
            Ok(Ok(true)) => {
                applied.endpoint = None;
                applied.link_state = LinkState::NotConfigured;
                applied.last_settled_desired = SettledDesired::Standalone;
                applied.pinned = false;
                applied.needs_reapply = false;
                applied.certificate_expires_at = None;
                applied.expiry_defederation_pending = false;
                true
            }
            Ok(Ok(false)) => {
                applied.last_settled_desired = SettledDesired::Unsettled;
                applied.pinned = true;
                applied.needs_reapply = true;
                failure
                    .push_str("; ZENOH_CONFIG is pinned, so automatic de-federation was refused");
                false
            }
            Ok(Err(error)) => {
                applied.last_settled_desired = SettledDesired::Unsettled;
                applied.needs_reapply = true;
                failure.push_str(&format!("; standalone reapply failed: {error}"));
                false
            }
            Err(_) => {
                applied.last_settled_desired = SettledDesired::Unsettled;
                applied.needs_reapply = true;
                failure.push_str("; standalone reapply timed out");
                false
            }
        };
        if defederated {
            // The expired upstream is gone. Discard its stale (possibly zero)
            // maintenance wake and expiry backoff/error state; the Failed
            // outcome still schedules the ordinary nonzero resolver retry.
            applied.next_maintenance = None;
            applied.renewal_failures = 0;
            applied.certificate_error = None;
            warn!(error = %failure, "router federation: certificate expired during resolver failure");
            return FederationOutcome::Failed(failure);
        }
        applied.certificate_error = Some(failure.clone());
        warn!(error = %failure, "router federation: certificate expired during resolver failure");
        FederationOutcome::Failed(failure)
    }

    /// Rejects an unverified generation, restores its metadata pointer, and
    /// immediately re-renders/restarts Zenoh with the prior desired TLS paths.
    /// Renewal backoff begins only after this restore attempt, avoiding a window
    /// where the running router references a deleted rejected generation.
    async fn reject_rotation_and_restore(
        &self,
        rotation: Option<auth::IdentityRotation>,
        applied: &mut AppliedState,
        prior: &AppliedState,
        reason: String,
    ) -> FederationOutcome {
        let Some(rotation) = rotation else {
            return FederationOutcome::Failed(reason);
        };
        applied.certificate_renewing = false;
        let next_maintenance = applied.next_maintenance;
        let failures = applied.renewal_failures.saturating_add(1);
        let rejected_generation = match rotation.rollback_for_router_restore() {
            Ok(rejected) => rejected,
            Err(error) => {
                Self::mark_restore_uncertain(applied, prior, next_maintenance, failures);
                applied.certificate_error = Some(reason.clone());
                return FederationOutcome::Failed(format!(
                    "{reason}; core-node certificate rollback also failed: {error}"
                ));
            }
        };
        let restored_previous = rejected_generation.restored_previous();
        let mut reason = if restored_previous {
            format!("{reason}; restored the previous still-valid core-node certificate metadata")
        } else {
            format!(
                "{reason}; no still-valid prior core-node certificate remained, so restored identity state is intentionally standalone"
            )
        };

        let prior_link = match (&prior.last_settled_desired, restored_previous) {
            (SettledDesired::Upstream(backend), true) => Some(backend.upstream_link()),
            _ => None,
        };
        let restore = tokio::time::timeout(self.apply_timeout, (self.federator)(prior_link)).await;
        let mut prior_router_confirmed = false;
        match restore {
            Ok(Ok(true)) => {
                *applied = prior.clone();
                applied.next_maintenance = next_maintenance;
                applied.renewal_failures = failures;
                applied.needs_reapply = false;
                if !restored_previous {
                    applied.endpoint = None;
                    applied.link_state = LinkState::NotConfigured;
                    applied.last_settled_desired = SettledDesired::Standalone;
                    applied.pinned = false;
                    applied.needs_reapply = false;
                    applied.certificate_expires_at = None;
                    prior_router_confirmed = true;
                } else if let SettledDesired::Upstream(backend) = &prior.last_settled_desired {
                    match probe_with_bound(
                        self.prober.clone(),
                        backend.endpoint.host().to_string(),
                        backend.endpoint.port(),
                        backend.tls.clone(),
                        PROBE_TIMEOUT,
                    )
                    .await
                    {
                        Ok(()) => {
                            applied.link_state = LinkState::Verified;
                            prior_router_confirmed = true;
                        }
                        Err(error) => {
                            applied.link_state = LinkState::Error(error.clone());
                            applied.needs_reapply = true;
                            reason.push_str(&format!(
                                "; prior-generation managed Zenoh link verification failed: {error}"
                            ));
                        }
                    }
                } else {
                    // A successful apply of `None` confirms the router is now
                    // standalone and no longer references the rejected files.
                    prior_router_confirmed = true;
                }
            }
            Ok(Ok(false)) => {
                // Operator-pinned configuration cannot be rewritten here. Its
                // actual path ownership remains external; preserve the prior
                // reported state and keep retrying maintenance with backoff.
                Self::mark_restore_uncertain(applied, prior, next_maintenance, failures);
                applied.pinned = true;
                reason.push_str(
                    "; prior generation could not be re-applied because ZENOH_CONFIG is pinned",
                );
            }
            Ok(Err(error)) => {
                Self::mark_restore_uncertain(applied, prior, next_maintenance, failures);
                reason.push_str(&format!("; prior-generation reapply failed: {error}"));
            }
            Err(_) => {
                Self::mark_restore_uncertain(applied, prior, next_maintenance, failures);
                reason.push_str("; prior-generation reapply timed out");
            }
        }
        if prior_router_confirmed
            && let Err(error) = rejected_generation.cleanup_after_router_restore()
        {
            reason.push_str(&format!(
                "; prior/standalone router state was restored, but rejected-generation cleanup failed: {error}"
            ));
        }
        warn!(
            error = %reason,
            retry_after = ?applied.renewal_retry_delay(),
            "router federation: rejected renewed certificate generation and attempted immediate prior-generation restore"
        );
        applied.certificate_error = Some(reason.clone());
        FederationOutcome::Failed(reason)
    }

    fn mark_restore_uncertain(
        applied: &mut AppliedState,
        prior: &AppliedState,
        next_maintenance: Option<Duration>,
        failures: u32,
    ) {
        *applied = prior.clone();
        applied.last_settled_desired = SettledDesired::Unsettled;
        applied.needs_reapply = true;
        applied.next_maintenance = next_maintenance;
        applied.renewal_failures = failures;
    }

    fn publish_progress(&self, applied: &AppliedState) {
        if let Some(status_tx) = &self.status_tx {
            status_tx.send_replace(FederationStatus {
                link: applied.platform_link(),
                pinned: applied.pinned,
                pat_active: applied.pat_active,
                certificate_error: applied.certificate_error.clone(),
                certificate_renewing: applied.certificate_renewing,
            });
        }
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
    upstream: Option<UpstreamLink>,
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
        .restart_router_and_wait_for_session(SESSION_RECONNECT_TIMEOUT)
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
            rotation: None,
            maintenance_after: None,
            certificate_expires_after: None,
            renewal_error: None,
            resolve_error: None,
            pat_active: false,
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
            status_tx: None,
            late_resolve_cleanup: Mutex::new(None),
            finalization_delay: Duration::ZERO,
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
            initial_pat_active: false,
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
                rotation: None,
                maintenance_after: None,
                certificate_expires_after: None,
                renewal_error: None,
                resolve_error: None,
                pat_active: false,
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
            tls: pmi::TlsConfig::default(),
        })
    }

    #[test]
    fn renewal_backoff_becomes_urgent_before_the_hard_deadline() {
        let applied = AppliedState {
            renewal_failures: 1,
            next_maintenance: Some(Duration::from_secs(10)),
            ..AppliedState::default()
        };
        assert_eq!(applied.timer_after(false), Some(Duration::from_secs(5)));

        let nearly_expired = AppliedState {
            renewal_failures: 6,
            next_maintenance: Some(Duration::from_secs(1)),
            ..AppliedState::default()
        };
        assert_eq!(
            nearly_expired.timer_after(false),
            Some(Duration::from_secs(1))
        );

        let applied_deadline = AppliedState {
            next_maintenance: Some(Duration::from_secs(60)),
            certificate_expires_at: Some(tokio::time::Instant::now() + Duration::from_secs(2)),
            ..AppliedState::default()
        };
        assert!(applied_deadline.timer_after(false).unwrap() <= Duration::from_secs(2));
    }

    #[tokio::test]
    async fn scheduled_maintenance_wakes_without_a_cli_poke() {
        let calls = Arc::new(AtomicUsize::new(0));
        let counted = calls.clone();
        let resolver: Resolver = Arc::new(move || {
            counted.fetch_add(1, Ordering::SeqCst);
            Ok(Resolved {
                upstream: None,
                namespace: Namespace::local(),
                rotation: None,
                maintenance_after: Some(Duration::from_millis(20)),
                certificate_expires_after: None,
                renewal_error: None,
                resolve_error: None,
                pat_active: false,
            })
        });
        let (prober, _) = counting_prober(Ok(()));
        let (_messaging_tx, messaging_rx) = watch::channel(true);
        let (trigger_tx, trigger_rx) = trigger_channel();
        let (ready_tx, ready_rx) = oneshot::channel();
        let (federation, _status) = federation_under_test(
            applying_federator(),
            resolver,
            prober,
            messaging_rx,
            trigger_rx,
        );
        let task = tokio::spawn(federation.manage(ready_tx));
        ready_rx.await.expect("initial maintenance completes");

        tokio::time::timeout(Duration::from_secs(1), async {
            while calls.load(Ordering::SeqCst) < 2 {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("scheduled maintenance fires without a poke");
        drop(trigger_tx);
        task.await
            .expect("federation loop exits when control closes");
    }

    fn recording_federator() -> (Federator, Arc<std::sync::Mutex<Vec<Option<UpstreamLink>>>>) {
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
                rotation: None,
                maintenance_after: None,
                certificate_expires_after: None,
                renewal_error: None,
                resolve_error: None,
                pat_active: false,
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
    async fn real_prober_requires_managed_router_link_evidence() {
        let messenger = Arc::new(Mutex::new(Messenger::new(pmi::MessengerAdapter::Mock(
            pmi::MockAdapter::default(),
        ))));

        let error = real_prober(messenger)(
            "127.0.0.1".to_string(),
            7447,
            pmi::TlsConfig::default(),
            Duration::from_millis(50),
        )
        .await
        .expect_err("a raw connection must not substitute for managed-router link evidence");

        assert!(
            error.contains("exposes no configured link"),
            "unexpected verification error: {error}"
        );
    }

    #[tokio::test]
    async fn an_upstream_apply_hands_pmi_the_typed_platform_link() {
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
        assert_eq!(
            *calls.lock().unwrap(),
            vec![Some(UpstreamLink {
                endpoint: ENDPOINT.to_string(),
                tls: pmi::TlsConfig::default(),
            })],
            "the apply carries the endpoint and its TLS material typed; pmi renders the locator"
        );
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
    async fn changed_tls_material_reapplies_the_unchanged_endpoint() {
        // Same endpoint, refreshed TLS material (a re-issued certificate
        // changes the connect paths): the desired target differs, so the poll
        // must re-apply even though the endpoint is identical.
        let generation_2_tls = pmi::TlsConfig {
            connect_certificate: Some("/certs/generation-2/cert.pem".into()),
            ..pmi::TlsConfig::default()
        };
        let refreshed = Some(DesiredBackend {
            endpoint: dial(ENDPOINT),
            tls: generation_2_tls.clone(),
        });
        let (resolver, _) = counting_resolver(refreshed.clone());
        let (federator, calls) = recording_federator();
        let (prober, _) = counting_prober(Ok(()));
        let mut applied = AppliedState {
            endpoint: Some(ENDPOINT.to_string()),
            link_state: LinkState::Unverified,
            last_settled_desired: SettledDesired::Upstream(DesiredBackend {
                endpoint: dial(ENDPOINT),
                tls: pmi::TlsConfig {
                    connect_certificate: Some("/certs/generation-1/cert.pem".into()),
                    ..pmi::TlsConfig::default()
                },
            }),
            pinned: false,
            needs_reapply: false,
            next_maintenance: None,
            certificate_expires_at: None,
            expiry_defederation_pending: false,
            renewal_failures: 0,
            pat_active: false,
            certificate_error: None,
            certificate_renewing: false,
        };

        let outcome = poller_under_test(federator, resolver, prober)
            .poll_and_apply(&mut applied, false)
            .await;

        assert!(matches!(outcome, FederationOutcome::Applied(_)));
        assert_eq!(
            *calls.lock().unwrap(),
            vec![Some(UpstreamLink {
                endpoint: ENDPOINT.to_string(),
                tls: generation_2_tls,
            })],
            "changed TLS material must re-render the router config"
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
            SettledDesired::Upstream(DesiredBackend {
                endpoint: dial(ENDPOINT),
                tls: pmi::TlsConfig::default(),
            }),
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
            last_settled_desired: SettledDesired::Upstream(DesiredBackend {
                endpoint: dial(ENDPOINT),
                tls: pmi::TlsConfig::default(),
            }),
            pinned: false,
            needs_reapply: false,
            next_maintenance: None,
            certificate_expires_at: None,
            expiry_defederation_pending: false,
            renewal_failures: 0,
            pat_active: false,
            certificate_error: None,
            certificate_renewing: false,
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

    #[tokio::test]
    async fn transient_resolve_failure_defederates_an_expired_applied_generation() {
        let resolver: Resolver = Arc::new(|| Err("issuer unavailable".to_string()));
        let (federator, calls) = recording_federator();
        let (prober, _) = counting_prober(Ok(()));
        let backend = upstream().unwrap();
        let mut applied = AppliedState {
            endpoint: Some(ENDPOINT.to_string()),
            link_state: LinkState::Verified,
            last_settled_desired: SettledDesired::Upstream(backend),
            next_maintenance: Some(Duration::ZERO),
            certificate_expires_at: Some(tokio::time::Instant::now()),
            renewal_failures: 4,
            ..AppliedState::default()
        };

        let outcome = poller_under_test(federator, resolver, prober)
            .poll_and_apply(&mut applied, false)
            .await;
        assert!(
            matches!(outcome, FederationOutcome::Failed(ref message) if message.contains("hard expiry"))
        );
        assert_eq!(*calls.lock().unwrap(), vec![None]);
        assert!(applied.endpoint.is_none());
        assert_eq!(applied.last_settled_desired, SettledDesired::Standalone);
        assert!(applied.certificate_expires_at.is_none());
        assert!(!applied.expiry_defederation_pending);
        assert!(applied.next_maintenance.is_none());
        assert_eq!(applied.renewal_failures, 0);
        assert!(applied.certificate_error.is_none());
        assert_eq!(
            applied.timer_after(true),
            Some(RETRY_DELAY),
            "successful fail-closed apply must discard stale zero wakes and use the ordinary retry floor"
        );
    }

    #[tokio::test]
    async fn forced_standalone_does_not_resolve_retained_auth() {
        let resolver: Resolver = Arc::new(|| panic!("forced standalone must not resolve auth"));
        let (federator, calls) = recording_federator();
        let (prober, _) = counting_prober(Ok(()));
        let mut applied = AppliedState {
            endpoint: Some(ENDPOINT.to_string()),
            link_state: LinkState::Verified,
            last_settled_desired: SettledDesired::Upstream(upstream().unwrap()),
            certificate_expires_at: Some(tokio::time::Instant::now() + Duration::from_secs(3600)),
            ..AppliedState::default()
        };

        let outcome = poller_under_test(federator, resolver, prober)
            .force_standalone(&mut applied)
            .await;

        assert_eq!(
            outcome,
            FederationOutcome::Applied(PlatformLink {
                endpoint: None,
                link_state: LinkState::NotConfigured,
            })
        );
        assert_eq!(*calls.lock().unwrap(), vec![None]);
        assert!(applied.certificate_expires_at.is_none());
        assert_eq!(applied.last_settled_desired, SettledDesired::Standalone);
    }

    async fn assert_failed_expiry_defederation_uses_backoff(
        federator: Federator,
        apply_timeout: Duration,
    ) {
        let resolver: Resolver = Arc::new(|| Err("issuer unavailable".to_string()));
        let (prober, _) = counting_prober(Ok(()));
        let mut applied = AppliedState {
            endpoint: Some(ENDPOINT.to_string()),
            link_state: LinkState::Verified,
            last_settled_desired: SettledDesired::Upstream(upstream().unwrap()),
            next_maintenance: Some(Duration::ZERO),
            certificate_expires_at: Some(tokio::time::Instant::now()),
            ..AppliedState::default()
        };
        let mut poller = poller_under_test(federator, resolver, prober);
        poller.apply_timeout = apply_timeout;

        let outcome = poller.poll_and_apply(&mut applied, false).await;

        assert!(matches!(outcome, FederationOutcome::Failed(_)));
        assert!(applied.certificate_expired());
        assert!(applied.expiry_defederation_pending);
        assert_eq!(
            applied.timer_after(true),
            Some(applied.renewal_retry_delay()),
            "an elapsed certificate and stale zero maintenance deadline must not busy-loop"
        );
        assert!(applied.timer_after(true).unwrap() >= RENEWAL_RETRY_BASE);
        assert!(applied.timer_after(true).unwrap() <= RENEWAL_RETRY_MAX);
    }

    #[tokio::test]
    async fn failed_hard_expiry_defederation_retries_on_bounded_backoff() {
        let pinned: Federator = Arc::new(|_upstream| Box::pin(async { Ok(false) }));
        assert_failed_expiry_defederation_uses_backoff(pinned, APPLY_TIMEOUT).await;

        let failed: Federator = Arc::new(|_upstream| {
            Box::pin(async {
                Err(Error::ExecutionFailed(
                    "could not render standalone".to_string(),
                ))
            })
        });
        assert_failed_expiry_defederation_uses_backoff(failed, APPLY_TIMEOUT).await;

        assert_failed_expiry_defederation_uses_backoff(
            wedged_federator(),
            Duration::from_millis(5),
        )
        .await;
    }

    #[tokio::test]
    async fn successfully_resolved_but_already_expired_identity_is_never_applied() {
        let resolver: Resolver = Arc::new(|| {
            let mut value = resolved(upstream());
            value.certificate_expires_after = Some(Duration::ZERO);
            Ok(value)
        });
        let (federator, calls) = recording_federator();
        let (prober, _) = counting_prober(Ok(()));
        let mut applied = AppliedState::default();

        let outcome = poller_under_test(federator, resolver, prober)
            .poll_and_apply(&mut applied, false)
            .await;
        assert!(
            matches!(outcome, FederationOutcome::Failed(ref message) if message.contains("expired before router apply"))
        );
        assert_eq!(
            *calls.lock().unwrap(),
            vec![None],
            "the expired desired upstream must never reach the federator"
        );
    }

    #[tokio::test]
    async fn identity_expiring_during_router_apply_is_never_reported_applied() {
        let resolver: Resolver = Arc::new(|| {
            let mut value = resolved(upstream());
            value.certificate_expires_after = Some(Duration::from_millis(50));
            Ok(value)
        });
        let calls = Arc::new(std::sync::Mutex::new(Vec::new()));
        let recorded = calls.clone();
        let federator: Federator = Arc::new(move |upstream| {
            let applying_upstream = upstream.is_some();
            recorded.lock().unwrap().push(upstream);
            Box::pin(async move {
                if applying_upstream {
                    tokio::time::sleep(Duration::from_millis(80)).await;
                }
                Ok(true)
            })
        });
        let (prober, _) = counting_prober(Ok(()));
        let mut applied = AppliedState::default();

        let outcome = poller_under_test(federator, resolver, prober)
            .poll_and_apply(&mut applied, false)
            .await;

        assert!(
            matches!(outcome, FederationOutcome::Failed(ref message) if message.contains("while the router apply was completing")),
            "got {outcome:?}"
        );
        let calls = calls.lock().unwrap();
        assert!(calls.first().is_some_and(Option::is_some));
        assert!(calls.last().is_some_and(Option::is_none));
        assert_eq!(applied.last_settled_desired, SettledDesired::Standalone);
        assert!(applied.certificate_expires_at.is_none());
    }

    #[tokio::test]
    async fn identity_expiring_during_link_verification_is_never_verified_or_committed() {
        let resolver: Resolver = Arc::new(|| {
            let mut value = resolved(upstream());
            value.certificate_expires_after = Some(Duration::from_millis(200));
            Ok(value)
        });
        let (federator, calls) = recording_federator();
        let prober: Prober = Arc::new(|_host, _port, _tls, _timeout| {
            Box::pin(async {
                tokio::time::sleep(Duration::from_millis(250)).await;
                Ok(())
            })
        });
        let mut applied = AppliedState::default();

        let outcome = poller_under_test(federator, resolver, prober)
            .poll_and_apply(&mut applied, true)
            .await;

        assert!(
            matches!(outcome, FederationOutcome::Failed(ref message) if message.contains("while managed Zenoh link verification was completing")),
            "got {outcome:?}"
        );
        let calls = calls.lock().unwrap();
        assert!(calls.first().is_some_and(Option::is_some));
        assert!(calls.last().is_some_and(Option::is_none));
        assert_eq!(applied.link_state, LinkState::NotConfigured);
        assert_eq!(applied.last_settled_desired, SettledDesired::Standalone);
        assert!(applied.certificate_expires_at.is_none());
    }

    #[tokio::test]
    async fn identity_expiring_during_durable_finalization_is_never_reported_applied() {
        let resolver: Resolver = Arc::new(|| {
            let mut value = resolved(upstream());
            value.certificate_expires_after = Some(Duration::from_millis(200));
            Ok(value)
        });
        let (federator, calls) = recording_federator();
        let (prober, _) = counting_prober(Ok(()));
        let mut applied = AppliedState::default();
        let mut poller = poller_under_test(federator, resolver, prober);
        // Models blocking receipt validation, unlink/fsync, and generation
        // pruning after the pre-commit deadline check has already passed.
        poller.finalization_delay = Duration::from_millis(250);

        let outcome = poller.poll_and_apply(&mut applied, false).await;

        assert!(
            matches!(outcome, FederationOutcome::Failed(ref message) if message.contains("while durable identity finalization was completing")),
            "got {outcome:?}"
        );
        let calls = calls.lock().unwrap();
        assert!(calls.first().is_some_and(Option::is_some));
        assert!(calls.last().is_some_and(Option::is_none));
        assert_eq!(applied.link_state, LinkState::NotConfigured);
        assert_eq!(applied.last_settled_desired, SettledDesired::Standalone);
        assert!(applied.certificate_expires_at.is_none());
    }

    #[tokio::test]
    async fn uncertain_restore_forces_the_next_poll_to_reapply_prior_state() {
        let backend = upstream().unwrap();
        let prior = AppliedState {
            endpoint: Some(ENDPOINT.to_string()),
            link_state: LinkState::Verified,
            last_settled_desired: SettledDesired::Upstream(backend.clone()),
            ..AppliedState::default()
        };
        let mut applied = AppliedState::default();
        FederationPoller::mark_restore_uncertain(&mut applied, &prior, None, 1);
        assert_eq!(applied.last_settled_desired, SettledDesired::Unsettled);
        assert!(applied.needs_reapply);

        let (resolver, _) = counting_resolver(Some(backend));
        let (federator, calls) = recording_federator();
        let (prober, _) = counting_prober(Ok(()));
        let outcome = poller_under_test(federator, resolver, prober)
            .poll_and_apply(&mut applied, false)
            .await;
        assert!(matches!(outcome, FederationOutcome::Applied(_)));
        assert_eq!(
            calls.lock().unwrap().len(),
            1,
            "an uncertain timeout/error restore cannot cache-hit the prior target"
        );
    }

    #[tokio::test]
    async fn initial_resolve_error_cannot_clear_captured_pat_status() {
        let resolver: Resolver = Arc::new(|| Err("backend unavailable".to_string()));
        let (prober, _) = counting_prober(Ok(()));
        let (_messaging_tx, messaging_rx) = watch::channel(true);
        let (trigger_tx, trigger_rx) = trigger_channel();
        let (ready_tx, ready_rx) = oneshot::channel();
        let (mut federation, status_rx) = federation_under_test(
            applying_federator(),
            resolver,
            prober,
            messaging_rx,
            trigger_rx,
        );
        federation.initial_pat_active = true;
        let task = tokio::spawn(federation.manage(ready_tx));
        ready_rx.await.unwrap();
        assert!(status_rx.borrow().pat_active);
        drop(trigger_tx);
        task.await.unwrap();
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
        trigger_tx
            .send(FederationTrigger::Refederate(ack_tx))
            .await
            .expect("trigger accepted");
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
        trigger_tx
            .send(FederationTrigger::Refederate(ack_tx))
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
        trigger_tx
            .send(FederationTrigger::Refederate(ack_tx))
            .await
            .expect("trigger accepted");
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
        trigger_tx
            .send(FederationTrigger::Refederate(ack_tx))
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

    #[tokio::test]
    async fn late_resolve_cleanup_retains_and_awaits_the_blocking_task() {
        let finished = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let mark_finished = finished.clone();
        let resolve_task = tokio::task::spawn_blocking(move || {
            std::thread::sleep(Duration::from_millis(40));
            mark_finished.store(true, Ordering::SeqCst);
            Ok(resolved(None))
        });

        cleanup_late_resolve(resolve_task).await;
        assert!(
            finished.load(Ordering::SeqCst),
            "cleanup must retain and await the non-cancellable blocking resolver"
        );
    }

    #[tokio::test]
    async fn a_retry_cannot_race_a_timed_out_resolve_cleanup() {
        let calls = Arc::new(AtomicUsize::new(0));
        let counted = calls.clone();
        let resolver: Resolver = Arc::new(move || {
            let call = counted.fetch_add(1, Ordering::SeqCst);
            if call == 0 {
                std::thread::sleep(Duration::from_millis(80));
            }
            Ok(resolved(None))
        });
        let (prober, _) = counting_prober(Ok(()));
        let mut poller = poller_under_test(applying_federator(), resolver, prober);
        poller.connect_timeout = Duration::from_millis(10);
        let mut applied = AppliedState::default();

        assert_eq!(
            poller.poll_and_apply(&mut applied, false).await,
            FederationOutcome::Failed("resolve timed out".into())
        );
        assert_eq!(
            poller.poll_and_apply(&mut applied, false).await,
            FederationOutcome::Failed(
                "previous timed-out resolve is still being cleaned up".into()
            )
        );
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "an immediate poke must not start a second identity resolver"
        );

        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if poller
                    .late_resolve_cleanup
                    .lock()
                    .await
                    .as_ref()
                    .is_some_and(tokio::task::JoinHandle::is_finished)
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("late cleanup finishes");
        assert!(matches!(
            poller.poll_and_apply(&mut applied, false).await,
            FederationOutcome::Applied(_)
        ));
        assert_eq!(calls.load(Ordering::SeqCst), 2);
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
        trigger_tx
            .send(FederationTrigger::Refederate(ack_tx))
            .await
            .expect("trigger accepted");
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
