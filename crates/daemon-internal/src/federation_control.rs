//! Version-1 local identity-control server.
//!
//! A [`ServeAsyncCommand`] that binds the per-user Unix-domain socket
//! ([`crate::control::federation_control_socket_path`]) and, for each
//! connection, validates a strict JSON-line [`ControlRequest`], and temporarily
//! bridges identity-changing operations to the
//! [`IdentityController`](super::router_federation::IdentityController) loop. It waits for that poll
//! to apply and writes the resulting [`ControlResponse`] back, so
//! the CLI learns federation is in place *after* the local zenohd bounce, which
//! is exactly why the channel is a UDS, independent of the router being
//! restarted. Status requests are answered inline from the federation loop's
//! status watch, so they can never queue behind an in-flight apply. No legacy
//! raw-command parser exists.
//!
//! Binding is best-effort: a bind failure is logged and the task idles until
//! shutdown rather than taking the daemon down; the periodic federation poll
//! still (de)federates, only the *immediate* poke is unavailable.

use std::path::{Path, PathBuf};
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::mpsc::error::TrySendError;
use tokio::sync::{oneshot, watch};
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use uuid::Uuid;

use crate::control::{
    CleanupState, ControlErrorCode, ControlOperation, ControlRequest, ControlResponse,
    ControlResult, FederationStatus, LogoutResult, MAX_REQUEST_LINE_BYTES, MAX_RESPONSE_LINE_BYTES,
    PROTOCOL_VERSION, RouterApplyState,
};
use crate::router_federation::{
    FederationOutcome, FederationTrigger, IdentityFailureCode, LogoutRouterState, TriggerSender,
};
use crate::serve::{ServeAsyncCommand, ServeAsyncHandle};

/// Bound on reading a request line, so a client that connects but never writes
/// cannot hold a handler task open.
const REQUEST_READ_TIMEOUT: Duration = Duration::from_secs(5);

/// Extra time the daemon waits for the federation loop to apply, on top of the
/// bounded backend windows below. It must cover the
/// post-resolve work of a *verifying* login poke: the zenohd bounce plus the TLS
/// reachability probe ([`super::router_federation::PROBE_TIMEOUT`]). Kept smaller
/// than the client's read slack ([`crate::control::POKE_READ_SLACK`]) so
/// the daemon always replies a definite outcome before the client gives up (the
/// `ack_budget_*` test guards both relationships).
const APPLY_ACK_SLACK: Duration = Duration::from_secs(15);

/// Worst-case sequential request windows by operation. These mirror the CLI's
/// two-window login and five-window logout budgets; sharing logout's larger
/// ceiling with login would allow a queued login to mutate after its client had
/// already timed out.
const LOGIN_BACKEND_REQUEST_WINDOWS: u32 = 2;
const LOGOUT_BACKEND_REQUEST_WINDOWS: u32 = 5;

/// Mutation commands are admitted only for an immediately-idle controller. A
/// command that sits behind startup/maintenance or another operation is dropped
/// before it can touch identity state, with ample operation budget still left
/// for any command that starts inside this short scheduling allowance.
const MUTATION_START_ALLOWANCE: Duration = Duration::from_millis(100);

/// Reserved inside the total deadline so an operation timeout can still be
/// serialized and flushed as a definite response.
const RESPONSE_WRITE_BUDGET: Duration = Duration::from_millis(250);

impl From<FederationOutcome> for ControlResponse {
    fn from(outcome: FederationOutcome) -> Self {
        match outcome {
            FederationOutcome::Applied(link) => {
                ControlResponse::new(ControlResult::Applied { link })
            }
            FederationOutcome::OperatorManaged => {
                ControlResponse::new(ControlResult::OperatorManaged)
            }
            FederationOutcome::LoggedOut(outcome) => {
                let cleanup = |attempt: auth::logout::CleanupAttempt| match attempt {
                    auth::logout::CleanupAttempt::NotNeeded => CleanupState::NotNeeded,
                    auth::logout::CleanupAttempt::Succeeded => CleanupState::Succeeded,
                    auth::logout::CleanupAttempt::Failed(_) => CleanupState::Failed,
                };
                let router_apply = match outcome.router {
                    LogoutRouterState::Standalone => RouterApplyState::Standalone,
                    LogoutRouterState::OperatorManaged => RouterApplyState::OperatorManaged,
                    LogoutRouterState::Uncertain => RouterApplyState::Error,
                };
                ControlResponse::new(ControlResult::LoggedOut {
                    outcome: LogoutResult {
                        certificate_revocation: cleanup(outcome.certificate_revocation),
                        oauth_revocation: cleanup(outcome.oauth_revocation),
                        router_apply,
                        local_cleanup: cleanup(outcome.local_cleanup),
                        operator_action_required: outcome.operator_action_required,
                        target_namespace: outcome
                            .target_namespace
                            .map(|namespace| namespace.as_str().to_string()),
                    },
                })
            }
            // Raw resolver/backend errors can contain request or transport
            // context. They stay in daemon-local logs; the control wire gets a
            // fixed public diagnostic.
            FederationOutcome::Failed(_message) => ControlResponse::error(
                ControlErrorCode::OperationFailed,
                "identity operation failed",
            ),
            FederationOutcome::Rejected { code, message: _ } => {
                let code = match code {
                    IdentityFailureCode::StaleSessionRevision => {
                        ControlErrorCode::StaleSessionRevision
                    }
                    IdentityFailureCode::NotAuthenticated => ControlErrorCode::NotAuthenticated,
                    IdentityFailureCode::PatNotConfigured => ControlErrorCode::PatNotConfigured,
                    IdentityFailureCode::PatActive => ControlErrorCode::PatActive,
                    IdentityFailureCode::PatPrincipalMismatch => {
                        ControlErrorCode::PatPrincipalMismatch
                    }
                    IdentityFailureCode::PatOriginMismatch => ControlErrorCode::PatOriginMismatch,
                    IdentityFailureCode::DeadlineExceeded => ControlErrorCode::DeadlineExceeded,
                    IdentityFailureCode::OperationFailed => ControlErrorCode::OperationFailed,
                };
                ControlResponse::error(code, "identity operation was rejected")
            }
            FederationOutcome::Restart { target_namespace } => {
                ControlResponse::new(ControlResult::Restarting {
                    target_namespace: target_namespace.as_str().to_string(),
                })
            }
        }
    }
}

