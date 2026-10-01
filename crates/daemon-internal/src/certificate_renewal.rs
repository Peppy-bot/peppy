//! Keeps the peer certificate of an enrolled machine valid.
//!
//! The cloud router closes the link of a peer when the leaf certificate of
//! that peer expires. This task asks the platform for a new leaf when the
//! renewal is due ([`auth::CertificateValidity::renewal_due_at`], read from
//! `peer.crt` itself), and [`auth::renewal::renew`] writes it over `peer.crt`. The identity of the
//! peer does not change, so the generation does not restart: zenoh reads
//! `peer.crt` each time it opens the link, and the link that the cloud router
//! closes at the expiry of the old leaf opens again with the new one.
//!
//! The platform accepts a renewal from a signed-in person only, so the task
//! uses the session that `peppy platform login` cached on this machine. With
//! no session the renewal fails, and the task tries again at the next check.
//!
//! The task sleeps for [`CHECK_INTERVAL`] at most and then reads the
//! enrollment and the clock again. A machine that was suspended, or a clock
//! that was set, thus moves the renewal by one interval at most.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use auth::{AuthError, Enrollment, ProblemKind};
use daemon_config::consts::PeppyDirs;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

use crate::router_federation::{Clock, EnrollmentReader};
use crate::serve::{ServeAsyncCommand, ServeAsyncHandle};

/// The longest sleep between two checks, and the delay before the next attempt
/// after a renewal that failed with no delay from the platform.
const CHECK_INTERVAL: Duration = Duration::from_secs(60 * 60);

/// The shortest delay before the next attempt, for a platform that asks for
/// no delay or for a delay of zero.
const MIN_RETRY_DELAY: Duration = Duration::from_secs(10);

/// Asks the platform for a new leaf and writes it; the answer is the renewed
/// enrollment. The call blocks: it does HTTP and file I/O.
type Renewer = Arc<dyn Fn() -> auth::Result<Enrollment> + Send + Sync>;

/// Waits for the given time. Injected so that a test does not wait.
type Sleeper = Arc<dyn Fn(Duration) -> Pin<Box<dyn Future<Output = ()> + Send>> + Send + Sync>;

/// The injectable collaborators of the task.
struct RenewalDeps {
    enrollment: EnrollmentReader,
    renewer: Renewer,
    clock: Clock,
    sleeper: Sleeper,
}

/// What the task does after one check.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Step {
    /// Check again after this time.
    Wait(Duration),
    /// No later check can succeed for this generation.
    Stop,
}

/// Background task (a [`ServeAsyncCommand`]) that renews the peer certificate
/// for the life of a generation. See the module docs.
pub(crate) struct CertificateRenewal {
    deps: RenewalDeps,
    /// Shared coordinator token: the task tears down when it is cancelled (an
    /// in-process restart) or on a real OS shutdown signal.
    teardown_token: CancellationToken,
}

impl CertificateRenewal {
    pub(crate) fn new(peppy_dirs: PeppyDirs, teardown_token: CancellationToken) -> Self {
        let renewal_dirs = peppy_dirs.clone();
        Self {
            deps: RenewalDeps {
                enrollment: Arc::new(move || auth::enrollment::load(&peppy_dirs)),
                renewer: Arc::new(move || {
                    auth::renewal::renew(&renewal_dirs, &auth::http::HttpClient::new())
                }),
                clock: Arc::new(auth::storage::now_unix),
                sleeper: Arc::new(|time| Box::pin(tokio::time::sleep(time))),
            },
            teardown_token,
        }
    }
}

impl ServeAsyncCommand for CertificateRenewal {
    fn run(self: Box<Self>) -> ServeAsyncHandle {
        let CertificateRenewal {
            deps,
            teardown_token,
        } = *self;
        let future = Box::pin(async move {
            tokio::select! {
                _ = keep_certificate_valid(&deps) => {}
                _ = crate::shutdown_signal::shutdown_or_token(&teardown_token) => {}
            }
            Ok(())
        });
        // No readiness gate: the router boots with the certificate on disk.
        ServeAsyncHandle::new(future, None)
    }
}

/// Checks, and waits, until a check says that no later one can succeed.
async fn keep_certificate_valid(deps: &RenewalDeps) {
    while let Step::Wait(time) = check_and_renew(deps).await {
        (deps.sleeper)(time).await;
    }
}

