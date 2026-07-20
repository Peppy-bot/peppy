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

use crate::control::{
    AuthenticationState, CertificateState, FederationStatus, LinkState, PlatformLink,
    RouterApplyState,
};
use crate::error::Result;
use crate::identity_applicator::{
    IdentityApplicator, ManagedIdentityApplicator, OperatorManagedIdentityApplicator,
    RouterApplyDisposition,
};
use crate::router_process::RouterProcessRecorder;
use crate::serve::{ServeAsyncCommand, ServeAsyncHandle};
use config::namespace::Namespace;
use daemon_config::consts::PeppyDirs;
use daemon_config::peppy_config::ParsedEndpointBuf;
use pmi::{Messenger, UpstreamLink};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{Mutex, mpsc, oneshot, watch};
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};
use uuid::Uuid;

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
    force_standalone: bool,
    pat_active: bool,
    identity_snapshot: IdentitySnapshot,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct IdentitySnapshot {
    authentication: AuthenticationState,
    metadata: Option<auth::identity::CoreNodeIdentity>,
    binding_incomplete: bool,
    offline_recovery_required: bool,
}

fn read_identity_snapshot(dirs: &PeppyDirs, pat_active: bool) -> IdentitySnapshot {
    let mut snapshot = IdentitySnapshot {
        authentication: if pat_active {
            AuthenticationState::Pat
        } else {
            AuthenticationState::Missing
        },
        ..IdentitySnapshot::default()
    };
    match auth::storage::load(&auth::storage::credentials_path(dirs)) {
        Ok(credentials) => {
            if !pat_active && credentials.session.is_some() {
                snapshot.authentication = AuthenticationState::Oauth;
            }
            snapshot.metadata = credentials.core_node_identity;
        }
        Err(_) => snapshot.offline_recovery_required = true,
    }
    match auth::identity::load_identity_metadata(dirs) {
        Ok(metadata) => snapshot.metadata = metadata.or(snapshot.metadata),
        Err(_) => snapshot.offline_recovery_required = true,
    }
    if let Some(metadata) = snapshot.metadata.as_ref()
        && auth::identity::validate_identity_material(dirs, metadata).is_err()
    {
        snapshot.offline_recovery_required = true;
    }
    match auth::identity::binding_incomplete(dirs) {
        Ok(incomplete) => snapshot.binding_incomplete = incomplete,
        Err(_) => snapshot.offline_recovery_required = true,
    }
    snapshot
}

fn certificate_state_for(
    snapshot: &IdentitySnapshot,
    renewing: bool,
    certificate_error: Option<&str>,
) -> CertificateState {
    if snapshot.offline_recovery_required {
        return CertificateState::Error;
    }
    if snapshot.binding_incomplete {
        return CertificateState::Enrolling;
    }
    let Some(identity) = snapshot.metadata.as_ref() else {
        return if renewing {
            CertificateState::Enrolling
        } else if certificate_error.is_some() || snapshot.offline_recovery_required {
            CertificateState::Error
        } else {
            CertificateState::Missing
        };
    };
    let now = auth::storage::now_unix();
    if identity.not_before > now {
        CertificateState::Error
    } else if identity.not_after <= now {
        CertificateState::Expired
    } else if renewing {
        CertificateState::Renewing
    } else if identity.renew_after <= now {
        CertificateState::Expiring
    } else {
        CertificateState::Valid
    }
}

#[derive(Debug, Clone)]
struct EnrollmentRequest {
    expected_session_revision: Option<Uuid>,
    expected_pat_subject: Option<String>,
}

/// Resolves the desired platform upstream and namespace from the credentials.
/// `None` is scheduled reconciliation; `Some` is an explicit PAT/OAuth
/// enrollment carrying only its non-secret principal/revision fence. The PAT
/// API-origin fence is validated by the controller before this resolver runs.
type Resolver = Arc<
    dyn Fn(Option<EnrollmentRequest>) -> std::result::Result<Resolved, IdentityFailure>
        + Send
        + Sync,
>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum IdentityFailureCode {
    StaleSessionRevision,
    NotAuthenticated,
    PatNotConfigured,
    PatActive,
    PatPrincipalMismatch,
    PatOriginMismatch,
    DeadlineExceeded,
    OperationFailed,
}

#[derive(Debug, Clone)]
struct IdentityFailure {
    code: IdentityFailureCode,
    message: String,
}

impl IdentityFailure {
    fn from_auth(error: auth::AuthError) -> Self {
        let code = match error {
            auth::AuthError::StaleSessionRevision => IdentityFailureCode::StaleSessionRevision,
            auth::AuthError::NotAuthenticated => IdentityFailureCode::NotAuthenticated,
            auth::AuthError::PatNotConfigured => IdentityFailureCode::PatNotConfigured,
            auth::AuthError::PatActive => IdentityFailureCode::PatActive,
            auth::AuthError::PatPrincipalMismatch => IdentityFailureCode::PatPrincipalMismatch,
            _ => IdentityFailureCode::OperationFailed,
        };
        Self {
            code,
            message: error.to_string(),
        }
    }

    #[cfg(test)]
    fn operation(message: impl Into<String>) -> Self {
        Self {
            code: IdentityFailureCode::OperationFailed,
            message: message.into(),
        }
    }
}

impl std::fmt::Display for IdentityFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

fn cleanup_attempt_label(attempt: &auth::logout::CleanupAttempt) -> &'static str {
    match attempt {
        auth::logout::CleanupAttempt::NotNeeded => "not_needed",
        auth::logout::CleanupAttempt::Succeeded => "succeeded",
        auth::logout::CleanupAttempt::Failed(_) => "failed",
    }
}

fn federation_outcome_label(outcome: &FederationOutcome) -> &'static str {
    match outcome {
        FederationOutcome::Applied(_) => "applied",
        FederationOutcome::OperatorManaged => "operator_managed",
        FederationOutcome::LoggedOut(_) => "logged_out",
        FederationOutcome::Failed(_) => "failed",
        FederationOutcome::Rejected { .. } => "rejected",
        FederationOutcome::Restart { .. } => "restart",
    }
}

fn outcome_requires_namespace_restart(outcome: &FederationOutcome) -> bool {
    matches!(outcome, FederationOutcome::Restart { .. })
        || matches!(
            outcome,
            FederationOutcome::LoggedOut(LogoutOperationOutcome {
                target_namespace: Some(_),
                ..
            })
        )
}

fn outcome_establishes_controller_readiness(outcome: &FederationOutcome) -> bool {
    matches!(
        outcome,
        FederationOutcome::Applied(_)
            | FederationOutcome::OperatorManaged
            | FederationOutcome::LoggedOut(_)
    )
}