/// Background task owning the federation control socket. See the module docs.
pub(crate) struct FederationControl {
    socket_path: PathBuf,
    trigger_tx: TriggerSender,
    /// The federation loop's published status, answered inline to
    /// status requests.
    status_rx: watch::Receiver<FederationStatus>,
    /// Bound on how long to wait for a poked poll to apply before replying with a
    /// timeout (so a wedged apply can't hold a connection open forever).
    connect_timeout: Duration,
    /// In-process restart signal. The control handler attempts to flush a
    /// `Restarting` ack before raising it (a real happens-before edge when the
    /// client remains connected). A disconnected client cannot suppress the
    /// restart required to apply the new namespace.
    restart_tx: watch::Sender<bool>,
    /// Shared coordinator token: the task tears down when it is cancelled (an
    /// in-process restart) or on a real OS shutdown signal.
    teardown_token: CancellationToken,
}

impl FederationControl {
    pub(crate) fn new(
        socket_path: PathBuf,
        trigger_tx: TriggerSender,
        status_rx: watch::Receiver<FederationStatus>,
        connect_timeout: Duration,
        restart_tx: watch::Sender<bool>,
        teardown_token: CancellationToken,
    ) -> Self {
        Self {
            socket_path,
            trigger_tx,
            status_rx,
            connect_timeout,
            restart_tx,
            teardown_token,
        }
    }
}

impl ServeAsyncCommand for FederationControl {
    fn run(self: Box<Self>) -> ServeAsyncHandle {
        let FederationControl {
            socket_path,
            trigger_tx,
            status_rx,
            connect_timeout,
            restart_tx,
            teardown_token,
        } = *self;
        let future = Box::pin(async move {
            // Race the accept loop against shutdown (a real signal or an in-process
            // restart via the shared token) so the daemon can exit promptly (the
            // loop is otherwise infinite).
            tokio::select! {
                _ = serve_control(&socket_path, trigger_tx, status_rx, connect_timeout, restart_tx) => {}
                _ = crate::shutdown_signal::shutdown_or_token(&teardown_token) => {}
            }
            // Best-effort cleanup so a stale socket does not linger (the next start
            // unlinks unconditionally anyway).
            let _ = std::fs::remove_file(&socket_path);
            Ok(())
        });
        // No readiness gate: binding the control socket is not a startup
        // dependency. The startup federation gate lives in `IdentityController`.
        ServeAsyncHandle::new(future, None)
    }
}

/// Binds the socket and accepts poke connections until cancelled. A bind failure
/// is non-fatal: log it and idle until shutdown rather than aborting the daemon.
async fn serve_control(
    socket_path: &Path,
    trigger_tx: TriggerSender,
    status_rx: watch::Receiver<FederationStatus>,
    connect_timeout: Duration,
    restart_tx: watch::Sender<bool>,
) {
    let listener = match bind_listener(socket_path) {
        Ok(listener) => listener,
        Err(e) => {
            warn!(
                error = %e,
                path = %socket_path.display(),
                "federation control: could not bind control socket; login/logout pokes \
                 will not be applied immediately (federation still updates on its own poll)"
            );
            // Hold `trigger_tx`/`restart_tx` alive (so the federation loop's
            // channel and the restart watch stay open) and wait for the shutdown
            // race above to cancel us.
            let _keep_sender_alive = trigger_tx;
            let _keep_restart_alive = restart_tx;
            std::future::pending::<()>().await;
            return;
        }
    };
    info!(
        path = %socket_path.display(),
        "federation control: listening for login/logout federation pokes"
    );
    accept_loop(listener, trigger_tx, status_rx, connect_timeout, restart_tx).await;
}

/// Accepts poke connections on an already-bound listener until cancelled.
/// Split from [`serve_control`] so callers that need a ready-to-connect socket
/// (the tests) can bind first and only then run the loop: once
/// [`bind_listener`] returns, connects succeed and queue in the backlog even
/// before this loop is scheduled.
async fn accept_loop(
    listener: UnixListener,
    trigger_tx: TriggerSender,
    status_rx: watch::Receiver<FederationStatus>,
    connect_timeout: Duration,
    restart_tx: watch::Sender<bool>,
) {
    // Back off on a failed `accept()` so a *persistent* error (e.g. the process
    // ran out of file descriptors) can't spin this loop into a CPU/log storm.
    // Starts small, doubles on each consecutive failure up to a cap, and resets
    // after any successful accept. The whole loop is raced against shutdown by the
    // caller, so a sleep here never delays daemon exit.
    const ACCEPT_BACKOFF_INIT: Duration = Duration::from_millis(5);
    const ACCEPT_BACKOFF_MAX: Duration = Duration::from_secs(1);
    let mut accept_backoff = ACCEPT_BACKOFF_INIT;
    loop {
        match listener.accept().await {
            Ok((stream, _addr)) => {
                accept_backoff = ACCEPT_BACKOFF_INIT;
                if let Err(error) = validate_peer_uid(&stream) {
                    // Do not log peer-provided data (there is none yet) or the
                    // peer credential itself. The fixed diagnostic is enough
                    // to identify a local permission/configuration problem.
                    warn!(error = %error, "identity control: rejected control peer");
                    continue;
                }
                let trigger_tx = trigger_tx.clone();
                let status_rx = status_rx.clone();
                let restart_tx = restart_tx.clone();
                tokio::spawn(async move {
                    if let Err(e) =
                        handle_conn(stream, trigger_tx, status_rx, connect_timeout, restart_tx)
                            .await
                    {
                        warn!(error = %e, "federation control: error handling a poke");
                    }
                });
            }
            Err(e) => {
                warn!(error = %e, "federation control: accept failed; backing off then continuing");
                tokio::time::sleep(accept_backoff).await;
                accept_backoff = (accept_backoff * 2).min(ACCEPT_BACKOFF_MAX);
            }
        }
    }
}