/// One check: read the enrollment and the clock, and renew the certificate
/// when the renewal is due.
async fn check_and_renew(deps: &RenewalDeps) -> Step {
    let reader = deps.enrollment.clone();
    let enrollment = match run_blocking(move || reader()).await {
        Ok(Some(enrollment)) => enrollment,
        Ok(None) => {
            info!("certificate renewal: this machine is not enrolled; nothing to renew");
            return Step::Stop;
        }
        Err(reason) => {
            warn!(reason = %reason, "certificate renewal: the enrollment could not be read");
            return Step::Wait(CHECK_INTERVAL);
        }
    };

    let now = (deps.clock)();
    let certificate = enrollment.certificate;
    if !certificate.is_renewal_due(now) {
        return Step::Wait(time_until(certificate.renewal_due_at(), now).min(CHECK_INTERVAL));
    }

    let renewer = deps.renewer.clone();
    let renewed = run_blocking(move || renewer()).await;
    step_after_renewal(&enrollment, renewed)
}

/// Runs a blocking call off the async workers. A call that panics is a
/// failure with the panic as its reason.
async fn run_blocking<T: Send + 'static>(
    call: impl FnOnce() -> auth::Result<T> + Send + 'static,
) -> std::result::Result<T, RenewalFailure> {
    match tokio::task::spawn_blocking(call).await {
        Ok(Ok(value)) => Ok(value),
        Ok(Err(error)) => Err(RenewalFailure::Refused(error)),
        Err(panic) => Err(RenewalFailure::Panicked(panic.to_string())),
    }
}

/// Why a read or a renewal did not complete.
#[derive(Debug)]
enum RenewalFailure {
    Refused(AuthError),
    Panicked(String),
}

impl std::fmt::Display for RenewalFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Refused(error) => write!(f, "{error}"),
            Self::Panicked(panic) => write!(f, "the call panicked: {panic}"),
        }
    }
}

/// The time from `now_unix` to `then_unix`; zero when `then_unix` is past.
fn time_until(then_unix: i64, now_unix: i64) -> Duration {
    Duration::from_secs(u64::try_from(then_unix.saturating_sub(now_unix)).unwrap_or(0))
}

/// Logs the result of a renewal and says when the next check is.
fn step_after_renewal(
    enrollment: &Enrollment,
    renewed: std::result::Result<Enrollment, RenewalFailure>,
) -> Step {
    let peer = &enrollment.document.peer_id;
    let failure = match renewed {
        Ok(renewed) => {
            info!(
                peer = %peer,
                expires_at = %rfc3339(renewed.certificate.not_after),
                next_renewal_at = %rfc3339(renewed.certificate.renewal_due_at()),
                "certificate renewal: the peer certificate is renewed"
            );
            return Step::Wait(CHECK_INTERVAL);
        }
        Err(failure) => failure,
    };

    let expires_at = rfc3339(enrollment.certificate.not_after);
    match &failure {
        RenewalFailure::Refused(AuthError::Problem(problem))
            if problem.kind == ProblemKind::PeerNotRenewable =>
        {
            error!(
                peer = %peer, expires_at = %expires_at, reason = %failure,
                "certificate renewal: the platform cannot renew this peer; run `peppy platform \
                 enroll --replace` before the certificate expires"
            );
            Step::Stop
        }
        RenewalFailure::Refused(AuthError::NotAuthenticated) => {
            warn!(
                peer = %peer, expires_at = %expires_at,
                "certificate renewal: there is no session on this machine; run `peppy platform \
                 login` so that the daemon can renew the peer certificate"
            );
            Step::Wait(CHECK_INTERVAL)
        }
        RenewalFailure::Refused(AuthError::Problem(problem)) => {
            let delay = retry_delay(problem.retry_after_secs);
            warn!(
                peer = %peer, expires_at = %expires_at, reason = %failure,
                retry_in_secs = delay.as_secs(),
                "certificate renewal: the platform refused the renewal"
            );
            Step::Wait(delay)
        }
        RenewalFailure::Refused(_) | RenewalFailure::Panicked(_) => {
            warn!(
                peer = %peer, expires_at = %expires_at, reason = %failure,
                retry_in_secs = CHECK_INTERVAL.as_secs(),
                "certificate renewal: the renewal did not complete"
            );
            Step::Wait(CHECK_INTERVAL)
        }
    }
}

/// The delay before the next attempt: the one the platform asked for, and
/// [`CHECK_INTERVAL`] when it asked for none.
fn retry_delay(retry_after_secs: Option<u64>) -> Duration {
    retry_after_secs
        .map(Duration::from_secs)
        .map_or(CHECK_INTERVAL, |asked| asked.max(MIN_RETRY_DELAY))
}

