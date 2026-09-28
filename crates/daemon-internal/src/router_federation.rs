//! Services the control-socket pokes `peppy platform enroll` and `unenroll`
//! send after they change this machine's enrollment.
//!
//! The router itself is built federated, or standalone, by the builder from
//! the enrollment on disk ([`auth::enrollment`]): a machine enrolled in a
//! platform project runs its zenohd under the platform-minted id, dials the
//! project's cloud router over mutual TLS with the enrolled certificate, and
//! opens its application sessions under the project namespace. Both the id
//! and the namespace are fixed for the life of a generation, so a change to
//! the enrollment is never applied to a running router: this task compares
//! what is on disk with what this generation booted under and asks for a
//! generation restart when they differ. The control handler flushes that ack
//! and only then raises the restart, so the CLI always reads it.
//!
//! When nothing changed, a poke verifies the link instead: a real TLS
//! handshake to the cloud router, presenting the enrolled certificate
//! ([`pmi::probe_tls_reachable`]), so an expired leaf, an unknown issuer or an
//! unreachable router is reported as [`FederationOutcome::Unreachable`] rather
//! than as a silent reconnect loop. There is no timer and no HTTP: the router
//! holds its own link open (`reconnect: true`), and the daemon never holds a
//! bearer token.

use crate::serve::{ServeAsyncCommand, ServeAsyncHandle};
use auth::Enrollment;
use config::namespace::Namespace;
use daemon_config::consts::PeppyDirs;
use pmi::RouterId;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

/// How long a poke waits for the cloud router's TLS handshake to validate.
/// A healthy handshake is sub-second, so a tight bound keeps the whole poke
/// inside the daemon's ack budget ([`super::federation_control`]'s
/// `ACK_BUDGET`); an unreachable or firewalled router fails the probe within
/// this bound and surfaces promptly as [`FederationOutcome::Unreachable`]
/// rather than as a daemon-side ack timeout.
pub(crate) const PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// Reads the enrollment on disk. A boxed closure so tests can inject a
/// deterministic value in place of the real (file-backed) `enrollment::load`.
type EnrollmentReader = Arc<dyn Fn() -> auth::Result<Option<Enrollment>> + Send + Sync>;

/// The future a [`Prober`] returns: `Ok(())` if the upstream's TLS link
/// validates, `Err(reason)` (human-readable) otherwise.
type ProbeFuture = Pin<Box<dyn Future<Output = std::result::Result<(), String>> + Send>>;

/// Verifies that the federation link to `host:port` validates with a real TLS
/// handshake presenting the enrolled identity. A boxed async closure so tests
/// can inject a deterministic probe (success/failure + a call counter) in
/// place of the real [`pmi::probe_tls_reachable`], which does network I/O.
type Prober = Arc<dyn Fn(String, u16, pmi::TlsConfig, Duration) -> ProbeFuture + Send + Sync>;

/// The current unix time. Injected so the expiry check is testable without
/// depending on the host clock.
type Clock = Arc<dyn Fn() -> i64 + Send + Sync>;

/// The real prober: a raw TLS handshake against the cloud router (see
/// [`pmi::probe_tls_reachable`]).
fn real_prober() -> Prober {
    Arc::new(|host, port, tls, timeout| -> ProbeFuture {
        Box::pin(async move { pmi::probe_tls_reachable(&host, port, &tls, timeout).await })
    })
}

/// The identity a daemon generation runs under: the namespace its sessions
/// open with and, when enrolled, the platform-minted id its router pins.
/// Compared against the enrollment on disk to decide whether a poke can be
/// answered live or needs a generation restart. An unenrolled generation runs
/// a per-boot router id that nothing on disk names, so it carries `None`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FederationIdentity {
    pub(crate) namespace: Namespace,
    pub(crate) zenoh_id: Option<RouterId>,
}

impl FederationIdentity {
    /// The identity `enrollment` prescribes: the project namespace and zid when
    /// enrolled, `local` and no pinned id otherwise.
    pub(crate) fn of(enrollment: Option<&Enrollment>) -> Self {
        match enrollment {
            Some(enrollment) => Self {
                namespace: enrollment.document.namespace.clone(),
                zenoh_id: Some(enrollment.document.zenoh_id.clone()),
            },
            None => Self {
                namespace: Namespace::local(),
                zenoh_id: None,
            },
        }
    }
}