/// Creates and validates the owner-only runtime dir, removes any stale socket
/// from a prior daemon, binds, and restricts the socket to the owner.
fn bind_listener(socket_path: &Path) -> std::io::Result<UnixListener> {
    if let Some(parent) = socket_path.parent() {
        std::fs::create_dir_all(parent)?;
        let metadata = std::fs::symlink_metadata(parent)?;
        if !metadata.file_type().is_dir() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "identity-control runtime path is not a directory",
            ));
        }
        {
            use std::os::unix::fs::{MetadataExt, PermissionsExt};
            if metadata.uid() != rustix::process::geteuid().as_raw() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "identity-control runtime directory has a different owner",
                ));
            }
            std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))?;
        }
    }
    match std::fs::remove_file(socket_path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    let listener = UnixListener::bind(socket_path)?;
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(socket_path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(listener)
}

/// Rejects cross-user local clients where the platform exposes reliable peer
/// credentials. Tokio's implementation reports real peer UIDs on these Unix
/// families; a small set of embedded Unix targets return placeholders and are
/// intentionally excluded.
#[cfg(not(any(
    target_os = "espidf",
    target_os = "nuttx",
    target_os = "vita",
    target_os = "hurd"
)))]
fn validate_peer_uid(stream: &UnixStream) -> std::io::Result<()> {
    let peer_uid = stream.peer_cred()?.uid();
    if peer_uid != rustix::process::geteuid().as_raw() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "identity-control peer belongs to a different user",
        ));
    }
    Ok(())
}

#[cfg(any(
    target_os = "espidf",
    target_os = "nuttx",
    target_os = "vita",
    target_os = "hurd"
))]
fn validate_peer_uid(_stream: &UnixStream) -> std::io::Result<()> {
    Ok(())
}

/// Services one request under one total deadline. The deadline covers request
/// reading, dispatch, the federation bridge, serialization, writing, and flush.
async fn handle_conn(
    stream: UnixStream,
    trigger_tx: TriggerSender,
    status_rx: watch::Receiver<FederationStatus>,
    connect_timeout: Duration,
    restart_tx: watch::Sender<bool>,
) -> std::io::Result<()> {
    // `handle_conn` is also called directly by socket-pair tests, so enforce the
    // same peer check here as the accept loop.
    validate_peer_uid(&stream)?;
    let connection_started = tokio::time::Instant::now();
    let total_deadline = connection_started
        + connect_timeout.saturating_mul(LOGOUT_BACKEND_REQUEST_WINDOWS)
        + APPLY_ACK_SLACK;
    match tokio::time::timeout_at(
        total_deadline,
        handle_conn_until(
            stream,
            trigger_tx,
            status_rx,
            restart_tx,
            connection_started,
            connect_timeout,
        ),
    )
    .await
    {
        Ok(result) => result,
        Err(_) => Err(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "identity-control connection exceeded its total deadline",
        )),
    }
}