/// Unix seconds as RFC 3339 for the log, or the number itself when it is not
/// a date.
pub(crate) fn rfc3339(unix: i64) -> String {
    chrono::DateTime::from_timestamp(unix, 0)
        .map(|time| time.to_rfc3339())
        .unwrap_or_else(|| unix.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use auth::CertificateValidity;
    use auth::test_support::{self, ISSUED_AT};
    use std::collections::VecDeque;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicI64, Ordering};

    const DAY: i64 = 24 * 60 * 60;
    const LIFETIME: i64 = 90 * DAY;
    const DUE_AT: i64 = ISSUED_AT + 60 * DAY;

    /// The enrollment with a leaf issued at `issued_at` for [`LIFETIME`].
    fn enrollment(issued_at: i64) -> Enrollment {
        Enrollment {
            certificate: CertificateValidity {
                not_before: issued_at,
                not_after: issued_at + LIFETIME,
            },
            ..test_support::enrollment()
        }
    }

    fn problem(kind: ProblemKind, status: u16, retry_after_secs: Option<u64>) -> AuthError {
        AuthError::Problem(auth::Problem {
            retry_after_secs,
            ..test_support::problem(kind, status)
        })
    }

    /// What the platform answers to one renewal.
    enum Answer {
        Renewed,
        Refused(AuthError),
    }

    /// A machine with a clock that moves only when the task sleeps, and a
    /// platform that answers from a script.
    struct Machine {
        now: Arc<AtomicI64>,
        on_disk: Arc<Mutex<auth::Result<Option<Enrollment>>>>,
        script: Arc<Mutex<VecDeque<Answer>>>,
        /// The clock at each renewal the task asked for.
        renewals: Arc<Mutex<Vec<i64>>>,
        /// Each time the task slept for.
        sleeps: Arc<Mutex<Vec<Duration>>>,
    }

    impl Machine {
        fn enrolled_at(now: i64) -> Self {
            Self::with_disk(now, Ok(Some(enrollment(ISSUED_AT))))
        }

        fn with_disk(now: i64, on_disk: auth::Result<Option<Enrollment>>) -> Self {
            Self {
                now: Arc::new(AtomicI64::new(now)),
                on_disk: Arc::new(Mutex::new(on_disk)),
                script: Arc::new(Mutex::new(VecDeque::new())),
                renewals: Arc::new(Mutex::new(Vec::new())),
                sleeps: Arc::new(Mutex::new(Vec::new())),
            }
        }

        fn answers(self, answers: impl IntoIterator<Item = Answer>) -> Self {
            self.script.lock().unwrap().extend(answers);
            self
        }

        fn deps(&self) -> RenewalDeps {
            let on_disk = self.on_disk.clone();
            let (disk, script, renewals) = (
                self.on_disk.clone(),
                self.script.clone(),
                self.renewals.clone(),
            );
            let clock = self.now.clone();
            let renewal_clock = self.now.clone();
            let (slept_clock, sleeps) = (self.now.clone(), self.sleeps.clone());
            RenewalDeps {
                enrollment: Arc::new(move || match &*on_disk.lock().unwrap() {
                    Ok(enrollment) => Ok(enrollment.clone()),
                    Err(error) => Err(AuthError::Auth(error.to_string())),
                }),
                renewer: Arc::new(move || {
                    let now = renewal_clock.load(Ordering::SeqCst);
                    renewals.lock().unwrap().push(now);
                    let answer = script
                        .lock()
                        .unwrap()
                        .pop_front()
                        .expect("the task asked for a renewal the test did not expect");
                    match answer {
                        Answer::Refused(error) => Err(error),
                        Answer::Renewed => {
                            let renewed = enrollment(now);
                            *disk.lock().unwrap() = Ok(Some(renewed.clone()));
                            Ok(renewed)
                        }
                    }
                }),
                clock: Arc::new(move || clock.load(Ordering::SeqCst)),
                sleeper: Arc::new(move |time| {
                    sleeps.lock().unwrap().push(time);
                    slept_clock.fetch_add(time.as_secs() as i64, Ordering::SeqCst);
                    Box::pin(async {})
                }),
            }
        }

        fn renewals(&self) -> Vec<i64> {
            self.renewals.lock().unwrap().clone()
        }
    }

    #[tokio::test]
    async fn a_certificate_far_from_its_renewal_waits_one_check_interval() {
        let machine = Machine::enrolled_at(ISSUED_AT);

        assert_eq!(
            check_and_renew(&machine.deps()).await,
            Step::Wait(CHECK_INTERVAL)
        );
        assert!(machine.renewals().is_empty());
    }

    #[tokio::test]
    async fn a_certificate_near_its_renewal_waits_until_the_renewal_is_due() {
        let machine = Machine::enrolled_at(DUE_AT - 120);

        assert_eq!(
            check_and_renew(&machine.deps()).await,
            Step::Wait(Duration::from_secs(120))
        );
        assert!(machine.renewals().is_empty());
    }

    #[tokio::test]
    async fn a_due_renewal_asks_the_platform_and_checks_again_after_one_interval() {
        let machine = Machine::enrolled_at(DUE_AT).answers([Answer::Renewed]);

        assert_eq!(
            check_and_renew(&machine.deps()).await,
            Step::Wait(CHECK_INTERVAL)
        );
        assert_eq!(machine.renewals(), [DUE_AT]);
    }

    /// The platform can renew a leaf that expired, so the task asks.
    #[tokio::test]
    async fn an_expired_certificate_is_renewed() {
        let now = ISSUED_AT + LIFETIME + DAY;
        let machine = Machine::enrolled_at(now).answers([Answer::Renewed]);

        assert_eq!(
            check_and_renew(&machine.deps()).await,
            Step::Wait(CHECK_INTERVAL)
        );
        assert_eq!(machine.renewals(), [now]);
    }

    #[tokio::test]
    async fn a_machine_with_no_session_tries_again_after_one_interval() {
        let machine =
            Machine::enrolled_at(DUE_AT).answers([Answer::Refused(AuthError::NotAuthenticated)]);

        assert_eq!(
            check_and_renew(&machine.deps()).await,
            Step::Wait(CHECK_INTERVAL)
        );
    }

    #[tokio::test]
    async fn a_refusal_waits_for_the_delay_the_platform_asked_for() {
        let cases = [
            (Some(1800), Duration::from_secs(1800)),
            (Some(0), MIN_RETRY_DELAY),
            (None, CHECK_INTERVAL),
        ];
        for (retry_after_secs, expected) in cases {
            let machine = Machine::enrolled_at(DUE_AT).answers([Answer::Refused(problem(
                ProblemKind::ProvisionerUnavailable,
                503,
                retry_after_secs,
            ))]);

            assert_eq!(
                check_and_renew(&machine.deps()).await,
                Step::Wait(expected),
                "{retry_after_secs:?}"
            );
        }
    }

    #[tokio::test]
    async fn a_failure_that_is_not_a_refusal_tries_again_after_one_interval() {
        let machine = Machine::enrolled_at(DUE_AT).answers([Answer::Refused(AuthError::Http(
            "the platform did not answer".into(),
        ))]);

        assert_eq!(
            check_and_renew(&machine.deps()).await,
            Step::Wait(CHECK_INTERVAL)
        );
    }

    #[tokio::test]
    async fn a_peer_the_platform_cannot_renew_stops_the_task() {
        let machine = Machine::enrolled_at(DUE_AT).answers([Answer::Refused(problem(
            ProblemKind::PeerNotRenewable,
            409,
            None,
        ))]);

        assert_eq!(check_and_renew(&machine.deps()).await, Step::Stop);
    }

    #[tokio::test]
    async fn a_machine_that_is_not_enrolled_stops_the_task() {
        let machine = Machine::with_disk(DUE_AT, Ok(None));

        assert_eq!(check_and_renew(&machine.deps()).await, Step::Stop);
        assert!(machine.renewals().is_empty());
    }

    #[tokio::test]
    async fn an_enrollment_that_cannot_be_read_is_read_again_after_one_interval() {
        let machine = Machine::with_disk(
            DUE_AT,
            Err(AuthError::Auth("enrollment material missing".into())),
        );

        assert_eq!(
            check_and_renew(&machine.deps()).await,
            Step::Wait(CHECK_INTERVAL)
        );
        assert!(machine.renewals().is_empty());
    }

    /// The life of the task from the enrollment: it waits for the renewal, the
    /// platform is not available at the first attempt, the second attempt
    /// renews, and the task then waits for the renewal of the new leaf.
    #[tokio::test]
    async fn the_task_renews_each_certificate_when_its_renewal_is_due() {
        let machine = Machine::enrolled_at(ISSUED_AT).answers([
            Answer::Refused(problem(ProblemKind::ProvisionerUnavailable, 503, Some(30))),
            Answer::Renewed,
            Answer::Refused(problem(ProblemKind::PeerNotRenewable, 409, None)),
        ]);

        keep_certificate_valid(&machine.deps()).await;

        let second_issue = DUE_AT + 30;
        assert_eq!(
            machine.renewals(),
            [DUE_AT, second_issue, second_issue + 60 * DAY]
        );
        let sleeps = machine.sleeps.lock().unwrap();
        assert!(
            sleeps.iter().all(|time| *time <= CHECK_INTERVAL),
            "no sleep is longer than one check interval"
        );
        assert!(sleeps.contains(&Duration::from_secs(30)));
    }
}