/// Outcome of one poke, reported back over the control socket so the CLI can
/// tell the user the daemon's federation state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum FederationOutcome {
    /// The router runs what the enrollment prescribes: `Some(locator)` dialing
    /// the cloud router at `locator`, with the mutual-TLS link verified;
    /// `None` not enrolled, so a standalone router under `local`.
    Applied(Option<String>),
    /// The managed router uses an operator-pinned `ZENOH_CONFIG`, so its
    /// federation belongs to the operator and was neither rendered nor probed.
    Pinned,
    /// The enrollment on disk could not be read (present but malformed, or
    /// missing its material), so nothing can be said about the link.
    Failed(String),
    /// The router is enrolled, but the mutual-TLS link to the cloud router does
    /// not validate: an expired leaf, an issuer the router does not trust, or
    /// a router that is down or unreachable.
    Unreachable(String),
    /// The enrollment on disk prescribes a different identity (namespace or
    /// pinned id) than this generation booted under. Neither can change on a
    /// live router or session, so the generation restarts; the control handler
    /// owns triggering it after flushing the ack.
    Restart,
}

/// The injectable collaborators one poke needs: the enrollment on disk, the
/// link probe, and the clock the expiry check reads.
pub(crate) struct FederationDeps {
    pub(crate) enrollment: EnrollmentReader,
    pub(crate) prober: Prober,
    pub(crate) clock: Clock,
}

/// A "reconcile now" request from the control socket: compare the enrollment
/// with this generation, verify the link, and reply over `ack`.
pub(crate) struct RefederateRequest {
    pub(crate) ack: oneshot::Sender<FederationOutcome>,
}

/// Sends pokes to the federation task (held by [`FederationControl`]).
///
/// [`FederationControl`]: super::federation_control::FederationControl
pub(crate) type TriggerSender = mpsc::Sender<RefederateRequest>;
/// Receives pokes in the federation task.
pub(crate) type TriggerReceiver = mpsc::Receiver<RefederateRequest>;

/// Background task (a [`ServeAsyncCommand`]) that answers federation pokes for
/// the daemon's lifetime. See the module docs.
pub(crate) struct RouterFederation {
    deps: FederationDeps,
    trigger_rx: TriggerReceiver,
    /// What this generation's router and sessions were built from.
    generation: FederationIdentity,
    /// Whether the router runs an operator-pinned `ZENOH_CONFIG`.
    pinned: bool,
    /// Shared coordinator token: the task tears down when it is cancelled (an
    /// in-process restart) or on a real OS shutdown signal.
    teardown_token: CancellationToken,
}

impl RouterFederation {
    pub(crate) fn new(
        peppy_dirs: PeppyDirs,
        trigger_rx: TriggerReceiver,
        generation: FederationIdentity,
        pinned: bool,
        teardown_token: CancellationToken,
    ) -> Self {
        Self {
            deps: FederationDeps {
                enrollment: Arc::new(move || auth::enrollment::load(&peppy_dirs)),
                prober: real_prober(),
                clock: Arc::new(auth::storage::now_unix),
            },
            trigger_rx,
            generation,
            pinned,
            teardown_token,
        }
    }
}

impl ServeAsyncCommand for RouterFederation {
    fn run(self: Box<Self>) -> ServeAsyncHandle {
        let RouterFederation {
            deps,
            trigger_rx,
            generation,
            pinned,
            teardown_token,
        } = *self;
        let future = Box::pin(async move {
            // Race the poke loop against shutdown (a real signal or an
            // in-process restart via the shared token) so the daemon can exit
            // promptly (the loop is otherwise infinite).
            tokio::select! {
                _ = serve_pokes(&deps, trigger_rx, &generation, pinned) => {}
                _ = crate::shutdown_signal::shutdown_or_token(&teardown_token) => {}
            }
            Ok(())
        });
        // No readiness gate: the router boots already federated, so nothing
        // here stands between startup and `serve` reporting ready.
        ServeAsyncHandle::new(future, None)
    }
}

/// Answers pokes until every trigger sender drops (the control listener is
/// gone, only at teardown in practice).
async fn serve_pokes(
    deps: &FederationDeps,
    mut trigger_rx: TriggerReceiver,
    generation: &FederationIdentity,
    pinned: bool,
) {
    while let Some(req) = trigger_rx.recv().await {
        let outcome = reconcile(deps, generation, pinned).await;
        // The CLI may have already given up (read timeout); ignore. On an
        // identity change this acks `Restart`; the control handler flushes that
        // ack and only then raises the in-process restart signal, so the
        // restart is never triggered from this loop.
        let _ = req.ack.send(outcome);
    }
}