async fn handle_conn_until(
    stream: UnixStream,
    trigger_tx: TriggerSender,
    status_rx: watch::Receiver<FederationStatus>,
    restart_tx: watch::Sender<bool>,
    connection_started: tokio::time::Instant,
    connect_timeout: Duration,
) -> std::io::Result<()> {
    let (read_half, mut write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);

    let request = match tokio::time::timeout(REQUEST_READ_TIMEOUT, read_request(&mut reader)).await
    {
        Ok(Ok(Some(request))) => request,
        Ok(Ok(None)) => return Ok(()),
        Ok(Err(error)) => {
            return write_response(
                &mut write_half,
                ControlResponse::error(error.code(), error.public_message()),
            )
            .await;
        }
        Err(_) => {
            return write_response(
                &mut write_half,
                ControlResponse::error(
                    ControlErrorCode::DeadlineExceeded,
                    "timed out reading control request",
                ),
            )
            .await;
        }
    };

    if request.protocol_version != PROTOCOL_VERSION {
        return write_response(
            &mut write_half,
            ControlResponse::error(
                ControlErrorCode::UnsupportedProtocol,
                "unsupported control protocol version",
            ),
        )
        .await;
    }

    let response = match request.request {
        ControlOperation::Hello => ControlResponse::hello(),
        ControlOperation::Status => ControlResponse::new(ControlResult::Status {
            // Answered straight from the published cache: no resolve, router
            // bounce, or queueing behind an in-flight apply.
            status: status_rx.borrow().clone(),
        }),
        ControlOperation::EnrollCurrentCredential {
            expected_session_revision,
            expected_pat_subject,
            expected_api_origin,
        } => {
            dispatch_identity_action(
                IdentityControlAction::EnrollCurrentCredential {
                    expected_session_revision,
                    expected_pat_subject,
                    expected_api_origin,
                },
                &trigger_tx,
                connection_started
                    + connect_timeout.saturating_mul(LOGIN_BACKEND_REQUEST_WINDOWS)
                    + APPLY_ACK_SLACK,
            )
            .await
        }
        ControlOperation::PrepareOauthLogin {
            expected_session_revision,
        } => {
            dispatch_identity_action(
                IdentityControlAction::PrepareOauthLogin {
                    expected_session_revision,
                },
                &trigger_tx,
                connection_started
                    + connect_timeout.saturating_mul(LOGIN_BACKEND_REQUEST_WINDOWS)
                    + APPLY_ACK_SLACK,
            )
            .await
        }
        ControlOperation::Logout {
            expected_session_revision,
        } => {
            dispatch_identity_action(
                IdentityControlAction::Logout {
                    expected_session_revision,
                },
                &trigger_tx,
                connection_started
                    + connect_timeout.saturating_mul(LOGOUT_BACKEND_REQUEST_WINDOWS)
                    + APPLY_ACK_SLACK,
            )
            .await
        }
    };

    // A successful flush happens before restart. A disconnected client cannot
    // suppress the restart required for namespace safety.
    let trigger_restart = matches!(&response.response, ControlResult::Restarting { .. })
        || matches!(
            &response.response,
            ControlResult::LoggedOut { outcome }
                if outcome.target_namespace.is_some()
        );
    write_response_then_restart(
        &mut write_half,
        response,
        trigger_restart.then_some(restart_tx),
    )
    .await
}

/// Once a namespace-changing outcome exists, cancellation of the connection
/// task must not strand the old daemon generation. The guard signals after a
/// successful flush, after a write error, or when the outer total deadline
/// drops a still-pending write future.
struct RestartOnDrop(Option<watch::Sender<bool>>);

impl Drop for RestartOnDrop {
    fn drop(&mut self) {
        if let Some(restart_tx) = self.0.take() {
            let _ = restart_tx.send(true);
        }
    }
}

async fn write_response_then_restart(
    write_half: &mut (impl AsyncWriteExt + Unpin),
    response: ControlResponse,
    restart_tx: Option<watch::Sender<bool>>,
) -> std::io::Result<()> {
    let _restart = RestartOnDrop(restart_tx);
    write_response(write_half, response).await
}

/// Wire-independent command accepted by the daemon identity controller.
enum IdentityControlAction {
    EnrollCurrentCredential {
        expected_session_revision: Option<Uuid>,
        expected_pat_subject: Option<String>,
        expected_api_origin: Option<String>,
    },
    PrepareOauthLogin {
        expected_session_revision: Uuid,
    },
    Logout {
        expected_session_revision: Option<Uuid>,
    },
}

async fn dispatch_identity_action(
    action: IdentityControlAction,
    trigger_tx: &TriggerSender,
    total_deadline: tokio::time::Instant,
) -> ControlResponse {
    // Forward the operation and await the controller outcome. The queue holds
    // at most one waiting command (see `router_federation::trigger_channel`), so a second
    // concurrent poke is rejected as busy instead of piling up. Bound the ack
    // wait to the connection's single total deadline.
    let (ack_tx, ack_rx) = oneshot::channel();
    let operation_deadline = total_deadline - RESPONSE_WRITE_BUDGET;
    if tokio::time::Instant::now() >= operation_deadline {
        return ControlResponse::error(
            ControlErrorCode::DeadlineExceeded,
            "identity operation deadline elapsed before dispatch",
        );
    }
    let start_not_after = std::cmp::min(
        operation_deadline,
        tokio::time::Instant::now() + MUTATION_START_ALLOWANCE,
    );
    let trigger = match action {
        IdentityControlAction::EnrollCurrentCredential {
            expected_session_revision,
            expected_pat_subject,
            expected_api_origin,
        } => FederationTrigger::EnrollCurrentCredential {
            expected_session_revision,
            expected_pat_subject,
            expected_api_origin,
            not_after: start_not_after,
            reply: ack_tx,
        },
        IdentityControlAction::PrepareOauthLogin {
            expected_session_revision,
        } => FederationTrigger::PrepareOauthLogin {
            expected_session_revision,
            not_after: start_not_after,
            reply: ack_tx,
        },
        IdentityControlAction::Logout {
            expected_session_revision,
        } => FederationTrigger::Logout {
            expected_session_revision,
            not_after: start_not_after,
            reply: ack_tx,
        },
    };
    match trigger_tx.try_send(trigger) {
        Ok(()) => {}
        Err(TrySendError::Full(_)) => {
            return ControlResponse::error(ControlErrorCode::Busy, "identity task is busy");
        }
        Err(TrySendError::Closed(_)) => {
            return ControlResponse::error(
                ControlErrorCode::Unavailable,
                "identity task is not running",
            );
        }
    }
    match tokio::time::timeout_at(operation_deadline, ack_rx).await {
        Ok(Ok(outcome)) => ControlResponse::from(outcome),
        Ok(Err(_)) => ControlResponse::error(
            ControlErrorCode::Unavailable,
            "identity task dropped the request",
        ),
        Err(_) => ControlResponse::error(
            ControlErrorCode::DeadlineExceeded,
            "timed out applying identity operation",
        ),
    }
}

#[derive(Debug)]
enum RequestReadError {
    TooLarge,
    MissingNewline,
    Invalid,
    Io,
}