/// Owns a blocking resolver after its caller-facing deadline. A late successful
/// resolve may already have atomically published a certificate generation, so
/// explicitly reject its receipt instead of merely detaching and forgetting the
/// task. If this future itself is cancelled during runtime shutdown, dropping
/// the eventual `Resolved` still invokes `IdentityRotation`'s armed guard.
async fn cleanup_late_resolve(
    resolve_task: tokio::task::JoinHandle<std::result::Result<Resolved, IdentityFailure>>,
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
fn applicator_prober(applicator: Arc<dyn IdentityApplicator>) -> Prober {
    Arc::new(move |host, port, _tls, timeout| -> ProbeFuture {
        let applicator = Arc::clone(&applicator);
        Box::pin(async move {
            applicator
                .verify(host, port, _tls, timeout)
                .await
                .map(|_| ())
        })
    })
}

#[cfg(test)]
fn real_prober(messenger: Arc<Mutex<Messenger>>) -> Prober {
    applicator_prober(Arc::new(ManagedIdentityApplicator::new(messenger, None)))
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
/// real [`ManagedIdentityApplicator`], whose mock backend can only ever report
/// operator-managed and so cannot exercise the applied/verify path.
type Federator = Arc<dyn Fn(Option<UpstreamLink>) -> FederateFuture + Send + Sync>;

/// Adapter from the injected poller seam to the explicit router applicator.
fn applicator_federator(applicator: Arc<dyn IdentityApplicator>) -> Federator {
    Arc::new(move |upstream| -> FederateFuture {
        let applicator = Arc::clone(&applicator);
        Box::pin(async move {
            let disposition = match upstream {
                Some(upstream) => applicator.apply(Some(upstream)).await,
                None => applicator.apply_standalone().await,
            }?;
            Ok(matches!(disposition, RouterApplyDisposition::Applied))
        })
    })
}

type StopFuture = Pin<Box<dyn Future<Output = Result<bool>> + Send>>;
type Stopper = Arc<dyn Fn() -> StopFuture + Send + Sync>;

fn applicator_stopper(applicator: Arc<dyn IdentityApplicator>) -> Stopper {
    Arc::new(move || -> StopFuture {
        let applicator = Arc::clone(&applicator);
        Box::pin(async move {
            applicator
                .stop()
                .await
                .map(|disposition| matches!(disposition, RouterApplyDisposition::Applied))
        })
    })
}

type LogoutWorker = Arc<
    dyn Fn(Option<Uuid>) -> std::result::Result<auth::logout::PreparedLogout, IdentityFailure>
        + Send
        + Sync,
>;
type LogoutRouterFence = Arc<dyn Fn() -> std::result::Result<(), IdentityFailure> + Send + Sync>;
type PatPreflight =
    Arc<dyn Fn(&str, &str) -> std::result::Result<(), IdentityFailure> + Send + Sync>;
type RevisionChecker =
    Arc<dyn Fn(Option<Uuid>) -> std::result::Result<(), IdentityFailure> + Send + Sync>;
type TransitionArmer = Arc<
    dyn Fn(Option<Uuid>) -> std::result::Result<IdentitySnapshot, IdentityFailure> + Send + Sync,
>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TransitionArm {
    /// Publish or supersede the durable transition with this exact owner.
    Arm(Option<Uuid>),
    /// Keep the transition established by the preceding OAuth Prepare. This
    /// prevents an older enrollment from overwriting a newer Prepare owner.
    Preserve,
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
    /// The router is external or uses an operator-pinned configuration, so
    /// Peppy changed only its own identity state and makes no claim about the
    /// router's installed configuration.
    OperatorManaged,
    /// Logout completed its fail-closed local transaction. Remote cleanup is
    /// best effort and each outcome is preserved separately for presentation.
    LoggedOut(LogoutOperationOutcome),
    /// The resolve or apply failed; the loop keeps retrying.
    Failed(String),
    /// A command-specific, machine-readable rejection. The detailed message is
    /// retained for daemon logs; the wire adapter emits a bounded public error.
    Rejected {
        code: IdentityFailureCode,
        message: String,
    },
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LogoutRouterState {
    Standalone,
    OperatorManaged,
    Uncertain,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LogoutOperationOutcome {
    pub(crate) certificate_revocation: auth::logout::CleanupAttempt,
    pub(crate) oauth_revocation: auth::logout::CleanupAttempt,
    pub(crate) local_cleanup: auth::logout::CleanupAttempt,
    pub(crate) router: LogoutRouterState,
    pub(crate) operator_action_required: bool,
    pub(crate) target_namespace: Option<Namespace>,
}

/// One control request delivered to the federation loop. Ordinary login/logout
/// asks for a full re-resolve (including namespace-change detection). Only the
/// fail-closed recovery path asks for unconditional standalone, so retained
/// credentials or an older same-subject certificate cannot be reused.
pub(crate) enum IdentityCommand {
    EnrollCurrentCredential {
        expected_session_revision: Option<Uuid>,
        expected_pat_subject: Option<String>,
        expected_api_origin: Option<String>,
        not_after: tokio::time::Instant,
        reply: oneshot::Sender<FederationOutcome>,
    },
    Logout {
        expected_session_revision: Option<Uuid>,
        not_after: tokio::time::Instant,
        reply: oneshot::Sender<FederationOutcome>,
    },
    PrepareOauthLogin {
        expected_session_revision: Uuid,
        not_after: tokio::time::Instant,
        reply: oneshot::Sender<FederationOutcome>,
    },
}

impl IdentityCommand {
    fn cancelled_before_start(&self) -> bool {
        let (not_after, reply_closed) = match self {
            Self::EnrollCurrentCredential {
                not_after, reply, ..
            }
            | Self::Logout {
                not_after, reply, ..
            }
            | Self::PrepareOauthLogin {
                not_after, reply, ..
            } => (*not_after, reply.is_closed()),
        };
        reply_closed || tokio::time::Instant::now() >= not_after
    }

    fn acknowledge_start_deadline(self) {
        let reply = match self {
            Self::EnrollCurrentCredential { reply, .. }
            | Self::Logout { reply, .. }
            | Self::PrepareOauthLogin { reply, .. } => reply,
        };
        let _ = reply.send(FederationOutcome::Rejected {
            code: IdentityFailureCode::DeadlineExceeded,
            message: "identity operation could not start before its admission deadline".into(),
        });
    }
}

pub(crate) type FederationTrigger = IdentityCommand;

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
/// [`IdentityController`] so the poll engine carries no channel plumbing.
struct FederationPoller {
    federator: Federator,
    resolver: Resolver,
    prober: Prober,
    stopper: Stopper,
    logout_worker: Option<LogoutWorker>,
    /// Captures the exact Peppy-spawned zenohd identity before auth writes the
    /// durable logout intent. External routers and pure unit-test pollers have
    /// no process fence.
    logout_router_fence: Option<LogoutRouterFence>,
    pat_preflight: PatPreflight,
    revision_checker: RevisionChecker,
    transition_armer: TransitionArmer,
    /// An adopted external router is never configured, verified, or stopped by
    /// Peppy. This is distinct from a managed router whose config is pinned.
    operator_managed: bool,
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
pub(crate) struct IdentityController {
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
    /// Whether the running Zenoh process was adopted from an operator and is
    /// therefore outside Peppy's router lifecycle.
    initial_operator_managed: bool,
    initial_identity_snapshot: IdentitySnapshot,
}

impl IdentityController {
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
        operator_managed: bool,
        router_process_recorder: Option<RouterProcessRecorder>,
        teardown_token: CancellationToken,
    ) -> (Self, watch::Receiver<FederationStatus>) {
        // The loop's one ambient input, re-read on every poll, derives from the
        // generation's data root: the federation resolve reads the credentials
        // file and the materialized dev TLS under it, and carries the namespace
        // out of the same read.
        let resolver_dirs = peppy_dirs.clone();
        let pat_preflight_api_url = api_url.clone();
        let resolver: Resolver = Arc::new(move |enrollment| {
            if let Some(enrollment) = enrollment.as_ref() {
                let http = auth::http::HttpClient::with_timeout(connect_timeout);
                let rotation = auth::identity::enroll_current_credential(
                    &resolver_dirs,
                    &http,
                    &api_url,
                    auth::resolver::pat_from_env(),
                    &core_node_name,
                    enrollment.expected_session_revision,
                    enrollment.expected_pat_subject.clone(),
                )
                .map_err(IdentityFailure::from_auth)?;
                // The poll below immediately recovers this durable receipt and
                // owns apply/probe/commit. Release only the in-process receipt
                // owner; the binding transition must remain armed until that
                // recovered rotation commits after the real link probe.
                rotation
                    .handoff_to_resolver()
                    .map_err(IdentityFailure::from_auth)?;
            }
            let resolved = if let Some(enrollment) = enrollment.as_ref() {
                auth::router::resolve_federation_target_for_enrollment(
                    &resolver_dirs,
                    &api_url,
                    connect_timeout,
                    &core_node_name,
                    enrollment.expected_session_revision,
                )
                .map_err(IdentityFailure::from_auth)?
            } else {
                auth::router::resolve_federation_target(
                    &resolver_dirs,
                    &api_url,
                    connect_timeout,
                    &core_node_name,
                )
            };
            let identity_snapshot = read_identity_snapshot(&resolver_dirs, resolved.pat_active);
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
                force_standalone: resolved.force_standalone,
                pat_active: resolved.pat_active,
                identity_snapshot,
            })
        });
        let initial_pat_active = auth::resolver::pat_from_env().is_some();
        let initial_identity_snapshot = read_identity_snapshot(&peppy_dirs, initial_pat_active);
        let applicator: Arc<dyn IdentityApplicator> = if operator_managed {
            Arc::new(OperatorManagedIdentityApplicator)
        } else {
            Arc::new(ManagedIdentityApplicator::new(
                messenger,
                router_process_recorder.clone(),
            ))
        };
        let logout_dirs = peppy_dirs.clone();
        let logout_worker: LogoutWorker = Arc::new(move |expected_session_revision| {
            if auth::resolver::pat_from_env().is_some() {
                return Err(IdentityFailure {
                    code: IdentityFailureCode::PatActive,
                    message: "logout is disabled while the daemon service PAT is active".into(),
                });
            }
            let http = auth::http::HttpClient::with_timeout(connect_timeout);
            auth::logout::prepare_logout_current_credential(
                &logout_dirs,
                &http,
                expected_session_revision,
            )
            .map_err(IdentityFailure::from_auth)
        });
        let logout_router_fence: Option<LogoutRouterFence> =
            router_process_recorder.map(|recorder| {
                Arc::new(move || {
                    recorder.capture_current().map_err(|error| IdentityFailure {
                        code: IdentityFailureCode::OperationFailed,
                        message: error.to_string(),
                    })
                }) as LogoutRouterFence
            });
        let pat_preflight_dirs = peppy_dirs.clone();
        let pat_preflight: PatPreflight = Arc::new(move |expected_subject, expected_api_origin| {
            let daemon_api_origin = auth::identity::normalize_api_origin(&pat_preflight_api_url)
                .map_err(IdentityFailure::from_auth)?;
            if daemon_api_origin != expected_api_origin {
                return Err(IdentityFailure {
                    code: IdentityFailureCode::PatOriginMismatch,
                    message: "the CLI and daemon selected different platform API origins".into(),
                });
            }
            let pat = auth::resolver::pat_from_env().ok_or_else(|| IdentityFailure {
                code: IdentityFailureCode::PatNotConfigured,
                message: "the daemon service PEPPY_API_KEY is not configured".into(),
            })?;
            let http = auth::http::HttpClient::with_timeout(connect_timeout);
            let credentials_path = auth::storage::credentials_path(&pat_preflight_dirs);
            let mut credential = auth::resolver::resolve(&credentials_path, &http, Some(pat))
                .map_err(IdentityFailure::from_auth)?;
            let principal = auth::client::get_me(&http, &pat_preflight_api_url, &mut credential)
                .map_err(IdentityFailure::from_auth)?;
            if principal.sub != expected_subject {
                return Err(IdentityFailure {
                    code: IdentityFailureCode::PatPrincipalMismatch,
                    message:
                        "the CLI and daemon PEPPY_API_KEY values belong to different principals"
                            .into(),
                });
            }
            Ok(())
        });
        let revision_dirs = peppy_dirs.clone();
        let revision_checker: RevisionChecker = Arc::new(move |expected| {
            auth::identity::ensure_session_revision_current(&revision_dirs, expected)
                .map_err(IdentityFailure::from_auth)
        });
        let transition_dirs = peppy_dirs.clone();
        let transition_armer: TransitionArmer = Arc::new(move |expected_session_revision| {
            auth::identity::arm_binding_incomplete_for_session(
                &transition_dirs,
                expected_session_revision,
            )
            .map_err(IdentityFailure::from_auth)?;
            Ok(read_identity_snapshot(
                &transition_dirs,
                auth::resolver::pat_from_env().is_some(),
            ))
        });
        let (status_tx, status_rx) = watch::channel(FederationStatus {
            controller_settled: false,
            link: PlatformLink::default(),
            pinned: initial_pinned,
            pat_active: initial_pat_active,
            certificate_error: None,
            certificate_renewing: false,
            operator_managed,
            router_apply_state: if operator_managed || initial_pinned {
                RouterApplyState::OperatorManaged
            } else {
                RouterApplyState::Standalone
            },
            authentication: initial_identity_snapshot.authentication,
            certificate: certificate_state_for(&initial_identity_snapshot, false, None),
            bound_core_node_name: initial_identity_snapshot
                .metadata
                .as_ref()
                .map(|identity| identity.core_node_name.clone()),
            certificate_expiry_unix: initial_identity_snapshot
                .metadata
                .as_ref()
                .map(|identity| identity.not_after),
            generation: initial_identity_snapshot
                .metadata
                .as_ref()
                .map(|identity| identity.active_generation.clone()),
            offline_recovery_required: initial_identity_snapshot.offline_recovery_required,
            ..FederationStatus::default()
        });
        let federation = Self {
            poller: FederationPoller {
                federator: applicator_federator(Arc::clone(&applicator)),
                resolver,
                prober: applicator_prober(Arc::clone(&applicator)),
                stopper: applicator_stopper(applicator),
                logout_worker: Some(logout_worker),
                logout_router_fence,
                pat_preflight,
                revision_checker,
                transition_armer,
                operator_managed,
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
            initial_operator_managed: operator_managed,
            initial_identity_snapshot,
        };
        (federation, status_rx)
    }
}

impl ServeAsyncCommand for IdentityController {
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
    /// Whether this daemon generation completed its initial reconciliation.
    /// This distinguishes authoritative status from the optimistic cache seed
    /// published while the controller is still starting.
    controller_settled: bool,
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
    /// The daemon adopted an external router and therefore never claims
    /// application, verification, or shutdown of that router.
    operator_managed: bool,
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
    identity_snapshot: IdentitySnapshot,
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

    fn federation_status(&self, retry_pending: bool) -> FederationStatus {
        let router_apply_state = if self.operator_managed || self.pinned {
            RouterApplyState::OperatorManaged
        } else {
            match &self.link_state {
                LinkState::Error(_) => RouterApplyState::Error,
                _ if self.endpoint.is_some() => RouterApplyState::Applied,
                _ => RouterApplyState::Standalone,
            }
        };
        FederationStatus {
            controller_settled: self.controller_settled,
            authentication: self.identity_snapshot.authentication,
            certificate: certificate_state_for(
                &self.identity_snapshot,
                self.certificate_renewing,
                self.certificate_error.as_deref(),
            ),
            bound_core_node_name: self
                .identity_snapshot
                .metadata
                .as_ref()
                .map(|identity| identity.core_node_name.clone()),
            certificate_expiry_unix: self
                .identity_snapshot
                .metadata
                .as_ref()
                .map(|identity| identity.not_after),
            generation: self
                .identity_snapshot
                .metadata
                .as_ref()
                .map(|identity| identity.active_generation.clone()),
            next_retry_after_secs: self.timer_after(retry_pending).map(|delay| delay.as_secs()),
            router_apply_state,
            operator_managed: self.operator_managed || self.pinned,
            offline_recovery_required: self.identity_snapshot.offline_recovery_required,
            link: self.platform_link(),
            pinned: self.pinned,
            pat_active: self.pat_active,
            certificate_error: self.certificate_error.clone(),
            certificate_renewing: self.certificate_renewing,
        }
    }
}

impl IdentityController {
    /// Waits for the router to come up, runs the initial federation (firing the
    /// startup gate when it completes or the timeout elapses), then services
    /// immediate login/logout pokes and scheduled certificate/config
    /// maintenance for the daemon's lifetime (the caller races it against the
    /// shutdown signal). This is not a periodic keepalive: the wakeup follows
    /// server deadlines and zenoh owns ordinary reconnects.
    async fn manage(self, ready_tx: oneshot::Sender<()>) {
        let IdentityController {
            poller,
            messaging_ready,
            trigger_rx,
            status_tx,
            restart_tx,
            presence_gate_tx,
            teardown_token: _,
            initial_pinned,
            initial_pat_active,
            initial_operator_managed,
            initial_identity_snapshot,
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
        // an apply and caches the operator-managed rejection.
        let initial = AppliedState {
            last_settled_desired: if initial_pinned {
                SettledDesired::Unsettled
            } else {
                SettledDesired::Standalone
            },
            pinned: initial_pinned,
            pat_active: initial_pat_active,
            operator_managed: initial_operator_managed,
            identity_snapshot: initial_identity_snapshot,
            ..AppliedState::default()
        };
        lifecycle.run(messaging_ready, trigger_rx, initial).await;
    }
}

/// The federation lifecycle after setup: the poll engine plus the channels its
/// phases share. Split from [`IdentityController::manage`] so the phases can
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
        let mut retry_pending = matches!(&initial_outcome, FederationOutcome::Failed(_));
        applied.controller_settled = outcome_establishes_controller_readiness(&initial_outcome);
        self.publish_status(&applied, retry_pending);
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
            if matches!(&work, Work::Poke(trigger) if trigger.cancelled_before_start()) {
                warn!(
                    event = "identity_command_cancelled_before_start",
                    "identity controller: discarded a queued command after its client deadline"
                );
                if let Work::Poke(trigger) = work {
                    trigger.acknowledge_start_deadline();
                }
                continue;
            }
            let (outcome, ack) = match work {
                Work::Poke(FederationTrigger::EnrollCurrentCredential {
                    expected_session_revision,
                    expected_pat_subject,
                    expected_api_origin,
                    not_after: _,
                    reply: ack,
                }) => (
                    self.poller
                        .enroll_and_apply(
                            &mut applied,
                            expected_session_revision,
                            expected_pat_subject,
                            expected_api_origin,
                        )
                        .await,
                    Some(ack),
                ),
                Work::Poke(FederationTrigger::PrepareOauthLogin {
                    expected_session_revision,
                    not_after: _,
                    reply: ack,
                }) => (
                    self.poller
                        .prepare_oauth_login(&mut applied, expected_session_revision)
                        .await,
                    Some(ack),
                ),
                Work::Poke(FederationTrigger::Logout {
                    expected_session_revision,
                    not_after: _,
                    reply: ack,
                }) => (
                    self.poller
                        .logout(&mut applied, expected_session_revision)
                        .await,
                    Some(ack),
                ),
                Work::Timer => (self.poller.poll_and_apply(&mut applied, false).await, None),
            };
            retry_pending = match &outcome {
                FederationOutcome::Failed(_) => true,
                FederationOutcome::LoggedOut(LogoutOperationOutcome {
                    router: LogoutRouterState::Uncertain,
                    ..
                }) => true,
                FederationOutcome::Applied(_)
                | FederationOutcome::OperatorManaged
                | FederationOutcome::LoggedOut(_) => false,
                FederationOutcome::Rejected { .. } => {
                    retry_pending || applied.identity_snapshot.binding_incomplete
                }
                FederationOutcome::Restart { .. } => retry_pending,
            };
            applied.controller_settled |= outcome_establishes_controller_readiness(&outcome);
            self.publish_status(&applied, retry_pending);
            if let Some(ack) = ack {
                // Ordinarily the control handler owns restart ordering so it
                // can flush the structured response first. If its deadline
                // already elapsed, the dropped receiver must not suppress a
                // namespace-safety restart that the completed operation now
                // requires.
                let restart_if_unreceived = outcome_requires_namespace_restart(&outcome);
                if ack.send(outcome).is_err() && restart_if_unreceived {
                    let _ = self.restart_tx.send(true);
                    return;
                }
            } else if timer_work && matches!(&outcome, FederationOutcome::Restart { .. }) {
                let _ = self.restart_tx.send(true);
                return;
            }
        }
    }

    /// Publishes the cached status the control socket answers status queries
    /// from: the platform link now in effect plus router-config ownership.
    fn publish_status(&self, applied: &AppliedState, retry_pending: bool) {
        self.status_tx
            .send_replace(applied.federation_status(retry_pending));
    }
}

impl FederationPoller {
    /// Atomically fences the OAuth handoff against the daemon's immutable
    /// service-authentication mode. A daemon restarted with a PAT while the
    /// browser flow was pending rejects before arming the binding marker or
    /// touching a healthy PAT-backed router.
    async fn prepare_oauth_login(
        &self,
        applied: &mut AppliedState,
        expected_session_revision: Uuid,
    ) -> FederationOutcome {
        if applied.pat_active {
            return FederationOutcome::Rejected {
                code: IdentityFailureCode::PatActive,
                message: "OAuth login is disabled while the daemon service PAT is active".into(),
            };
        }
        self.force_standalone_with_transition(
            applied,
            TransitionArm::Arm(Some(expected_session_revision)),
        )
        .await
    }

    /// Owns the complete normal logout transaction: best-effort remote
    /// revocation while auth is present, managed-router de-federation (or
    /// last-resort stop), then fail-closed local deletion under the same
    /// maintenance lease.
    async fn logout(
        &self,
        applied: &mut AppliedState,
        expected_session_revision: Option<Uuid>,
    ) -> FederationOutcome {
        if applied.pat_active {
            return FederationOutcome::Rejected {
                code: IdentityFailureCode::PatActive,
                message: "logout is disabled while a daemon service PAT is active".into(),
            };
        }

        let Some(logout_worker) = self.logout_worker.clone() else {
            return FederationOutcome::Failed(
                "identity controller has no logout implementation".into(),
            );
        };
        info!(
            event = "identity_logout_attempt",
            "identity controller: starting logout"
        );
        if let Some(router_fence) = self.logout_router_fence.clone() {
            match tokio::task::spawn_blocking(move || router_fence()).await {
                Ok(Ok(())) => {}
                Ok(Err(failure)) => {
                    return FederationOutcome::Rejected {
                        code: failure.code,
                        message: failure.message,
                    };
                }
                Err(error) => {
                    warn!(error = %error, "identity controller: router process-fence task panicked");
                    return FederationOutcome::Failed(
                        "managed-router logout process fence task panicked".into(),
                    );
                }
            }
        }
        // Backend requests carry their own bounded HTTP timeouts. Do not abandon
        // the transaction at the shorter control-connection deadline: the
        // caller may time out, but the serialized controller must still take
        // the router standalone and complete fail-closed local deletion.
        let prepared =
            match tokio::task::spawn_blocking(move || logout_worker(expected_session_revision))
                .await
            {
                Ok(Ok(prepared)) => prepared,
                Ok(Err(failure)) => {
                    if failure.code == IdentityFailureCode::StaleSessionRevision {
                        warn!(
                            event = "identity_stale_session_revision",
                            "identity controller: rejected stale logout revision"
                        );
                    }
                    return FederationOutcome::Rejected {
                        code: failure.code,
                        message: failure.message,
                    };
                }
                Err(error) => {
                    warn!(error = %error, "identity controller: logout preparation task panicked");
                    return FederationOutcome::Failed("logout preparation task panicked".into());
                }
            };
        info!(
            event = "identity_logout_remote_cleanup",
            certificate_revocation = cleanup_attempt_label(prepared.certificate_revocation()),
            oauth_revocation = cleanup_attempt_label(prepared.oauth_revocation()),
            "identity controller: remote logout cleanup settled"
        );

        let was_pinned = applied.pinned;
        let router = if self.operator_managed {
            LogoutRouterState::OperatorManaged
        } else {
            let standalone = tokio::time::timeout(self.apply_timeout, (self.federator)(None)).await;
            match standalone {
                Ok(Ok(true)) => LogoutRouterState::Standalone,
                Ok(Ok(false)) | Ok(Err(_)) | Err(_) => {
                    warn!(
                        event = "identity_logout_router_stop_attempt",
                        "identity controller: standalone apply did not settle; attempting to stop the managed router"
                    );
                    match tokio::time::timeout(self.apply_timeout, (self.stopper)()).await {
                        Ok(Ok(true)) => LogoutRouterState::Standalone,
                        Ok(Ok(false)) | Ok(Err(_)) | Err(_) => LogoutRouterState::Uncertain,
                    }
                }
            }
        };
        let operator_action_required =
            self.operator_managed || was_pinned || router == LogoutRouterState::Uncertain;
        Self::record_logout_router_state(applied, router);

        // Local deletion remains blocking durable filesystem work. Keep the
        // controller serialized until it finishes; the control connection has
        // its own total deadline and a late completion cannot race a new
        // command through this loop.
        let local_task = tokio::task::spawn_blocking(move || prepared.finish_local_cleanup());
        let local = match local_task.await {
            Ok(Ok(outcome)) => outcome,
            Ok(Err(error)) => {
                let failure = IdentityFailure::from_auth(error);
                if failure.code != IdentityFailureCode::StaleSessionRevision {
                    self.mark_logout_cleanup_debt(applied).await;
                }
                return FederationOutcome::Rejected {
                    code: failure.code,
                    message: failure.message,
                };
            }
            Err(error) => {
                warn!(error = %error, "identity controller: local logout cleanup task panicked");
                self.mark_logout_cleanup_debt(applied).await;
                return FederationOutcome::Failed("local logout cleanup task panicked".into());
            }
        };

        let local_succeeded =
            matches!(local.local_cleanup, auth::logout::CleanupAttempt::Succeeded);
        let target_namespace =
            (local_succeeded && !self.startup_namespace.is_local()).then(Namespace::local);
        if local_succeeded {
            applied.identity_snapshot = IdentitySnapshot::default();
            applied.certificate_expires_at = None;
            applied.certificate_error = None;
            applied.renewal_failures = 0;
            applied.pat_active = false;
        } else {
            self.mark_logout_cleanup_debt(applied).await;
        }
        info!(
            event = "identity_logout_outcome",
            local_cleanup = cleanup_attempt_label(&local.local_cleanup),
            router_state = ?router,
            "identity controller: logout settled"
        );
        FederationOutcome::LoggedOut(LogoutOperationOutcome {
            certificate_revocation: local.certificate_revocation,
            oauth_revocation: local.oauth_revocation,
            local_cleanup: local.local_cleanup,
            router,
            operator_action_required,
            target_namespace,
        })
    }

    fn record_logout_router_state(applied: &mut AppliedState, router: LogoutRouterState) {
        applied.next_maintenance = None;
        applied.certificate_renewing = false;
        applied.expiry_defederation_pending = false;
        applied.pinned = false;
        match router {
            LogoutRouterState::Standalone => {
                applied.endpoint = None;
                applied.link_state = LinkState::NotConfigured;
                applied.last_settled_desired = SettledDesired::Standalone;
                applied.needs_reapply = false;
            }
            LogoutRouterState::OperatorManaged => {
                applied.endpoint = None;
                applied.link_state = LinkState::Unverified;
                applied.last_settled_desired = SettledDesired::Unsettled;
                applied.operator_managed = true;
                applied.needs_reapply = false;
            }
            LogoutRouterState::Uncertain => {
                applied.link_state =
                    LinkState::Error("managed router shutdown is uncertain after logout".into());
                applied.last_settled_desired = SettledDesired::Unsettled;
                applied.needs_reapply = true;
            }
        }
    }

    async fn mark_logout_cleanup_debt(&self, applied: &mut AppliedState) {
        let transition_armer = Arc::clone(&self.transition_armer);
        match tokio::task::spawn_blocking(move || transition_armer(None)).await {
            Ok(Ok(snapshot)) => applied.identity_snapshot = snapshot,
            Ok(Err(error)) => warn!(
                error = %error,
                "identity controller: could not durably arm failed-logout recovery"
            ),
            Err(error) => warn!(
                error = %error,
                "identity controller: failed-logout recovery task panicked"
            ),
        }
        applied.identity_snapshot.offline_recovery_required = true;
        applied.certificate_error =
            Some("local logout cleanup did not complete; offline recovery is required".into());
    }

    /// Applies intentional standalone without consulting credentials or the
    /// identity resolver. This is distinct from a normal refederation poke:
    /// post-login failure deliberately retains OAuth/PAT state for retry, and
    /// re-resolving it could otherwise reuse a same-subject prior certificate.
    async fn force_standalone_with_transition(
        &self,
        applied: &mut AppliedState,
        transition: TransitionArm,
    ) -> FederationOutcome {
        let transition_failure = match transition {
            TransitionArm::Arm(expected_session_revision) => {
                let transition_armer = Arc::clone(&self.transition_armer);
                match tokio::task::spawn_blocking(move || {
                    transition_armer(expected_session_revision)
                })
                .await
                {
                    Ok(Ok(snapshot)) => {
                        applied.identity_snapshot = snapshot;
                        info!(
                            event = "identity_binding_transition_armed",
                            "identity controller: durable fail-closed login transition armed"
                        );
                        None
                    }
                    Ok(Err(failure)) => Some(failure),
                    Err(error) => Some(IdentityFailure {
                        code: IdentityFailureCode::OperationFailed,
                        message: format!("fail-closed login transition task panicked: {error}"),
                    }),
                }
            }
            TransitionArm::Preserve => None,
        };
        if transition_failure.is_none()
            && matches!(applied.last_settled_desired, SettledDesired::Standalone)
            && !applied.needs_reapply
            && !applied.pinned
            && !self.operator_managed
        {
            return FederationOutcome::Applied(applied.platform_link());
        }
        let apply_outcome = match tokio::time::timeout(self.apply_timeout, (self.federator)(None))
            .await
        {
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
                applied.pinned = !self.operator_managed;
                applied.operator_managed = self.operator_managed;
                applied.needs_reapply = true;
                FederationOutcome::OperatorManaged
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
        };
        if let Some(mut failure) = transition_failure {
            applied.identity_snapshot.offline_recovery_required = true;
            applied.certificate_error = Some(failure.message.clone());
            match &apply_outcome {
                FederationOutcome::Applied(_) => {
                    failure
                        .message
                        .push_str("; the managed router was nevertheless forced standalone");
                }
                FederationOutcome::OperatorManaged => {
                    failure.message.push_str(
                        "; router configuration is operator-managed and could not be changed",
                    );
                }
                FederationOutcome::Failed(error) => {
                    failure
                        .message
                        .push_str(&format!("; emergency standalone also failed: {error}"));
                }
                _ => {}
            }
            return FederationOutcome::Rejected {
                code: failure.code,
                message: failure.message,
            };
        }
        apply_outcome
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
        self.poll_and_apply_inner(applied, verify, None).await
    }

    async fn enroll_and_apply(
        &self,
        applied: &mut AppliedState,
        expected_session_revision: Option<Uuid>,
        expected_pat_subject: Option<String>,
        expected_api_origin: Option<String>,
    ) -> FederationOutcome {
        let authentication = if expected_session_revision.is_some() {
            "oauth"
        } else {
            "pat"
        };
        info!(
            event = "identity_enrollment_attempt",
            authentication, "identity controller: explicit enrollment started"
        );
        if expected_session_revision.is_some() && applied.pat_active {
            return FederationOutcome::Rejected {
                code: IdentityFailureCode::PatActive,
                message: "OAuth enrollment is disabled while the daemon service PAT is active"
                    .into(),
            };
        }
        if expected_session_revision.is_none() {
            let Some(expected_subject) = expected_pat_subject.as_deref() else {
                return FederationOutcome::Rejected {
                    code: IdentityFailureCode::OperationFailed,
                    message: "PAT enrollment is missing its validated CLI principal".into(),
                };
            };
            let Some(expected_api_origin) = expected_api_origin.as_deref() else {
                return FederationOutcome::Rejected {
                    code: IdentityFailureCode::OperationFailed,
                    message: "PAT enrollment is missing its validated CLI API origin".into(),
                };
            };
            let preflight = Arc::clone(&self.pat_preflight);
            let expected_subject = expected_subject.to_owned();
            let expected_api_origin = expected_api_origin.to_owned();
            match tokio::task::spawn_blocking(move || {
                preflight(&expected_subject, &expected_api_origin)
            })
            .await
            {
                Ok(Ok(())) => {}
                Ok(Err(failure)) => {
                    return FederationOutcome::Rejected {
                        code: failure.code,
                        message: failure.message,
                    };
                }
                Err(error) => {
                    return FederationOutcome::Failed(format!(
                        "daemon PAT preflight task panicked: {error}"
                    ));
                }
            }
        }
        if let Some(expected) = expected_session_revision
            && let Err(failure) = (self.revision_checker)(Some(expected))
        {
            warn!(
                event = "identity_stale_session_revision",
                "identity controller: rejected stale enrollment before changing router state"
            );
            let outcome = FederationOutcome::Rejected {
                code: failure.code,
                message: failure.message,
            };
            info!(
                event = "identity_enrollment_outcome",
                authentication,
                outcome = federation_outcome_label(&outcome),
                "identity controller: explicit enrollment settled"
            );
            return outcome;
        }
        // Enforce the durable fail-closed transition in the controller too,
        // even if a local client skipped the normal pre-publication handshake.
        // This makes resolver errors, panics, timeouts, and late completion all
        // start from standalone rather than preserving a prior login's link.
        let transition = if expected_session_revision.is_some() {
            TransitionArm::Preserve
        } else {
            TransitionArm::Arm(None)
        };
        let outcome = match self
            .force_standalone_with_transition(applied, transition)
            .await
        {
            FederationOutcome::Applied(_) | FederationOutcome::OperatorManaged => {
                self.poll_and_apply_inner(
                    applied,
                    true,
                    Some(EnrollmentRequest {
                        expected_session_revision,
                        expected_pat_subject,
                    }),
                )
                .await
            }
            failure => failure,
        };
        info!(
            event = "identity_enrollment_outcome",
            authentication,
            outcome = federation_outcome_label(&outcome),
            "identity controller: explicit enrollment settled"
        );
        outcome
    }

    async fn poll_and_apply_inner(
        &self,
        applied: &mut AppliedState,
        verify: bool,
        enrollment: Option<EnrollmentRequest>,
    ) -> FederationOutcome {
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
        let explicit_enrollment = enrollment.is_some();
        let mut resolve_task = tokio::task::spawn_blocking(move || resolver(enrollment));
        let mut resolved = match tokio::time::timeout_at(resolve_deadline, &mut resolve_task).await
        {
            Ok(Ok(Ok(t))) => t,
            Ok(Ok(Err(failure))) => {
                warn!(error = %failure, "identity controller: desired-state resolve failed; will retry");
                if explicit_enrollment {
                    // Authentication changed but its new binding could not be
                    // established. Never leave the previous link in effect.
                    let _ = self
                        .force_standalone_with_transition(applied, TransitionArm::Preserve)
                        .await;
                    return FederationOutcome::Rejected {
                        code: failure.code,
                        message: failure.message,
                    };
                }
                return self.fail_resolve_or_expire(applied, failure.message).await;
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
                event = "identity_renewal_outcome",
                outcome = "failed",
                error = %error,
                retry_after = ?applied.renewal_retry_delay(),
                "router federation: certificate maintenance failed; a still-valid generation remains active while renewal backs off"
            );
        } else {
            applied.renewal_failures = 0;
            applied.certificate_error = None;
        }
        let mut rotation = resolved.rotation.take();
        let mut resolved_identity_snapshot = resolved.identity_snapshot.clone();
        applied.pat_active = resolved.pat_active;
        if resolved.force_standalone {
            let fail_closed_reason = resolved.resolve_error.take();
            let standalone = self
                .force_standalone_with_transition(applied, TransitionArm::Preserve)
                .await;
            return match (standalone, fail_closed_reason) {
                (FederationOutcome::Applied(_), Some(reason))
                | (FederationOutcome::OperatorManaged, Some(reason)) => {
                    FederationOutcome::Failed(reason)
                }
                (settled @ FederationOutcome::Applied(_), None)
                | (settled @ FederationOutcome::OperatorManaged, None) => settled,
                (failure, _) => failure,
            };
        }
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
            info!(
                event = "identity_rotation_attempt",
                operation = if explicit_enrollment {
                    "enrollment"
                } else {
                    "renewal"
                },
                "identity controller: staged a fresh certificate generation"
            );
        }
        if let Some(error) = resolved.resolve_error.take() {
            // A transient control-plane failure is not a desired standalone
            // state. Preserve the currently applied valid link and retry; at
            // startup the applied state is already standalone, so this remains
            // fail closed without needlessly tearing down a healthy old link.
            return self.fail_resolve_or_expire(applied, error).await;
        }

        // Fence the exact session before either a router apply or a durable
        // namespace-restart handoff. The latter must not retain a receipt for a
        // login that was replaced after issuance.
        if let Some(active_rotation) = rotation.as_ref()
            && let Err(failure) =
                (self.revision_checker)(active_rotation.activated().session_revision)
        {
            return self
                .reject_stale_session_rotation(rotation.take(), applied, failure)
                .await;
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
                // `retain_for_restart` also clears the transition by comparing
                // its activated session revision. A newer concurrent Prepare
                // therefore survives this older namespace handoff.
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
            applied.identity_snapshot = resolved_identity_snapshot.clone();
            applied.certificate_expires_at = resolved_certificate_deadline;
            if self.operator_managed {
                applied.operator_managed = true;
                return FederationOutcome::OperatorManaged;
            }
            if applied.pinned {
                return FederationOutcome::OperatorManaged;
            }
            // Equality includes the immutable generation-specific TLS paths,
            // so only an actually unchanged applied generation may refresh its
            // validated absolute-expiry projection.
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
            let apply_started = std::time::Instant::now();
            let apply_result =
                tokio::time::timeout_at(apply_deadline, (self.federator)(desired_link)).await;
            let apply_latency_ms = apply_started.elapsed().as_millis() as u64;
            match apply_result {
                Err(_elapsed) => {
                    warn!(
                        event = "identity_router_apply",
                        outcome = "timed_out",
                        latency_ms = apply_latency_ms,
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
                        event = "identity_router_apply",
                        outcome = "applied",
                        latency_ms = apply_latency_ms,
                        endpoint = ?desired_endpoint,
                        "router federation: applied the desired platform upstream"
                    );
                    *applied = AppliedState {
                        controller_settled: applied.controller_settled,
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
                        operator_managed: applied.operator_managed,
                        needs_reapply: false,
                        next_maintenance: applied.next_maintenance,
                        certificate_expires_at: resolved_certificate_deadline,
                        expiry_defederation_pending: false,
                        renewal_failures: applied.renewal_failures,
                        pat_active: applied.pat_active,
                        certificate_error: applied.certificate_error.clone(),
                        certificate_renewing: applied.certificate_renewing,
                        identity_snapshot: resolved_identity_snapshot.clone(),
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
                    info!(
                        event = "identity_router_apply",
                        outcome = "operator_managed",
                        latency_ms = apply_latency_ms,
                        "identity controller: router application remains operator-managed"
                    );
                    if self.operator_managed {
                        // Peppy owns the certificate store but not this router.
                        // Complete the local rotation without pruning any old
                        // immutable generation: the operator may still have an
                        // older path installed and only the operator can tell
                        // when it is safe to remove.
                        if let Some(active_rotation) = rotation.as_ref()
                            && let Err(failure) = (self.revision_checker)(
                                active_rotation.activated().session_revision,
                            )
                        {
                            return self
                                .reject_stale_session_rotation(rotation.take(), applied, failure)
                                .await;
                        }
                        if let Some(rotation) = rotation.take() {
                            if let Err(error) = rotation.commit_for_operator_managed_router() {
                                applied.certificate_renewing = false;
                                return FederationOutcome::Failed(format!(
                                    "could not finalize the operator-managed identity: {error}"
                                ));
                            }
                            resolved_identity_snapshot.binding_incomplete = false;
                            info!(
                                event = "identity_rotation_outcome",
                                outcome = "operator_managed",
                                validity_remaining_secs = ?resolved_certificate_deadline.map(
                                    |deadline| deadline
                                        .saturating_duration_since(tokio::time::Instant::now())
                                        .as_secs()
                                ),
                                "identity controller: local certificate rotation committed for an operator-managed router"
                            );
                        }
                        applied.endpoint = None;
                        applied.link_state = if resolved.upstream.is_some() {
                            LinkState::Unverified
                        } else {
                            LinkState::NotConfigured
                        };
                        applied.last_settled_desired =
                            SettledDesired::from_completed(resolved.upstream.clone());
                        applied.pinned = false;
                        applied.operator_managed = true;
                        applied.needs_reapply = false;
                        applied.certificate_expires_at = resolved_certificate_deadline;
                        applied.certificate_renewing = false;
                        applied.identity_snapshot = resolved_identity_snapshot.clone();
                        info!(
                            "identity controller: local identity is ready; the external router remains operator-managed"
                        );
                        return FederationOutcome::OperatorManaged;
                    }
                    // A managed router with a pinned `ZENOH_CONFIG` cannot be
                    // rewritten. Commit Peppy's local generation without
                    // pruning older immutable paths, exactly as for an
                    // external router; the operator decides when/how to load
                    // the new generation and when old paths are unused.
                    warn!(
                        "router federation: the managed router uses an operator-pinned \
                         ZENOH_CONFIG; the desired federation change was not applied"
                    );
                    if let Some(active_rotation) = rotation.as_ref()
                        && let Err(failure) =
                            (self.revision_checker)(active_rotation.activated().session_revision)
                    {
                        return self
                            .reject_stale_session_rotation(rotation.take(), applied, failure)
                            .await;
                    }
                    if let Some(rotation) = rotation.take() {
                        if let Err(error) = rotation.commit_for_operator_managed_router() {
                            applied.certificate_renewing = false;
                            return FederationOutcome::Failed(format!(
                                "could not finalize the pinned-router identity: {error}"
                            ));
                        }
                        resolved_identity_snapshot.binding_incomplete = false;
                    }
                    applied.endpoint = None;
                    applied.link_state = if resolved.upstream.is_some() {
                        LinkState::Unverified
                    } else {
                        LinkState::NotConfigured
                    };
                    applied.last_settled_desired =
                        SettledDesired::from_completed(resolved.upstream.clone());
                    applied.pinned = true;
                    applied.operator_managed = false;
                    applied.needs_reapply = false;
                    applied.certificate_expires_at = resolved_certificate_deadline;
                    applied.certificate_renewing = false;
                    applied.identity_snapshot = resolved_identity_snapshot.clone();
                    return FederationOutcome::OperatorManaged;
                }
                Ok(Err(e)) => {
                    // Leave `applied` unchanged so the next poll retries the apply.
                    warn!(
                        event = "identity_router_apply",
                        outcome = "failed",
                        latency_ms = apply_latency_ms,
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
            let verification_started = std::time::Instant::now();
            let result = probe_with_bound(
                self.prober.clone(),
                backend.endpoint.host().to_string(),
                backend.endpoint.port(),
                backend.tls.clone(),
                PROBE_TIMEOUT,
            )
            .await;
            let verification_latency_ms = verification_started.elapsed().as_millis() as u64;
            match result {
                Ok(()) => {
                    info!(
                        event = "identity_router_verification",
                        outcome = "verified",
                        latency_ms = verification_latency_ms,
                        "identity controller: managed-router link verified"
                    );
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
                        event = "identity_router_verification",
                        outcome = "failed",
                        latency_ms = verification_latency_ms,
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

        if let Some(active_rotation) = rotation.as_ref()
            && let Err(failure) =
                (self.revision_checker)(active_rotation.activated().session_revision)
        {
            return self
                .reject_stale_session_rotation(rotation.take(), applied, failure)
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
                warn!(
                    event = "identity_rotation_outcome",
                    outcome = "finalization_failed",
                    error = %error,
                    "router federation: verified certificate could not be durably finalized; forcing standalone"
                );
                let emergency = self
                    .force_standalone_with_transition(applied, TransitionArm::Preserve)
                    .await;
                return FederationOutcome::Failed(format!(
                    "verified core-node identity could not be durably finalized; emergency standalone outcome: {}",
                    federation_outcome_label(&emergency)
                ));
            }
            resolved_identity_snapshot.binding_incomplete = false;
            applied.renewal_failures = 0;
            applied.certificate_error = None;
            info!(
                event = "identity_rotation_outcome",
                outcome = "committed",
                validity_remaining_secs = ?resolved_certificate_deadline
                    .map(|deadline| deadline.saturating_duration_since(tokio::time::Instant::now()).as_secs()),
                "identity controller: certificate rotation committed after router verification"
            );
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
        applied.identity_snapshot = resolved_identity_snapshot;

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

    async fn reject_stale_session_rotation(
        &self,
        rotation: Option<auth::IdentityRotation>,
        applied: &mut AppliedState,
        failure: IdentityFailure,
    ) -> FederationOutcome {
        warn!(
            event = "identity_stale_session_revision",
            "identity controller: session changed while enrollment was in flight"
        );
        if let Some(rotation) = rotation {
            match rotation.rollback_for_router_restore() {
                Ok(rejected) => {
                    let standalone =
                        tokio::time::timeout(self.apply_timeout, (self.federator)(None)).await;
                    if matches!(standalone, Ok(Ok(true))) {
                        applied.endpoint = None;
                        applied.link_state = LinkState::NotConfigured;
                        applied.last_settled_desired = SettledDesired::Standalone;
                        applied.needs_reapply = false;
                        applied.certificate_expires_at = None;
                        if let Err(error) = rejected.cleanup_after_router_restore() {
                            warn!(
                                error = %error,
                                "identity controller: stale generation rollback left cleanup debt"
                            );
                        }
                    } else {
                        applied.last_settled_desired = SettledDesired::Unsettled;
                        applied.needs_reapply = true;
                        applied.link_state = LinkState::Error(
                            "router state is uncertain after rejecting a stale login".into(),
                        );
                    }
                }
                Err(error) => {
                    applied.needs_reapply = true;
                    applied.link_state = LinkState::Error(
                        "identity rollback failed after rejecting a stale login".into(),
                    );
                    warn!(error = %error, "identity controller: stale generation rollback failed");
                    let emergency = self
                        .force_standalone_with_transition(applied, TransitionArm::Preserve)
                        .await;
                    warn!(
                        outcome = federation_outcome_label(&emergency),
                        "identity controller: attempted emergency standalone after rollback failure"
                    );
                }
            }
        }
        applied.certificate_renewing = false;
        FederationOutcome::Rejected {
            code: failure.code,
            message: failure.message,
        }
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
        warn!(
            event = "identity_expiry_defederation",
            outcome = "attempting",
            "identity controller: certificate reached hard expiry"
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
                applied.pinned = !self.operator_managed;
                applied.operator_managed = self.operator_managed;
                applied.needs_reapply = true;
                if self.operator_managed {
                    failure.push_str(
                        "; the router is operator-managed, so the operator must remove its expired identity",
                    );
                } else {
                    failure.push_str(
                        "; ZENOH_CONFIG is pinned, so automatic de-federation was refused",
                    );
                }
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
            warn!(
                event = "identity_expiry_defederation",
                outcome = "standalone",
                error = %failure,
                "router federation: certificate expired during resolver failure"
            );
            return FederationOutcome::Failed(failure);
        }
        applied.certificate_error = Some(failure.clone());
        warn!(
            event = "identity_expiry_defederation",
            outcome = "uncertain",
            error = %failure,
            "router federation: certificate expired during resolver failure"
        );
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
                let emergency = self
                    .force_standalone_with_transition(applied, TransitionArm::Preserve)
                    .await;
                return FederationOutcome::Failed(format!(
                    "{reason}; core-node certificate rollback also failed: {error}; emergency standalone outcome: {}",
                    federation_outcome_label(&emergency)
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
            event = "identity_rotation_rollback",
            outcome = if prior_router_confirmed {
                "restored"
            } else {
                "uncertain"
            },
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
            status_tx.send_replace(applied.federation_status(false));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::Error;
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
            force_standalone: false,
            pat_active: false,
            identity_snapshot: IdentitySnapshot::default(),
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
            stopper: Arc::new(|| Box::pin(async { Ok(true) })),
            logout_worker: None,
            logout_router_fence: None,
            pat_preflight: Arc::new(|_, _| Ok(())),
            revision_checker: Arc::new(|_| Ok(())),
            transition_armer: Arc::new(|_| {
                Ok(IdentitySnapshot {
                    binding_incomplete: true,
                    ..IdentitySnapshot::default()
                })
            }),
            operator_managed: false,
            connect_timeout: Duration::from_secs(1),
            apply_timeout: APPLY_TIMEOUT,
            startup_namespace: Namespace::local(),
            status_tx: None,
            late_resolve_cleanup: Mutex::new(None),
            finalization_delay: Duration::ZERO,
        }
    }

    /// An `IdentityController` with injected seams and test defaults, plus the
    /// receiving half of its status watch. Tests override fields (gates,
    /// restart signal, poller bounds) before calling `manage`.
    fn federation_under_test(
        federator: Federator,
        resolver: Resolver,
        prober: Prober,
        messaging_ready: watch::Receiver<bool>,
        trigger_rx: TriggerReceiver,
    ) -> (IdentityController, watch::Receiver<FederationStatus>) {
        let (status_tx, status_rx) = watch::channel(FederationStatus::default());
        let federation = IdentityController {
            poller: poller_under_test(federator, resolver, prober),
            messaging_ready,
            trigger_rx,
            status_tx,
            restart_tx: watch::channel(false).0,
            presence_gate_tx: None,
            teardown_token: CancellationToken::new(),
            initial_pinned: false,
            initial_pat_active: false,
            initial_operator_managed: false,
            initial_identity_snapshot: IdentitySnapshot::default(),
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
    /// `refederate` reports no rewrite (`Ok(false)`), so the poll is operator-managed.
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
        let resolver: Resolver = Arc::new(move |_| {
            counter.fetch_add(1, Ordering::SeqCst);
            Ok(Resolved {
                upstream: upstream.clone(),
                namespace: namespace.clone(),
                rotation: None,
                maintenance_after: None,
                certificate_expires_after: None,
                renewal_error: None,
                resolve_error: None,
                force_standalone: false,
                pat_active: false,
                identity_snapshot: IdentitySnapshot::default(),
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
    fn corrupt_or_missing_identity_material_is_fail_closed_in_status() {
        let temp = tempfile::tempdir().unwrap();
        let dirs = PeppyDirs::new(temp.path());
        let now = auth::storage::now_unix();
        let generation = "a".repeat(64);
        let metadata = auth::identity::CoreNodeIdentity {
            api_origin: "https://api.peppy.bot".into(),
            subject: "subject".into(),
            session_revision: None,
            workspace_id: workspace_namespace(),
            core_node_name: "core-node-test".into(),
            active_generation: generation.clone(),
            serial_number: "01".into(),
            spki_sha256: generation,
            not_before: now - 60,
            renew_after: now + 60,
            not_after: now + 120,
        };
        auth::storage::save(
            &auth::storage::credentials_path(&dirs),
            &auth::Credentials {
                core_node_identity: Some(metadata),
                ..Default::default()
            },
        )
        .unwrap();

        let snapshot = read_identity_snapshot(&dirs, true);

        assert!(snapshot.offline_recovery_required);
        assert_eq!(
            certificate_state_for(&snapshot, false, None),
            CertificateState::Error
        );
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
        let resolver: Resolver = Arc::new(move |_| {
            counted.fetch_add(1, Ordering::SeqCst);
            Ok(Resolved {
                upstream: None,
                namespace: Namespace::local(),
                rotation: None,
                maintenance_after: Some(Duration::from_millis(20)),
                certificate_expires_after: None,
                renewal_error: None,
                resolve_error: None,
                force_standalone: false,
                pat_active: false,
                identity_snapshot: IdentitySnapshot::default(),
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

    #[tokio::test]
    async fn expired_queued_enrollment_is_discarded_before_resolve_or_mutation() {
        let (resolver, resolve_calls) = counting_resolver(None);
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
        ready_rx.await.expect("initial reconcile completes");
        assert_eq!(resolve_calls.load(Ordering::SeqCst), 1);

        let (reply, outcome) = oneshot::channel();
        trigger_tx
            .send(IdentityCommand::EnrollCurrentCredential {
                expected_session_revision: None,
                expected_pat_subject: None,
                expected_api_origin: None,
                not_after: tokio::time::Instant::now(),
                reply,
            })
            .await
            .expect("expired command can reach the bounded queue");
        let outcome = tokio::time::timeout(Duration::from_secs(1), outcome)
            .await
            .expect("expired command is acknowledged")
            .expect("controller sends an explicit deadline outcome");
        assert!(matches!(
            outcome,
            FederationOutcome::Rejected {
                code: IdentityFailureCode::DeadlineExceeded,
                ..
            }
        ));

        assert_eq!(
            resolve_calls.load(Ordering::SeqCst),
            1,
            "the expired queued command must not invoke the identity resolver"
        );
        drop(trigger_tx);
        task.await.expect("controller exits after channel close");
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
        Arc::new(move |_| {
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
                force_standalone: false,
                pat_active: false,
                identity_snapshot: IdentitySnapshot::default(),
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
            controller_settled: false,
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
            operator_managed: false,
            identity_snapshot: IdentitySnapshot::default(),
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

        assert_eq!(first, FederationOutcome::OperatorManaged);
        assert_eq!(
            second,
            FederationOutcome::OperatorManaged,
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

        assert_eq!(outcome, FederationOutcome::OperatorManaged);
        assert_eq!(
            applied.endpoint, None,
            "the rejected desired endpoint must not leak into applied state"
        );
        assert_eq!(applied.link_state, LinkState::Unverified);
        assert!(applied.pinned);
        assert_eq!(
            applied.last_settled_desired,
            SettledDesired::Upstream(DesiredBackend {
                endpoint: dial(ENDPOINT),
                tls: pmi::TlsConfig::default(),
            }),
            "the rejected target is cached so an identical repeat stays operator-managed"
        );
    }

    #[tokio::test]
    async fn a_resolver_error_is_failed_and_preserves_applied_state() {
        let resolver: Resolver =
            Arc::new(|_| Err(IdentityFailure::operation("credentials file unreadable")));
        let (federator, calls) = recording_federator();
        let (prober, _) = counting_prober(Ok(()));
        let mut applied = AppliedState {
            controller_settled: false,
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
            operator_managed: false,
            identity_snapshot: IdentitySnapshot::default(),
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
    async fn unrecoverable_receipt_error_forces_standalone_instead_of_preserving_live_link() {
        let resolver: Resolver = Arc::new(|_| {
            Ok(Resolved {
                upstream: None,
                namespace: Namespace::local(),
                rotation: None,
                maintenance_after: None,
                certificate_expires_after: None,
                renewal_error: None,
                resolve_error: Some(
                    "unverified rotation receipt cannot be bound to an authenticated owner".into(),
                ),
                force_standalone: true,
                pat_active: false,
                identity_snapshot: IdentitySnapshot {
                    offline_recovery_required: true,
                    ..IdentitySnapshot::default()
                },
            })
        });
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
            .poll_and_apply(&mut applied, false)
            .await;

        assert!(
            matches!(outcome, FederationOutcome::Failed(ref reason) if reason.contains("cannot be bound"))
        );
        assert_eq!(*calls.lock().unwrap(), vec![None]);
        assert!(applied.endpoint.is_none());
        assert_eq!(applied.link_state, LinkState::NotConfigured);
        assert_eq!(applied.last_settled_desired, SettledDesired::Standalone);
    }

    #[tokio::test]
    async fn transient_resolve_failure_defederates_an_expired_applied_generation() {
        let resolver: Resolver =
            Arc::new(|_| Err(IdentityFailure::operation("issuer unavailable")));
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
        let resolver: Resolver = Arc::new(|_| panic!("forced standalone must not resolve auth"));
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
            .force_standalone_with_transition(&mut applied, TransitionArm::Arm(None))
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

    #[tokio::test]
    async fn oauth_preparation_rejects_daemon_pat_before_any_mutation() {
        let resolver: Resolver = Arc::new(|_| panic!("PAT rejection must not resolve auth"));
        let (federator, router_calls) = recording_federator();
        let (prober, _) = counting_prober(Ok(()));
        let arm_calls = Arc::new(AtomicUsize::new(0));
        let counted_arms = Arc::clone(&arm_calls);
        let mut poller = poller_under_test(federator, resolver, prober);
        poller.transition_armer = Arc::new(move |_| {
            counted_arms.fetch_add(1, Ordering::SeqCst);
            Ok(IdentitySnapshot::default())
        });
        let mut applied = AppliedState {
            endpoint: Some(ENDPOINT.to_string()),
            link_state: LinkState::Verified,
            last_settled_desired: SettledDesired::Upstream(upstream().unwrap()),
            pat_active: true,
            ..AppliedState::default()
        };
        let before = applied.clone();

        let outcome = poller
            .prepare_oauth_login(&mut applied, Uuid::new_v4())
            .await;

        assert!(matches!(
            outcome,
            FederationOutcome::Rejected {
                code: IdentityFailureCode::PatActive,
                ..
            }
        ));
        assert_eq!(applied, before);
        assert_eq!(arm_calls.load(Ordering::SeqCst), 0);
        assert!(router_calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn pat_principal_mismatch_is_rejected_before_router_or_store_mutation() {
        let (federator, router_calls) = recording_federator();
        let (resolver, resolver_calls) = counting_resolver(upstream());
        let (prober, _) = counting_prober(Ok(()));
        let arm_calls = Arc::new(AtomicUsize::new(0));
        let counted_arms = Arc::clone(&arm_calls);
        let mut poller = poller_under_test(federator, resolver, prober);
        poller.pat_preflight = Arc::new(|_, _| {
            Err(IdentityFailure {
                code: IdentityFailureCode::PatPrincipalMismatch,
                message: "different PAT principals".into(),
            })
        });
        poller.transition_armer = Arc::new(move |_| {
            counted_arms.fetch_add(1, Ordering::SeqCst);
            Ok(IdentitySnapshot::default())
        });
        let mut applied = AppliedState {
            endpoint: Some(ENDPOINT.to_string()),
            link_state: LinkState::Verified,
            last_settled_desired: SettledDesired::Upstream(upstream().unwrap()),
            pat_active: true,
            ..AppliedState::default()
        };
        let before = applied.clone();

        let outcome = poller
            .enroll_and_apply(
                &mut applied,
                None,
                Some("cli-subject".into()),
                Some("https://api.peppy.bot".into()),
            )
            .await;

        assert!(matches!(
            outcome,
            FederationOutcome::Rejected {
                code: IdentityFailureCode::PatPrincipalMismatch,
                ..
            }
        ));
        assert_eq!(applied, before);
        assert_eq!(arm_calls.load(Ordering::SeqCst), 0);
        assert_eq!(resolver_calls.load(Ordering::SeqCst), 0);
        assert!(router_calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn stale_enrollment_is_rejected_before_arming_or_changing_router_state() {
        let (federator, router_calls) = recording_federator();
        let (resolver, resolver_calls) = counting_resolver(upstream());
        let (prober, _) = counting_prober(Ok(()));
        let arm_calls = Arc::new(AtomicUsize::new(0));
        let counted_arms = Arc::clone(&arm_calls);
        let mut poller = poller_under_test(federator, resolver, prober);
        poller.transition_armer = Arc::new(move |_| {
            counted_arms.fetch_add(1, Ordering::SeqCst);
            Ok(IdentitySnapshot::default())
        });
        poller.revision_checker = Arc::new(|_| {
            Err(IdentityFailure {
                code: IdentityFailureCode::StaleSessionRevision,
                message: "stale session".into(),
            })
        });
        let mut applied = AppliedState {
            endpoint: Some(ENDPOINT.into()),
            link_state: LinkState::Verified,
            ..AppliedState::default()
        };

        let outcome = poller
            .enroll_and_apply(&mut applied, Some(Uuid::new_v4()), None, None)
            .await;

        assert!(matches!(
            outcome,
            FederationOutcome::Rejected {
                code: IdentityFailureCode::StaleSessionRevision,
                ..
            }
        ));
        assert_eq!(arm_calls.load(Ordering::SeqCst), 0);
        assert_eq!(resolver_calls.load(Ordering::SeqCst), 0);
        assert!(router_calls.lock().unwrap().is_empty());
        assert_eq!(applied.endpoint.as_deref(), Some(ENDPOINT));
        assert_eq!(applied.link_state, LinkState::Verified);
    }

    #[tokio::test]
    async fn pinned_logout_reports_operator_cleanup_even_after_stopper_succeeds() {
        let temporary = tempfile::tempdir().unwrap();
        let dirs = PeppyDirs::new(temporary.path());
        let worker_dirs = dirs.clone();
        let (federator, apply_calls) = counting_pinned_federator();
        let (resolver, _) = counting_resolver(None);
        let (prober, _) = counting_prober(Ok(()));
        let mut poller = poller_under_test(federator, resolver, prober);
        poller.logout_worker = Some(Arc::new(move |expected| {
            auth::logout::prepare_logout_current_credential(
                &worker_dirs,
                &auth::http::HttpClient::with_timeout(Duration::from_millis(25)),
                expected,
            )
            .map_err(IdentityFailure::from_auth)
        }));
        let stop_calls = Arc::new(AtomicUsize::new(0));
        let counted_stops = Arc::clone(&stop_calls);
        poller.stopper = Arc::new(move || {
            counted_stops.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { Ok(true) })
        });
        let mut applied = AppliedState {
            endpoint: Some(ENDPOINT.into()),
            link_state: LinkState::Verified,
            pinned: true,
            last_settled_desired: SettledDesired::from_completed(upstream()),
            ..AppliedState::default()
        };

        let outcome = poller.logout(&mut applied, None).await;

        assert!(matches!(
            outcome,
            FederationOutcome::LoggedOut(LogoutOperationOutcome {
                router: LogoutRouterState::Standalone,
                operator_action_required: true,
                local_cleanup: auth::logout::CleanupAttempt::Succeeded,
                ..
            })
        ));
        assert_eq!(apply_calls.load(Ordering::SeqCst), 1);
        assert_eq!(stop_calls.load(Ordering::SeqCst), 1);
        assert!(applied.endpoint.is_none());
        assert_eq!(applied.link_state, LinkState::NotConfigured);
    }

    #[tokio::test]
    async fn slow_remote_logout_still_completes_fail_closed_local_cleanup() {
        let temporary = tempfile::tempdir().unwrap();
        let dirs = PeppyDirs::new(temporary.path());
        let worker_dirs = dirs.clone();
        let (federator, _) = recording_federator();
        let (resolver, _) = counting_resolver(None);
        let (prober, _) = counting_prober(Ok(()));
        let mut poller = poller_under_test(federator, resolver, prober);
        poller.connect_timeout = Duration::from_millis(1);
        poller.logout_worker = Some(Arc::new(move |expected| {
            std::thread::sleep(Duration::from_millis(20));
            auth::logout::prepare_logout_current_credential(
                &worker_dirs,
                &auth::http::HttpClient::with_timeout(Duration::from_millis(1)),
                expected,
            )
            .map_err(IdentityFailure::from_auth)
        }));
        let mut applied = AppliedState {
            endpoint: Some(ENDPOINT.into()),
            link_state: LinkState::Verified,
            last_settled_desired: SettledDesired::from_completed(upstream()),
            ..AppliedState::default()
        };

        let outcome = poller.logout(&mut applied, None).await;

        assert!(matches!(
            outcome,
            FederationOutcome::LoggedOut(LogoutOperationOutcome {
                local_cleanup: auth::logout::CleanupAttempt::Succeeded,
                router: LogoutRouterState::Standalone,
                ..
            })
        ));
        assert!(applied.endpoint.is_none());
        assert_eq!(applied.link_state, LinkState::NotConfigured);
    }

    #[tokio::test]
    async fn logout_cleanup_error_keeps_router_status_fail_closed_and_marks_recovery() {
        let temporary = tempfile::tempdir().unwrap();
        let dirs = PeppyDirs::new(temporary.path());
        let credentials_path = auth::storage::credentials_path(&dirs);
        let worker_dirs = dirs.clone();
        let (resolver, _) = counting_resolver(None);
        let (prober, _) = counting_prober(Ok(()));
        let federator: Federator = Arc::new(move |_| {
            std::fs::create_dir_all(&credentials_path).unwrap();
            Box::pin(async { Ok(true) })
        });
        let mut poller = poller_under_test(federator, resolver, prober);
        poller.logout_worker = Some(Arc::new(move |expected| {
            auth::logout::prepare_logout_current_credential(
                &worker_dirs,
                &auth::http::HttpClient::with_timeout(Duration::from_millis(25)),
                expected,
            )
            .map_err(IdentityFailure::from_auth)
        }));
        let mut applied = AppliedState {
            endpoint: Some(ENDPOINT.into()),
            link_state: LinkState::Verified,
            last_settled_desired: SettledDesired::from_completed(upstream()),
            ..AppliedState::default()
        };

        let outcome = poller.logout(&mut applied, None).await;

        assert!(matches!(
            outcome,
            FederationOutcome::LoggedOut(LogoutOperationOutcome {
                local_cleanup: auth::logout::CleanupAttempt::Failed(_),
                router: LogoutRouterState::Standalone,
                ..
            })
        ));
        assert!(applied.endpoint.is_none());
        assert_eq!(applied.link_state, LinkState::NotConfigured);
        assert_eq!(applied.last_settled_desired, SettledDesired::Standalone);
        assert!(applied.identity_snapshot.binding_incomplete);
        assert!(applied.identity_snapshot.offline_recovery_required);
        assert!(applied.next_maintenance.is_none());
    }

    async fn assert_failed_expiry_defederation_uses_backoff(
        federator: Federator,
        apply_timeout: Duration,
    ) {
        let resolver: Resolver =
            Arc::new(|_| Err(IdentityFailure::operation("issuer unavailable")));
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
        let resolver: Resolver = Arc::new(|_| {
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
        let resolver: Resolver = Arc::new(|_| {
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
        let resolver: Resolver = Arc::new(|_| {
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
        let resolver: Resolver = Arc::new(|_| {
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
        let resolver: Resolver =
            Arc::new(|_| Err(IdentityFailure::operation("backend unavailable")));
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
        let resolver: Resolver = Arc::new(|_| {
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
            .send(FederationTrigger::EnrollCurrentCredential {
                expected_session_revision: None,
                expected_pat_subject: Some("cli-subject".into()),
                expected_api_origin: Some("https://api.peppy.bot".into()),
                not_after: tokio::time::Instant::now() + Duration::from_secs(60),
                reply: ack_tx,
            })
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
            .send(FederationTrigger::EnrollCurrentCredential {
                expected_session_revision: None,
                expected_pat_subject: Some("cli-subject".into()),
                expected_api_origin: Some("https://api.peppy.bot".into()),
                not_after: tokio::time::Instant::now() + Duration::from_secs(60),
                reply: ack_tx,
            })
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
            .send(FederationTrigger::EnrollCurrentCredential {
                expected_session_revision: None,
                expected_pat_subject: Some("cli-subject".into()),
                expected_api_origin: Some("https://api.peppy.bot".into()),
                not_after: tokio::time::Instant::now() + Duration::from_secs(60),
                reply: ack_tx,
            })
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
    /// reporting operator-managed on a repeat and must not publish the desired endpoint
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
        assert_eq!(status.link.link_state, LinkState::Unverified);
        assert!(status.pinned);

        let (ack_tx, ack_rx) = oneshot::channel();
        trigger_tx
            .send(FederationTrigger::EnrollCurrentCredential {
                expected_session_revision: None,
                expected_pat_subject: Some("cli-subject".into()),
                expected_api_origin: Some("https://api.peppy.bot".into()),
                not_after: tokio::time::Instant::now() + Duration::from_secs(60),
                reply: ack_tx,
            })
            .await
            .expect("trigger accepted");
        let outcome = tokio::time::timeout(Duration::from_secs(1), ack_rx)
            .await
            .expect("poke serviced immediately")
            .expect("ack sender not dropped");

        assert_eq!(
            outcome,
            FederationOutcome::OperatorManaged,
            "an identical repeat of a pinned target must stay operator-managed, not Applied"
        );
        assert_eq!(
            probe_calls.load(Ordering::SeqCst),
            0,
            "a pinned outcome is never probed"
        );
        assert_eq!(
            apply_calls.load(Ordering::SeqCst),
            3,
            "startup applies once, then explicit enrollment must attempt fail-closed standalone before retrying the pinned target"
        );

        drop(messaging_tx);
        task.abort();
    }

    /// Once the local router is ready, the startup gate fires within the resolve
    /// timeout even when the backend is slow enough to blow the bound. The
    /// federation loop then keeps retrying.
    #[tokio::test]
    async fn startup_gate_fires_within_timeout_when_resolve_is_slow() {
        let resolver: Resolver = Arc::new(|_| {
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
        let resolver: Resolver = Arc::new(move |_| {
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
            .send(FederationTrigger::EnrollCurrentCredential {
                expected_session_revision: None,
                expected_pat_subject: Some("cli-subject".into()),
                expected_api_origin: Some("https://api.peppy.bot".into()),
                not_after: tokio::time::Instant::now() + Duration::from_secs(60),
                reply: ack_tx,
            })
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

    #[tokio::test]
    async fn dropped_namespace_change_ack_still_requests_restart() {
        let calls = AtomicUsize::new(0);
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let release_rx = std::sync::Mutex::new(release_rx);
        let desired = upstream();
        let resolver: Resolver = Arc::new(move |_| {
            let namespace = if calls.fetch_add(1, Ordering::SeqCst) == 0 {
                Namespace::local()
            } else {
                started_tx.send(()).expect("announce active resolve");
                release_rx
                    .lock()
                    .unwrap()
                    .recv()
                    .expect("release active resolve");
                workspace_namespace()
            };
            Ok(Resolved {
                upstream: desired.clone(),
                namespace,
                rotation: None,
                maintenance_after: None,
                certificate_expires_after: None,
                renewal_error: None,
                resolve_error: None,
                force_standalone: false,
                pat_active: false,
                identity_snapshot: IdentitySnapshot::default(),
            })
        });
        let (prober, _) = counting_prober(Ok(()));
        let (_messaging_tx, messaging_rx) = watch::channel(true);
        let (trigger_tx, trigger_rx) = mpsc::channel(8);
        let (ready_tx, ready_rx) = oneshot::channel();
        let (restart_tx, mut restart_rx) = watch::channel(false);

        let (mut federation, _status_rx) = federation_under_test(
            applying_federator(),
            resolver,
            prober,
            messaging_rx,
            trigger_rx,
        );
        federation.restart_tx = restart_tx;
        let task = tokio::spawn(federation.manage(ready_tx));
        ready_rx.await.expect("startup gate");

        let (ack_tx, ack_rx) = oneshot::channel();
        trigger_tx
            .send(FederationTrigger::EnrollCurrentCredential {
                expected_session_revision: None,
                expected_pat_subject: Some("cli-subject".into()),
                expected_api_origin: Some("https://api.peppy.bot".into()),
                not_after: tokio::time::Instant::now() + Duration::from_secs(60),
                reply: ack_tx,
            })
            .await
            .expect("trigger accepted");
        tokio::task::spawn_blocking(move || {
            started_rx
                .recv_timeout(Duration::from_secs(1))
                .expect("controller begins the namespace-changing resolve")
        })
        .await
        .expect("start waiter does not panic");
        drop(ack_rx);
        release_tx.send(()).expect("finish active resolve");

        tokio::time::timeout(
            Duration::from_secs(1),
            restart_rx.wait_for(|restart| *restart),
        )
        .await
        .expect("late completed operation requests restart")
        .expect("restart sender remains live");
        tokio::time::timeout(Duration::from_secs(1), task)
            .await
            .expect("controller exits after requesting restart")
            .expect("controller task does not panic");
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