/// One poke: read the enrollment, compare it with this generation, and verify
/// the link when there is one to verify.
async fn reconcile(
    deps: &FederationDeps,
    generation: &FederationIdentity,
    pinned: bool,
) -> FederationOutcome {
    // The read is file-backed; keep it off the async worker.
    let reader = deps.enrollment.clone();
    let enrollment = match tokio::task::spawn_blocking(move || reader()).await {
        Ok(Ok(enrollment)) => enrollment,
        Ok(Err(error)) => {
            warn!(error = %error, "router federation: the enrollment could not be read");
            return FederationOutcome::Failed(error.to_string());
        }
        Err(error) => {
            warn!(error = %error, "router federation: the enrollment read panicked");
            return FederationOutcome::Failed(format!("enrollment read panicked: {error}"));
        }
    };

    let on_disk = FederationIdentity::of(enrollment.as_ref());
    if &on_disk != generation {
        info!(
            from = %generation.namespace,
            to = %on_disk.namespace,
            "router federation: the enrollment changed this daemon's identity; requesting a \
             daemon restart (a router id or a session namespace cannot change while live)"
        );
        return FederationOutcome::Restart;
    }
    if pinned {
        warn!(
            "router federation: the managed router uses an operator-pinned ZENOH_CONFIG, so its \
             federation is not managed by the enrollment"
        );
        return FederationOutcome::Pinned;
    }
    let Some(enrollment) = enrollment else {
        return FederationOutcome::Applied(None);
    };

    let now = (deps.clock)();
    if enrollment.document.is_expired(now) {
        let expired_at =
            chrono::DateTime::from_timestamp(enrollment.document.certificate_expires_at, 0)
                .map(|t| t.to_rfc3339())
                .unwrap_or_else(|| enrollment.document.certificate_expires_at.to_string());
        warn!(
            expired_at = %expired_at,
            "router federation: the peer certificate has expired; the cloud router refuses the link"
        );
        return FederationOutcome::Unreachable(format!(
            "the peer certificate expired at {expired_at}; run `peppy platform enroll --replace`"
        ));
    }

    let (locator, tls) = enrollment.federation_target();
    let host = enrollment.document.router.host().to_string();
    let port = enrollment.document.router.port();
    match (deps.prober)(host, port, tls, PROBE_TIMEOUT).await {
        Ok(()) => FederationOutcome::Applied(Some(locator)),
        Err(reason) => {
            warn!(
                upstream = %locator, reason = %reason,
                "router federation: the mutual-TLS link to the project's cloud router could not \
                 be established; the router keeps retrying"
            );
            FederationOutcome::Unreachable(reason)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use auth::enrollment::{EnrollmentDocument, RouterEndpoint};
    use std::path::PathBuf;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

    const PROJECT: &str = "550e8400-e29b-41d4-a716-446655440000";
    const ZID: &str = "2f6c1d8e9a0b4c5d6e7f8091a2b3c4d5";
    const LOCATOR: &str = "tls/rtr-p.example:7447";
    const EXPIRES_AT: i64 = 2_000_000_000;

    /// An enrollment whose material paths are nominal: the reader is injected,
    /// so nothing here opens them.
    fn enrollment(zid: &str) -> Enrollment {
        Enrollment {
            document: EnrollmentDocument {
                version: auth::enrollment::ENROLLMENT_VERSION,
                api_url: "https://api.example".into(),
                workspace_id: "ws".into(),
                project_id: PROJECT.into(),
                peer_id: "peer-1".into(),
                peer_name: "robot-7".into(),
                zenoh_id: RouterId::parse(zid).unwrap(),
                namespace: Namespace::parse(PROJECT).unwrap(),
                router: RouterEndpoint::parse("rtr-p.example", 7447).unwrap(),
                certificate_expires_at: EXPIRES_AT,
                enrolled_at: 1_700_000_000,
            },
            peer_key: PathBuf::from("/peer/peer.key"),
            peer_certificate: PathBuf::from("/peer/peer.crt"),
            trust_anchor: PathBuf::from("/peer/ca.crt"),
        }
    }

    fn reader(value: Option<Enrollment>) -> EnrollmentReader {
        Arc::new(move || Ok(value.clone()))
    }

    /// A prober returning a fixed result and counting its calls.
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

    struct TestDeps {
        enrollment: EnrollmentReader,
        prober: Prober,
        clock: Clock,
    }

    impl TestDeps {
        /// The uninteresting case: enrolled, a passing probe, a clock well
        /// before the certificate expires.
        fn new() -> Self {
            Self {
                enrollment: reader(Some(enrollment(ZID))),
                prober: counting_prober(Ok(())).0,
                clock: Arc::new(|| EXPIRES_AT - 1),
            }
        }

        fn enrollment(mut self, enrollment: EnrollmentReader) -> Self {
            self.enrollment = enrollment;
            self
        }

        fn prober(mut self, prober: Prober) -> Self {
            self.prober = prober;
            self
        }

        fn clock(mut self, now: i64) -> Self {
            self.clock = Arc::new(move || now);
            self
        }

        fn build(self) -> FederationDeps {
            FederationDeps {
                enrollment: self.enrollment,
                prober: self.prober,
                clock: self.clock,
            }
        }
    }

    fn enrolled_generation() -> FederationIdentity {
        FederationIdentity::of(Some(&enrollment(ZID)))
    }

    #[tokio::test]
    async fn an_enrolled_generation_probes_and_reports_applied() {
        let (prober, calls) = counting_prober(Ok(()));
        let deps = TestDeps::new().prober(prober).build();

        let outcome = reconcile(&deps, &enrolled_generation(), false).await;

        assert_eq!(
            outcome,
            FederationOutcome::Applied(Some(LOCATOR.to_string()))
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1, "the link is probed once");
    }

    #[tokio::test]
    async fn a_failing_probe_reports_unreachable() {
        let reason = "received fatal alert: UnknownCA";
        let (prober, calls) = counting_prober(Err(reason.to_string()));
        let deps = TestDeps::new().prober(prober).build();

        let outcome = reconcile(&deps, &enrolled_generation(), false).await;

        assert_eq!(outcome, FederationOutcome::Unreachable(reason.to_string()));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    /// The probe dials the enrolled router with the enrolled material: the
    /// project CA as trust and the peer certificate and key as identity, under
    /// the bounded `PROBE_TIMEOUT`.
    #[tokio::test]
    async fn the_probe_receives_the_enrollments_identity_and_bound() {
        let seen: Arc<Mutex<Option<(String, u16, pmi::TlsConfig)>>> = Arc::new(Mutex::new(None));
        let timeout_millis = Arc::new(AtomicU64::new(0));
        let (record, millis) = (seen.clone(), timeout_millis.clone());
        let prober: Prober = Arc::new(move |host, port, tls, timeout| -> ProbeFuture {
            *record.lock().unwrap() = Some((host, port, tls));
            millis.store(timeout.as_millis() as u64, Ordering::SeqCst);
            Box::pin(async { Ok(()) })
        });
        let deps = TestDeps::new().prober(prober).build();

        reconcile(&deps, &enrolled_generation(), false).await;

        let (host, port, tls) = seen.lock().unwrap().clone().expect("probed");
        assert_eq!((host.as_str(), port), ("rtr-p.example", 7447));
        assert_eq!(tls, enrollment(ZID).federation_target().1);
        assert_eq!(tls.root_ca_certificate, Some(PathBuf::from("/peer/ca.crt")));
        assert_eq!(
            tls.connect_identity,
            Some(pmi::ConnectIdentity {
                certificate: PathBuf::from("/peer/peer.crt"),
                private_key: PathBuf::from("/peer/peer.key"),
            })
        );
        assert!(tls.verify_name_on_connect);
        assert_eq!(
            timeout_millis.load(Ordering::SeqCst),
            PROBE_TIMEOUT.as_millis() as u64
        );
    }

    #[tokio::test]
    async fn a_pinned_router_reports_pinned_and_never_probes() {
        let (prober, calls) = counting_prober(Ok(()));
        let deps = TestDeps::new().prober(prober).build();

        assert_eq!(
            reconcile(&deps, &enrolled_generation(), true).await,
            FederationOutcome::Pinned
        );
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn an_unenrolled_generation_reports_applied_none_and_never_probes() {
        let (prober, calls) = counting_prober(Ok(()));
        let deps = TestDeps::new()
            .enrollment(reader(None))
            .prober(prober)
            .build();

        assert_eq!(
            reconcile(&deps, &FederationIdentity::of(None), false).await,
            FederationOutcome::Applied(None)
        );
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    /// Every identity change on disk asks for a restart without probing: a
    /// first enrollment under a `local` generation, a re-enrollment that keeps
    /// the project but mints a new id, and an unenrollment.
    #[tokio::test]
    async fn an_identity_change_on_disk_acks_restart_without_probing() {
        let cases: [(&str, EnrollmentReader, FederationIdentity); 3] = [
            (
                "enrolling",
                reader(Some(enrollment(ZID))),
                FederationIdentity::of(None),
            ),
            (
                "re-enrolling with a new id",
                reader(Some(enrollment("7f3a9c1e"))),
                enrolled_generation(),
            ),
            ("unenrolling", reader(None), enrolled_generation()),
        ];
        for (label, enrollment, generation) in cases {
            let (prober, calls) = counting_prober(Ok(()));
            let deps = TestDeps::new()
                .enrollment(enrollment)
                .prober(prober)
                .build();

            assert_eq!(
                reconcile(&deps, &generation, false).await,
                FederationOutcome::Restart,
                "{label}"
            );
            assert_eq!(calls.load(Ordering::SeqCst), 0, "{label}: never probed");
        }
    }

    /// The restart check runs before the pinned check: an operator-pinned
    /// router still restarts its generation so the sessions re-open under the
    /// new namespace.
    #[tokio::test]
    async fn a_pinned_router_still_restarts_on_an_identity_change() {
        let deps = TestDeps::new().enrollment(reader(None)).build();
        assert_eq!(
            reconcile(&deps, &enrolled_generation(), true).await,
            FederationOutcome::Restart
        );
    }

    #[tokio::test]
    async fn an_expired_certificate_reports_unreachable_without_probing() {
        let (prober, calls) = counting_prober(Ok(()));
        let deps = TestDeps::new().prober(prober).clock(EXPIRES_AT).build();

        let outcome = reconcile(&deps, &enrolled_generation(), false).await;

        match outcome {
            FederationOutcome::Unreachable(reason) => {
                assert!(
                    reason.contains("expired") && reason.contains("peppy platform enroll"),
                    "{reason}"
                )
            }
            other => panic!("expected Unreachable, got {other:?}"),
        }
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn an_unreadable_enrollment_reports_failed() {
        let failing: EnrollmentReader =
            Arc::new(|| Err(auth::AuthError::Auth("enrollment material missing".into())));
        let deps = TestDeps::new().enrollment(failing).build();

        assert_eq!(
            reconcile(&deps, &enrolled_generation(), false).await,
            FederationOutcome::Failed("enrollment material missing".to_string())
        );
    }

    /// A poke from the control socket is serviced immediately and acked with
    /// the reconcile outcome; the loop ends when the senders are gone.
    #[tokio::test]
    async fn pokes_are_serviced_and_acked() {
        let (prober, calls) = counting_prober(Ok(()));
        let deps = TestDeps::new().prober(prober).build();
        let (trigger_tx, trigger_rx) = mpsc::channel(8);
        let generation = enrolled_generation();
        let task =
            tokio::spawn(async move { serve_pokes(&deps, trigger_rx, &generation, false).await });

        for _ in 0..2 {
            let (ack_tx, ack_rx) = oneshot::channel();
            trigger_tx
                .send(RefederateRequest { ack: ack_tx })
                .await
                .expect("trigger accepted");
            let outcome = tokio::time::timeout(Duration::from_secs(1), ack_rx)
                .await
                .expect("the poke is serviced immediately")
                .expect("ack sender not dropped");
            assert_eq!(
                outcome,
                FederationOutcome::Applied(Some(LOCATOR.to_string()))
            );
        }
        assert_eq!(calls.load(Ordering::SeqCst), 2, "every poke probes");

        drop(trigger_tx);
        tokio::time::timeout(Duration::from_secs(1), task)
            .await
            .expect("the loop ends once the senders are gone")
            .expect("the task did not panic");
    }

    #[test]
    fn the_identity_follows_the_enrollment() {
        assert_eq!(
            FederationIdentity::of(None),
            FederationIdentity {
                namespace: Namespace::local(),
                zenoh_id: None
            }
        );
        assert_eq!(
            enrolled_generation(),
            FederationIdentity {
                namespace: Namespace::parse(PROJECT).unwrap(),
                zenoh_id: Some(RouterId::parse(ZID).unwrap())
            }
        );
    }
}