impl RequestReadError {
    fn code(&self) -> ControlErrorCode {
        match self {
            Self::Io => ControlErrorCode::Internal,
            Self::TooLarge | Self::MissingNewline | Self::Invalid => {
                ControlErrorCode::InvalidRequest
            }
        }
    }

    fn public_message(&self) -> &'static str {
        match self {
            Self::TooLarge => "control request exceeds the protocol limit",
            Self::MissingNewline => "control request is missing its required newline",
            Self::Invalid => "invalid control request",
            Self::Io => "failed to read control request",
        }
    }
}

async fn read_request(
    reader: &mut (impl AsyncBufReadExt + Unpin),
) -> Result<Option<ControlRequest>, RequestReadError> {
    let mut bytes = Vec::new();
    let mut bounded = reader.take((MAX_REQUEST_LINE_BYTES + 1) as u64);
    bounded
        .read_until(b'\n', &mut bytes)
        .await
        .map_err(|_| RequestReadError::Io)?;
    if bytes.is_empty() {
        return Ok(None);
    }
    if bytes.len() > MAX_REQUEST_LINE_BYTES {
        return Err(RequestReadError::TooLarge);
    }
    if bytes.last() != Some(&b'\n') {
        return Err(RequestReadError::MissingNewline);
    }
    bytes.pop();
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(|_| RequestReadError::Invalid)
}

/// Writes one bounded JSON response line and flushes.
async fn write_response(
    write_half: &mut (impl AsyncWriteExt + Unpin),
    response: ControlResponse,
) -> std::io::Result<()> {
    let mut line = serde_json::to_vec(&response)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
    line.push(b'\n');
    if line.len() > MAX_RESPONSE_LINE_BYTES {
        line = serde_json::to_vec(&ControlResponse::error(
            ControlErrorCode::Internal,
            "control response exceeds the protocol limit",
        ))
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
        line.push(b'\n');
    }
    debug_assert!(line.len() <= MAX_RESPONSE_LINE_BYTES);
    write_half.write_all(&line).await?;
    write_half.flush().await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control::{
        ApplyResult, FEDERATION_CONTROL_SOCK, FederationStatus, LinkState, PlatformLink,
        enroll_current_credential, status,
    };
    use tokio::sync::mpsc;

    async fn send_request(stream: &mut UnixStream, request: ControlRequest) {
        let mut line = serde_json::to_vec(&request).expect("serialize request");
        line.push(b'\n');
        stream.write_all(&line).await.expect("send control request");
    }

    async fn raw_request_response(payload: Vec<u8>, close_write: bool) -> Vec<u8> {
        let (server, mut client) = UnixStream::pair().expect("create control socket pair");
        let (trigger_tx, _trigger_rx) = mpsc::channel::<FederationTrigger>(1);
        let (restart_tx, _restart_rx) = watch::channel(false);
        let (_status_tx, status_rx) = watch::channel(FederationStatus::default());
        let handler = tokio::spawn(handle_conn(
            server,
            trigger_tx,
            status_rx,
            Duration::from_secs(1),
            restart_tx,
        ));
        client.write_all(&payload).await.expect("write raw request");
        if close_write {
            client.shutdown().await.expect("close request half");
        }
        let mut reader = BufReader::new(client);
        let mut line = Vec::new();
        tokio::time::timeout(Duration::from_secs(1), reader.read_until(b'\n', &mut line))
            .await
            .expect("server replies promptly")
            .expect("read server reply");
        handler.await.expect("handler does not panic").unwrap();
        line
    }

    /// The daemon ack budget must cover the verifying poke's post-resolve work
    /// (the TLS probe + bounce), and the client must always outlast the daemon so
    /// it receives a definite reply rather than a client-side timeout. Guards the
    /// constants from drifting back into the pre-probe sizing.
    #[test]
    fn ack_budget_covers_the_verify_probe_and_client_outlasts_daemon() {
        use crate::control::POKE_READ_SLACK;
        use crate::router_federation::{APPLY_TIMEOUT, PROBE_TIMEOUT};
        assert!(
            APPLY_TIMEOUT.saturating_mul(2) + PROBE_TIMEOUT < APPLY_ACK_SLACK,
            "the daemon ack slack must cover fail-closed apply, target apply, and verify probe"
        );
        assert!(
            APPLY_ACK_SLACK < POKE_READ_SLACK,
            "the client must outlast the daemon so it gets a definite reply"
        );
    }

    /// A federation probe failure crosses the wire as an errored link inside
    /// the `ok` ack, distinct from a plain `error`, so the CLI can word it
    /// specifically.
    #[test]
    fn a_link_error_outcome_maps_into_the_ok_ack() {
        let resp = ControlResponse::from(FederationOutcome::Applied(PlatformLink {
            endpoint: Some("tls/hub:7447".to_string()),
            link_state: LinkState::Error("received fatal alert: UnknownCA".to_string()),
        }));
        match resp.response {
            ControlResult::Applied { link } => {
                assert_eq!(link.endpoint.as_deref(), Some("tls/hub:7447"));
                assert_eq!(
                    link.link_state,
                    LinkState::Error("received fatal alert: UnknownCA".to_string())
                );
            }
            other => panic!("expected applied, got {other:?}"),
        }
    }

    #[test]
    fn logout_mapping_preserves_router_state_and_independent_operator_action() {
        let response = ControlResponse::from(FederationOutcome::LoggedOut(
            crate::router_federation::LogoutOperationOutcome {
                certificate_revocation: auth::logout::CleanupAttempt::Failed(
                    "daemon-local detail".into(),
                ),
                oauth_revocation: auth::logout::CleanupAttempt::Succeeded,
                local_cleanup: auth::logout::CleanupAttempt::Succeeded,
                router: LogoutRouterState::Standalone,
                operator_action_required: true,
                target_namespace: Some(config::namespace::Namespace::local()),
            },
        ));

        let ControlResult::LoggedOut { outcome } = response.response else {
            panic!("expected logged_out response")
        };
        assert_eq!(outcome.certificate_revocation, CleanupState::Failed);
        assert_eq!(outcome.oauth_revocation, CleanupState::Succeeded);
        assert_eq!(outcome.local_cleanup, CleanupState::Succeeded);
        assert_eq!(outcome.router_apply, RouterApplyState::Standalone);
        assert!(outcome.operator_action_required);
        assert_eq!(outcome.target_namespace.as_deref(), Some("local"));
    }

    /// A poke from the (sync) CLI client crosses the real control socket, reaches
    /// the trigger channel, and the federation loop's ack comes back to the
    /// client: the end-to-end glue between [`crate::control`] and the
    /// federation loop.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn poke_crosses_the_socket_and_acks() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join(FEDERATION_CONTROL_SOCK);
        let (trigger_tx, mut trigger_rx) = mpsc::channel::<FederationTrigger>(8);

        // Stand in for the federation loop: ack a canned applied outcome.
        let consumer = tokio::spawn(async move {
            if let Some(FederationTrigger::EnrollCurrentCredential {
                expected_session_revision: None,
                expected_pat_subject: Some(subject),
                expected_api_origin: Some(origin),
                not_after: _,
                reply: ack,
            }) = trigger_rx.recv().await
            {
                assert_eq!(subject, "cli-subject");
                assert_eq!(origin, "https://api.peppy.bot");
                let _ = ack.send(FederationOutcome::Applied(PlatformLink {
                    endpoint: Some("tls/hub:7447".to_string()),
                    link_state: LinkState::Verified,
                }));
            }
        });

        // Bind before the client can run, so the connect below is deterministic:
        // a bound listener queues connections in the backlog even while the
        // accept loop is still waiting to be scheduled. (Polling for the socket
        // *file* instead was flaky under load: the file appears between `bind`
        // and `listen`, and the listener task may not even bind within a fixed
        // poll window.)
        let listener = bind_listener(&socket).expect("bind control socket");
        let (restart_tx, _restart_rx) = watch::channel(false);
        let (_status_tx, status_rx) = watch::channel(FederationStatus::default());
        let control = tokio::spawn(accept_loop(
            listener,
            trigger_tx,
            status_rx,
            Duration::from_secs(5),
            restart_tx,
        ));

        // Drive the blocking client off the async workers.
        let socket_for_client = socket.clone();
        let outcome = tokio::task::spawn_blocking(move || {
            enroll_current_credential(
                &socket_for_client,
                Duration::from_secs(5),
                None,
                Some("cli-subject".into()),
                Some("https://api.peppy.bot".into()),
            )
        })
        .await
        .unwrap();

        assert_eq!(
            outcome,
            Ok(ApplyResult::Applied(PlatformLink {
                endpoint: Some("tls/hub:7447".to_string()),
                link_state: LinkState::Verified,
            }))
        );

        control.abort();
        consumer.abort();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn status_crosses_the_socket_without_refederating() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join(FEDERATION_CONTROL_SOCK);
        let (trigger_tx, mut trigger_rx) = mpsc::channel::<FederationTrigger>(8);
        let expected = FederationStatus {
            link: PlatformLink {
                endpoint: Some("tls/hub:7447".to_string()),
                link_state: LinkState::Error("UnknownIssuer".to_string()),
            },
            pinned: false,
            pat_active: false,
            certificate_error: None,
            certificate_renewing: false,
            ..FederationStatus::default()
        };
        let listener = bind_listener(&socket).expect("bind control socket");
        let (restart_tx, _restart_rx) = watch::channel(false);
        let (_status_tx, status_rx) = watch::channel(expected.clone());
        let control = tokio::spawn(accept_loop(
            listener,
            trigger_tx,
            status_rx,
            Duration::from_secs(5),
            restart_tx,
        ));

        let socket_for_client = socket.clone();
        let outcome =
            tokio::task::spawn_blocking(move || status(&socket_for_client, Duration::from_secs(5)))
                .await
                .unwrap();

        assert_eq!(outcome, Ok(expected));
        assert!(
            trigger_rx.try_recv().is_err(),
            "a status query must never poke the federation loop"
        );
        control.abort();
    }

    /// Status is answered from the watch even while the poke queue is full
    /// (a refederation in flight), so it can never queue behind an apply.
    #[tokio::test]
    async fn status_is_answered_while_the_poke_queue_is_full() {
        let (server, mut client) = UnixStream::pair().expect("create control socket pair");
        let (trigger_tx, _trigger_rx) = mpsc::channel::<FederationTrigger>(1);
        let (queued_ack, _queued_rx) = oneshot::channel();
        trigger_tx
            .try_send(FederationTrigger::EnrollCurrentCredential {
                expected_session_revision: None,
                expected_pat_subject: Some("cli-subject".into()),
                expected_api_origin: Some("https://api.peppy.bot".into()),
                not_after: tokio::time::Instant::now() + Duration::from_secs(60),
                reply: queued_ack,
            })
            .expect("prefill trigger channel");
        let (restart_tx, _restart_rx) = watch::channel(false);
        let expected = FederationStatus {
            link: PlatformLink {
                endpoint: Some("tls/hub:7447".to_string()),
                link_state: LinkState::Unverified,
            },
            pinned: false,
            pat_active: false,
            certificate_error: None,
            certificate_renewing: false,
            ..FederationStatus::default()
        };
        let (_status_tx, status_rx) = watch::channel(expected.clone());
        let handler = tokio::spawn(handle_conn(
            server,
            trigger_tx,
            status_rx,
            Duration::from_secs(1),
            restart_tx,
        ));

        send_request(&mut client, ControlRequest::status()).await;

        let mut reader = BufReader::new(client);
        let mut line = String::new();
        tokio::time::timeout(Duration::from_secs(1), reader.read_line(&mut line))
            .await
            .expect("status must be answered promptly with the queue full")
            .expect("read status response");
        assert!(line.contains("tls/hub:7447"), "reply: {line}");
        handler.await.expect("handler does not panic").unwrap();
    }

    /// With one poke already queued, a second concurrent poke is rejected as
    /// busy instead of piling up behind the in-progress apply.
    #[tokio::test]
    async fn a_second_concurrent_poke_is_rejected_as_busy() {
        let (server, mut client) = UnixStream::pair().expect("create control socket pair");
        let (trigger_tx, _trigger_rx) = mpsc::channel::<FederationTrigger>(1);
        let (queued_ack, _queued_rx) = oneshot::channel();
        trigger_tx
            .try_send(FederationTrigger::EnrollCurrentCredential {
                expected_session_revision: None,
                expected_pat_subject: Some("cli-subject".into()),
                expected_api_origin: Some("https://api.peppy.bot".into()),
                not_after: tokio::time::Instant::now() + Duration::from_secs(60),
                reply: queued_ack,
            })
            .expect("prefill trigger channel");
        let (restart_tx, _restart_rx) = watch::channel(false);
        let (_status_tx, status_rx) = watch::channel(FederationStatus::default());
        let handler = tokio::spawn(handle_conn(
            server,
            trigger_tx,
            status_rx,
            Duration::from_secs(1),
            restart_tx,
        ));

        send_request(
            &mut client,
            ControlRequest::enroll_current_credential(
                None,
                Some("cli-subject".into()),
                Some("https://api.peppy.bot".into()),
            ),
        )
        .await;

        let mut reader = BufReader::new(client);
        let mut line = String::new();
        tokio::time::timeout(Duration::from_secs(1), reader.read_line(&mut line))
            .await
            .expect("a full queue must be rejected promptly")
            .expect("read busy response");
        assert!(line.contains("identity task is busy"), "reply: {line}");
        handler.await.expect("handler does not panic").unwrap();
    }

    /// An unknown typed operation is rejected without ever poking the
    /// federation loop.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn unknown_verb_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join(FEDERATION_CONTROL_SOCK);
        let (trigger_tx, mut trigger_rx) = mpsc::channel::<FederationTrigger>(8);

        // Bind-before-client, as in `poke_crosses_the_socket_and_acks`.
        let listener = bind_listener(&socket).expect("bind control socket");
        let (restart_tx, _restart_rx) = watch::channel(false);
        let (_status_tx, status_rx) = watch::channel(FederationStatus::default());
        let control = tokio::spawn(accept_loop(
            listener,
            trigger_tx,
            status_rx,
            Duration::from_secs(5),
            restart_tx,
        ));

        let socket_for_client = socket.clone();
        let reply = tokio::task::spawn_blocking(move || {
            use std::io::{BufRead, BufReader, Write};
            use std::os::unix::net::UnixStream;
            let mut stream = UnixStream::connect(&socket_for_client).unwrap();
            stream
                .write_all(b"{\"protocol_version\":1,\"request\":{\"operation\":\"bogus\"}}\n")
                .unwrap();
            let mut reader = BufReader::new(stream);
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            line
        })
        .await
        .unwrap();

        assert!(
            reply.contains("invalid_request"),
            "unknown operation => error reply: {reply}"
        );
        // The federation loop was never poked.
        assert!(trigger_rx.try_recv().is_err());
        control.abort();
    }

    #[tokio::test]
    async fn legacy_raw_commands_are_not_accepted() {
        for command in [
            b"refederate\n".to_vec(),
            b"defederate\n".to_vec(),
            b"status\n".to_vec(),
        ] {
            let reply = raw_request_response(command, false).await;
            let response: ControlResponse = serde_json::from_slice(&reply).unwrap();
            assert!(matches!(
                response.response,
                ControlResult::Error {
                    code: ControlErrorCode::InvalidRequest,
                    ..
                }
            ));
        }
    }

    #[tokio::test]
    async fn request_size_newline_and_version_are_enforced() {
        let oversized = raw_request_response(vec![b'x'; MAX_REQUEST_LINE_BYTES + 1], false).await;
        let missing_newline = raw_request_response(
            br#"{"protocol_version":1,"request":{"operation":"hello"}}"#.to_vec(),
            true,
        )
        .await;
        let wrong_version = raw_request_response(
            b"{\"protocol_version\":2,\"request\":{\"operation\":\"hello\"}}\n".to_vec(),
            false,
        )
        .await;

        for (reply, expected_code) in [
            (oversized, ControlErrorCode::InvalidRequest),
            (missing_newline, ControlErrorCode::InvalidRequest),
            (wrong_version, ControlErrorCode::UnsupportedProtocol),
        ] {
            assert!(reply.len() <= MAX_RESPONSE_LINE_BYTES);
            assert_eq!(reply.last(), Some(&b'\n'));
            let response: ControlResponse = serde_json::from_slice(&reply).unwrap();
            assert!(matches!(
                response.response,
                ControlResult::Error { code, .. } if code == expected_code
            ));
        }
    }

    #[tokio::test]
    async fn oversized_status_is_replaced_by_a_bounded_error() {
        let (server, mut client) = UnixStream::pair().expect("create control socket pair");
        let (trigger_tx, _trigger_rx) = mpsc::channel::<FederationTrigger>(1);
        let (restart_tx, _restart_rx) = watch::channel(false);
        let (_status_tx, status_rx) = watch::channel(FederationStatus {
            certificate_error: Some("x".repeat(MAX_RESPONSE_LINE_BYTES * 2)),
            ..FederationStatus::default()
        });
        let handler = tokio::spawn(handle_conn(
            server,
            trigger_tx,
            status_rx,
            Duration::from_secs(1),
            restart_tx,
        ));
        send_request(&mut client, ControlRequest::status()).await;

        let mut reader = BufReader::new(client);
        let mut reply = Vec::new();
        reader.read_until(b'\n', &mut reply).await.unwrap();
        assert!(reply.len() <= MAX_RESPONSE_LINE_BYTES);
        let response: ControlResponse = serde_json::from_slice(&reply).unwrap();
        assert!(matches!(
            response.response,
            ControlResult::Error {
                code: ControlErrorCode::Internal,
                ..
            }
        ));
        handler.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn runtime_directory_socket_and_peer_are_owner_only() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let runtime = dir.path().join("runtime");
        let socket = runtime.join(FEDERATION_CONTROL_SOCK);
        let listener = bind_listener(&socket).unwrap();
        assert_eq!(
            std::fs::metadata(&runtime).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            std::fs::metadata(&socket).unwrap().permissions().mode() & 0o777,
            0o600
        );

        let client = UnixStream::connect(&socket).await.unwrap();
        let (server, _) = listener.accept().await.unwrap();
        validate_peer_uid(&server).unwrap();
        drop(client);
    }

    /// Once the federation loop detects a namespace change, a client that exits
    /// before reading the ack must not strand the daemon in its old namespace.
    #[tokio::test]
    async fn restart_is_signaled_when_the_client_disconnects_before_the_ack() {
        let (server, mut client) = UnixStream::pair().expect("create control socket pair");
        let (trigger_tx, mut trigger_rx) = mpsc::channel::<FederationTrigger>(1);
        let (restart_tx, mut restart_rx) = watch::channel(false);
        let (_status_tx, status_rx) = watch::channel(FederationStatus::default());

        let handler = tokio::spawn(handle_conn(
            server,
            trigger_tx,
            status_rx,
            Duration::from_secs(1),
            restart_tx,
        ));
        send_request(
            &mut client,
            ControlRequest::enroll_current_credential(
                None,
                Some("cli-subject".into()),
                Some("https://api.peppy.bot".into()),
            ),
        )
        .await;
        drop(client);

        let trigger = tokio::time::timeout(Duration::from_secs(1), trigger_rx.recv())
            .await
            .expect("handler forwards request promptly")
            .expect("request channel remains open");
        let FederationTrigger::EnrollCurrentCredential {
            expected_session_revision: None,
            expected_pat_subject: Some(subject),
            expected_api_origin: Some(origin),
            not_after: _,
            reply: ack,
        } = trigger
        else {
            panic!("restart test sent an unexpected forced-standalone request")
        };
        assert_eq!(subject, "cli-subject");
        assert_eq!(origin, "https://api.peppy.bot");
        ack.send(FederationOutcome::Restart {
            target_namespace: config::namespace::Namespace::parse(
                "550e8400-e29b-41d4-a716-446655440000",
            )
            .expect("valid test namespace"),
        })
        .expect("handler is awaiting the outcome");

        let _write_error = tokio::time::timeout(Duration::from_secs(1), handler)
            .await
            .expect("handler completes promptly")
            .expect("handler task does not panic")
            .expect_err("replying to a disconnected client fails");
        tokio::time::timeout(
            Duration::from_secs(1),
            restart_rx.wait_for(|restart| *restart),
        )
        .await
        .expect("restart signal is not suppressed by the write failure")
        .expect("restart sender remains open");
    }

    #[tokio::test]
    async fn cancelling_a_pending_restart_response_still_signals_restart() {
        struct PendingWriter;

        impl tokio::io::AsyncWrite for PendingWriter {
            fn poll_write(
                self: std::pin::Pin<&mut Self>,
                _cx: &mut std::task::Context<'_>,
                _buffer: &[u8],
            ) -> std::task::Poll<std::io::Result<usize>> {
                std::task::Poll::Pending
            }

            fn poll_flush(
                self: std::pin::Pin<&mut Self>,
                _cx: &mut std::task::Context<'_>,
            ) -> std::task::Poll<std::io::Result<()>> {
                std::task::Poll::Ready(Ok(()))
            }

            fn poll_shutdown(
                self: std::pin::Pin<&mut Self>,
                _cx: &mut std::task::Context<'_>,
            ) -> std::task::Poll<std::io::Result<()>> {
                std::task::Poll::Ready(Ok(()))
            }
        }

        let (restart_tx, restart_rx) = watch::channel(false);
        let mut writer = PendingWriter;
        {
            let mut pending = Box::pin(write_response_then_restart(
                &mut writer,
                ControlResponse::new(ControlResult::Restarting {
                    target_namespace: "550e8400-e29b-41d4-a716-446655440000".into(),
                }),
                Some(restart_tx),
            ));
            assert!(
                tokio::time::timeout(Duration::from_millis(10), pending.as_mut())
                    .await
                    .is_err()
            );
        }

        assert!(
            *restart_rx.borrow(),
            "dropping the pending write future must arm the namespace restart"
        );
    }
}
