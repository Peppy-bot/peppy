//! Versioned client and wire contract for the daemon's local identity-control
//! socket.
//!
//! The protocol is one UTF-8 JSON request line followed by one UTF-8 JSON
//! response line. Every envelope carries [`PROTOCOL_VERSION`], both envelopes
//! reject unknown fields, and there is deliberately no parser for the former
//! raw `refederate`/`defederate`/`status` commands. The Unix-domain socket keeps
//! the acknowledgement independent of the local router that an identity
//! operation may restart.

#[cfg(test)]
use std::io::{BufRead, BufReader};
use std::io::{ErrorKind, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use daemon_config::consts::PeppyDirs;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// The only protocol version understood by this pre-production, clean-slate
/// control surface.
pub const PROTOCOL_VERSION: u16 = 1;

/// File name of the daemon's identity-control socket under the private runtime
/// directory. The name is retained to avoid introducing a second stale socket
/// path while the surrounding identity-controller refactor lands.
pub const FEDERATION_CONTROL_SOCK: &str = "federation_control.sock";

/// Maximum request line size, including its required newline.
pub const MAX_REQUEST_LINE_BYTES: usize = 16 * 1024;

/// Maximum response line size, including its required newline.
pub const MAX_RESPONSE_LINE_BYTES: usize = 64 * 1024;

/// Maximum public error-message size, in UTF-8 bytes.
pub const MAX_ERROR_MESSAGE_BYTES: usize = 2 * 1024;

/// Extra time the client waits for the daemon's reply on top of the configured
/// identity/federation operation timeout. This remains strictly larger than the
/// daemon-side acknowledgement slack.
pub const POKE_READ_SLACK: Duration = Duration::from_secs(16);

/// Where the daemon binds and local clients connect.
pub fn federation_control_socket_path(peppy_dirs: &PeppyDirs) -> PathBuf {
    peppy_dirs
        .runtime_config_dir()
        .join(FEDERATION_CONTROL_SOCK)
}

/// Health of the daemon's single platform link as it last saw it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LinkState {
    #[default]
    NotConfigured,
    Unverified,
    Verified,
    Error(String),
}

/// The platform link applied by an identity operation.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlatformLink {
    pub endpoint: Option<String>,
    pub link_state: LinkState,
}

/// Sanitized authentication source known to the daemon. This reports only the
/// credential class; tokens, subjects, and session revisions are never exposed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthenticationState {
    #[default]
    Missing,
    Oauth,
    Pat,
}

/// Sanitized lifecycle state of the daemon-owned client certificate.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CertificateState {
    #[default]
    Missing,
    Enrolling,
    Valid,
    Renewing,
    Expiring,
    Expired,
    Error,
}

/// Last known disposition of applying identity state to the router.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RouterApplyState {
    #[default]
    Standalone,
    Applied,
    OperatorManaged,
    Error,
}

/// Sanitized result of one logout cleanup stage.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CleanupState {
    #[default]
    NotNeeded,
    Succeeded,
    Failed,
}

/// Structured outcome of a daemon-owned logout. Each field is deliberately a
/// bounded enum or boolean rather than a raw backend diagnostic.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LogoutResult {
    pub certificate_revocation: CleanupState,
    pub oauth_revocation: CleanupState,
    pub router_apply: RouterApplyState,
    pub local_cleanup: CleanupState,
    pub operator_action_required: bool,
    /// Namespace the daemon will restart into after flushing this response.
    /// `None` means the current generation namespace remains valid.
    pub target_namespace: Option<String>,
}

/// Cached daemon identity/federation state returned by [`status`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FederationStatus {
    /// The current daemon generation completed its initial identity/router
    /// reconciliation. Seeded startup state is deliberately not login-ready.
    pub controller_settled: bool,
    pub authentication: AuthenticationState,
    pub certificate: CertificateState,
    pub bound_core_node_name: Option<String>,
    pub certificate_expiry_unix: Option<i64>,
    pub generation: Option<String>,
    pub next_retry_after_secs: Option<u64>,
    pub router_apply_state: RouterApplyState,
    pub operator_managed: bool,
    pub offline_recovery_required: bool,
    // Transitional internal consumers still use the fields below. They remain
    // required/strict on the v1 wire unless explicitly annotated optional.
    pub link: PlatformLink,
    pub pinned: bool,
    /// Whether the daemon observed a PAT in its own environment. The PAT itself
    /// never enters this type or the wire protocol.
    #[serde(default, skip_serializing_if = "is_false")]
    pub pat_active: bool,
    /// Latest explicitly non-secret certificate maintenance failure.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub certificate_error: Option<String>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub certificate_renewing: bool,
}

fn is_false(value: &bool) -> bool {
    !*value
}

/// Strict request envelope. Operations carry only identifiers and expected
/// revisions; bearer tokens, private keys, and certificate PEM are not protocol
/// fields.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ControlRequest {
    pub protocol_version: u16,
    pub request: ControlOperation,
}

impl ControlRequest {
    pub fn new(request: ControlOperation) -> Self {
        Self {
            protocol_version: PROTOCOL_VERSION,
            request,
        }
    }

    pub fn hello() -> Self {
        Self::new(ControlOperation::Hello)
    }

    pub fn enroll_current_credential(
        expected_session_revision: Option<Uuid>,
        expected_pat_subject: Option<String>,
        expected_api_origin: Option<String>,
    ) -> Self {
        Self::new(ControlOperation::EnrollCurrentCredential {
            expected_session_revision,
            expected_pat_subject,
            expected_api_origin,
        })
    }

    pub fn prepare_oauth_login(expected_session_revision: Uuid) -> Self {
        Self::new(ControlOperation::PrepareOauthLogin {
            expected_session_revision,
        })
    }

    pub fn logout(expected_session_revision: Option<Uuid>) -> Self {
        Self::new(ControlOperation::Logout {
            expected_session_revision,
        })
    }

    pub fn status() -> Self {
        Self::new(ControlOperation::Status)
    }
}

/// Version-1 identity-control operations.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "operation", rename_all = "snake_case")]
pub enum ControlOperation {
    Hello,
    EnrollCurrentCredential {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        expected_session_revision: Option<Uuid>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        expected_pat_subject: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        expected_api_origin: Option<String>,
    },
    PrepareOauthLogin {
        expected_session_revision: Uuid,
    },
    Logout {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        expected_session_revision: Option<Uuid>,
    },
    Status,
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum ControlOperationTag {
    Hello,
    EnrollCurrentCredential,
    PrepareOauthLogin,
    Logout,
    Status,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StrictControlOperation {
    operation: ControlOperationTag,
    #[serde(default)]
    expected_session_revision: Option<Uuid>,
    #[serde(default)]
    expected_pat_subject: Option<String>,
    #[serde(default)]
    expected_api_origin: Option<String>,
}

impl<'de> Deserialize<'de> for ControlOperation {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use serde::de::Error as _;

        let wire = StrictControlOperation::deserialize(deserializer)?;
        match (
            wire.operation,
            wire.expected_session_revision,
            wire.expected_pat_subject,
            wire.expected_api_origin,
        ) {
            (ControlOperationTag::Hello, None, None, None) => Ok(Self::Hello),
            (
                ControlOperationTag::EnrollCurrentCredential,
                expected_session_revision,
                expected_pat_subject,
                expected_api_origin,
            ) if matches!(
                (
                    &expected_session_revision,
                    &expected_pat_subject,
                    &expected_api_origin
                ),
                (Some(_), None, None) | (None, Some(_), Some(_))
            ) && expected_pat_subject
                .as_ref()
                .is_none_or(|subject| !subject.is_empty() && subject.len() <= 1024)
                && expected_api_origin
                    .as_ref()
                    .is_none_or(|origin| !origin.is_empty() && origin.len() <= 2048) =>
            {
                Ok(Self::EnrollCurrentCredential {
                    expected_session_revision,
                    expected_pat_subject,
                    expected_api_origin,
                })
            }
            (
                ControlOperationTag::PrepareOauthLogin,
                Some(expected_session_revision),
                None,
                None,
            ) => Ok(Self::PrepareOauthLogin {
                expected_session_revision,
            }),
            (ControlOperationTag::Logout, expected_session_revision, None, None) => {
                Ok(Self::Logout {
                    expected_session_revision,
                })
            }
            (ControlOperationTag::Status, None, None, None) => Ok(Self::Status),
            _ => Err(D::Error::custom(
                "enroll_current_credential requires either one OAuth revision or a bounded PAT principal and API origin; logout alone accepts an optional revision",
            )),
        }
    }
}

/// Stable, machine-readable daemon error classes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ControlErrorCode {
    InvalidRequest,
    UnsupportedProtocol,
    Busy,
    Unavailable,
    DeadlineExceeded,
    StaleSessionRevision,
    NotAuthenticated,
    PatNotConfigured,
    PatActive,
    PatPrincipalMismatch,
    PatOriginMismatch,
    OperationFailed,
    Internal,
}

/// Strict response envelope.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ControlResponse {
    pub protocol_version: u16,
    pub response: ControlResult,
}

impl ControlResponse {
    pub fn new(response: ControlResult) -> Self {
        Self {
            protocol_version: PROTOCOL_VERSION,
            response,
        }
    }

    pub fn hello() -> Self {
        Self::new(ControlResult::Hello)
    }

    pub fn error(code: ControlErrorCode, message: impl AsRef<str>) -> Self {
        Self::new(ControlResult::Error {
            code,
            message: sanitize_error(message.as_ref()),
        })
    }
}

/// Version-1 operation results. The wire never contains credentials, key
/// material, certificate bodies, or raw internal errors.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum ControlResult {
    Hello,
    Applied {
        link: PlatformLink,
    },
    Status {
        status: FederationStatus,
    },
    OperatorManaged,
    LoggedOut {
        outcome: LogoutResult,
    },
    Error {
        code: ControlErrorCode,
        message: String,
    },
    Restarting {
        target_namespace: String,
    },
}

#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum ControlResultTag {
    Hello,
    Applied,
    Status,
    OperatorManaged,
    LoggedOut,
    Error,
    Restarting,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EmptyResultWire {
    result: ControlResultTag,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AppliedResultWire {
    result: ControlResultTag,
    link: PlatformLink,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StatusResultWire {
    result: ControlResultTag,
    status: FederationStatus,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LoggedOutResultWire {
    result: ControlResultTag,
    outcome: LogoutResult,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ErrorResultWire {
    result: ControlResultTag,
    code: ControlErrorCode,
    message: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RestartingResultWire {
    result: ControlResultTag,
    target_namespace: String,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum StrictControlResult {
    Applied(AppliedResultWire),
    Status(StatusResultWire),
    LoggedOut(LoggedOutResultWire),
    Error(ErrorResultWire),
    Restarting(RestartingResultWire),
    Empty(EmptyResultWire),
}

impl<'de> Deserialize<'de> for ControlResult {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use serde::de::Error as _;

        match StrictControlResult::deserialize(deserializer)? {
            StrictControlResult::Applied(AppliedResultWire {
                result: ControlResultTag::Applied,
                link,
            }) => Ok(Self::Applied { link }),
            StrictControlResult::Status(StatusResultWire {
                result: ControlResultTag::Status,
                status,
            }) => Ok(Self::Status { status }),
            StrictControlResult::LoggedOut(LoggedOutResultWire {
                result: ControlResultTag::LoggedOut,
                outcome,
            }) => Ok(Self::LoggedOut { outcome }),
            StrictControlResult::Error(ErrorResultWire {
                result: ControlResultTag::Error,
                code,
                message,
            }) => Ok(Self::Error { code, message }),
            StrictControlResult::Restarting(RestartingResultWire {
                result: ControlResultTag::Restarting,
                target_namespace,
            }) => Ok(Self::Restarting { target_namespace }),
            StrictControlResult::Empty(EmptyResultWire {
                result: ControlResultTag::Hello,
            }) => Ok(Self::Hello),
            StrictControlResult::Empty(EmptyResultWire {
                result: ControlResultTag::OperatorManaged,
            }) => Ok(Self::OperatorManaged),
            _ => Err(D::Error::custom(
                "control result fields do not match its result tag",
            )),
        }
    }
}

/// Successful result of an identity-changing control operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApplyResult {
    Applied(PlatformLink),
    OperatorManaged,
    Restarting { target_namespace: String },
}

/// Structured client failures. Protocol error codes remain typed instead of
/// being collapsed into transport strings.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ControlClientError {
    #[error("daemon is not running")]
    DaemonNotRunning,
    #[error("control request timed out")]
    TimedOut,
    #[error("control transport failed: {0}")]
    Transport(String),
    #[error("daemon protocol version {actual} is incompatible with client version {expected}")]
    ProtocolVersion { expected: u16, actual: u16 },
    #[error("daemon rejected the request ({code:?}): {message}")]
    Daemon {
        code: ControlErrorCode,
        message: String,
    },
    #[error("daemon returned {actual} to a request expecting {expected}")]
    UnexpectedResponse {
        expected: &'static str,
        actual: &'static str,
    },
}

/// Typed version handshake.
pub fn hello(socket_path: &Path, total_timeout: Duration) -> Result<(), ControlClientError> {
    match request(socket_path, total_timeout, &ControlRequest::hello())?.response {
        ControlResult::Hello => Ok(()),
        other => Err(unexpected("hello", &other)),
    }
}

/// Ask the daemon to enroll/apply the current credential, optionally guarded by
/// a session revision.
pub fn enroll_current_credential(
    socket_path: &Path,
    total_timeout: Duration,
    expected_session_revision: Option<Uuid>,
    expected_pat_subject: Option<String>,
    expected_api_origin: Option<String>,
) -> Result<ApplyResult, ControlClientError> {
    apply_request(
        socket_path,
        total_timeout,
        ControlRequest::enroll_current_credential(
            expected_session_revision,
            expected_pat_subject,
            expected_api_origin,
        ),
    )
}

/// Atomically reject daemon-PAT mode or prepare a fail-closed OAuth handoff.
pub fn prepare_oauth_login(
    socket_path: &Path,
    total_timeout: Duration,
    expected_session_revision: Uuid,
) -> Result<ApplyResult, ControlClientError> {
    apply_request(
        socket_path,
        total_timeout,
        ControlRequest::prepare_oauth_login(expected_session_revision),
    )
}

/// Ask the daemon to own logout, optionally guarded by a session revision.
pub fn logout(
    socket_path: &Path,
    total_timeout: Duration,
    expected_session_revision: Option<Uuid>,
) -> Result<LogoutResult, ControlClientError> {
    match request(
        socket_path,
        total_timeout,
        &ControlRequest::logout(expected_session_revision),
    )?
    .response
    {
        ControlResult::LoggedOut { outcome } => Ok(outcome),
        other => Err(unexpected("logout result", &other)),
    }
}

/// Query cached state without scheduling identity or router work.
pub fn status(
    socket_path: &Path,
    total_timeout: Duration,
) -> Result<FederationStatus, ControlClientError> {
    match request(socket_path, total_timeout, &ControlRequest::status())?.response {
        ControlResult::Status { status } => Ok(status),
        other => Err(unexpected("status", &other)),
    }
}

fn apply_request(
    socket_path: &Path,
    total_timeout: Duration,
    control_request: ControlRequest,
) -> Result<ApplyResult, ControlClientError> {
    match request(socket_path, total_timeout, &control_request)?.response {
        ControlResult::Applied { link } => Ok(ApplyResult::Applied(link)),
        ControlResult::OperatorManaged => Ok(ApplyResult::OperatorManaged),
        ControlResult::Restarting { target_namespace } => {
            Ok(ApplyResult::Restarting { target_namespace })
        }
        other => Err(unexpected("identity operation result", &other)),
    }
}

fn unexpected(expected: &'static str, response: &ControlResult) -> ControlClientError {
    if let ControlResult::Error { code, message } = response {
        return ControlClientError::Daemon {
            code: *code,
            message: sanitize_error(message),
        };
    }
    ControlClientError::UnexpectedResponse {
        expected,
        actual: response.kind(),
    }
}

impl ControlResult {
    fn kind(&self) -> &'static str {
        match self {
            Self::Hello => "hello",
            Self::Applied { .. } => "applied",
            Self::Status { .. } => "status",
            Self::OperatorManaged => "operator_managed",
            Self::LoggedOut { .. } => "logged_out",
            Self::Error { .. } => "error",
            Self::Restarting { .. } => "restarting",
        }
    }
}

fn request(
    socket_path: &Path,
    total_timeout: Duration,
    request: &ControlRequest,
) -> Result<ControlResponse, ControlClientError> {
    let deadline = Instant::now() + total_timeout;
    let mut stream = connect_until(socket_path, deadline)?;

    let mut request_line = serde_json::to_vec(request)
        .map_err(|error| ControlClientError::Transport(error.to_string()))?;
    request_line.push(b'\n');
    if request_line.len() > MAX_REQUEST_LINE_BYTES {
        return Err(ControlClientError::Transport(
            "serialized control request exceeds the protocol limit".into(),
        ));
    }

    write_all_until(&mut stream, &request_line, deadline)?;
    let response_line = read_bounded_line_until(&mut stream, MAX_RESPONSE_LINE_BYTES, deadline)?;
    let response: ControlResponse = serde_json::from_slice(&response_line).map_err(|_| {
        ControlClientError::Transport("daemon returned an invalid control response".into())
    })?;
    if response.protocol_version != PROTOCOL_VERSION {
        return Err(ControlClientError::ProtocolVersion {
            expected: PROTOCOL_VERSION,
            actual: response.protocol_version,
        });
    }
    Ok(response)
}

fn connect_until(socket_path: &Path, deadline: Instant) -> Result<UnixStream, ControlClientError> {
    use rustix::io::Errno;
    use rustix::net::{
        AddressFamily, SocketAddrUnix, SocketFlags, SocketType, connect, socket_with,
        sockopt::socket_error,
    };

    let socket = socket_with(
        AddressFamily::UNIX,
        SocketType::STREAM,
        SocketFlags::CLOEXEC | SocketFlags::NONBLOCK,
        None,
    )
    .map_err(|error| classify_io(error.into()))?;
    let address = SocketAddrUnix::new(socket_path).map_err(|error| classify_io(error.into()))?;
    match connect(&socket, &address) {
        Ok(()) => {}
        Err(error)
            if error == Errno::INPROGRESS
                || error == Errno::AGAIN
                || error == Errno::WOULDBLOCK =>
        {
            wait_until(&socket, rustix::event::PollFlags::OUT, deadline)?;
            match socket_error(&socket).map_err(|error| classify_io(error.into()))? {
                Ok(()) => {}
                Err(error) => return Err(classify_io(error.into())),
            }
        }
        Err(error) => return Err(classify_io(error.into())),
    }
    Ok(UnixStream::from(socket))
}

fn wait_until(
    fd: &impl std::os::fd::AsFd,
    readiness: rustix::event::PollFlags,
    deadline: Instant,
) -> Result<(), ControlClientError> {
    use rustix::event::{PollFd, Timespec, poll};

    loop {
        let timeout = Timespec::try_from(remaining(deadline)?).map_err(|_| {
            ControlClientError::Transport("control deadline is out of range".into())
        })?;
        let mut descriptor = [PollFd::new(fd, readiness)];
        match poll(&mut descriptor, Some(&timeout)) {
            Ok(0) => return Err(ControlClientError::TimedOut),
            Ok(_) => return Ok(()),
            Err(rustix::io::Errno::INTR) => continue,
            Err(error) => return Err(classify_io(error.into())),
        }
    }
}

fn write_all_until(
    stream: &mut UnixStream,
    mut bytes: &[u8],
    deadline: Instant,
) -> Result<(), ControlClientError> {
    while !bytes.is_empty() {
        match stream.write(bytes) {
            Ok(0) => {
                return Err(ControlClientError::Transport(
                    "daemon closed the control connection while receiving the request".into(),
                ));
            }
            Ok(written) => bytes = &bytes[written..],
            Err(error) if error.kind() == ErrorKind::WouldBlock => {
                wait_until(stream, rustix::event::PollFlags::OUT, deadline)?;
            }
            Err(error) if error.kind() == ErrorKind::Interrupted => {}
            Err(error) => return Err(classify_io(error)),
        }
    }
    Ok(())
}

fn remaining(deadline: Instant) -> Result<Duration, ControlClientError> {
    deadline
        .checked_duration_since(Instant::now())
        .filter(|remaining| !remaining.is_zero())
        .ok_or(ControlClientError::TimedOut)
}

fn read_bounded_line_until(
    stream: &mut UnixStream,
    maximum_including_newline: usize,
    deadline: Instant,
) -> Result<Vec<u8>, ControlClientError> {
    let mut bytes = Vec::new();
    let mut chunk = [0_u8; 4096];
    loop {
        let capacity = (maximum_including_newline + 1 - bytes.len()).min(chunk.len());
        match stream.read(&mut chunk[..capacity]) {
            Ok(0) if bytes.is_empty() => {
                return Err(ControlClientError::Transport(
                    "daemon closed the control connection before replying".into(),
                ));
            }
            Ok(0) => {
                return Err(ControlClientError::Transport(
                    "daemon response is missing its required newline".into(),
                ));
            }
            Ok(read) => {
                if let Some(newline) = chunk[..read].iter().position(|byte| *byte == b'\n') {
                    bytes.extend_from_slice(&chunk[..=newline]);
                    if bytes.len() > maximum_including_newline {
                        return Err(ControlClientError::Transport(
                            "daemon response exceeds the protocol limit".into(),
                        ));
                    }
                    bytes.pop();
                    return Ok(bytes);
                }
                bytes.extend_from_slice(&chunk[..read]);
                if bytes.len() > maximum_including_newline {
                    return Err(ControlClientError::Transport(
                        "daemon response exceeds the protocol limit".into(),
                    ));
                }
            }
            Err(error) if error.kind() == ErrorKind::WouldBlock => {
                wait_until(stream, rustix::event::PollFlags::IN, deadline)?;
            }
            Err(error) if error.kind() == ErrorKind::Interrupted => {}
            Err(error) => return Err(classify_io(error)),
        }
    }
}

fn classify_io(error: std::io::Error) -> ControlClientError {
    match error.kind() {
        ErrorKind::WouldBlock | ErrorKind::TimedOut => ControlClientError::TimedOut,
        ErrorKind::NotFound | ErrorKind::ConnectionRefused => ControlClientError::DaemonNotRunning,
        _ => ControlClientError::Transport(error.to_string()),
    }
}

/// Removes control characters and truncates on a UTF-8 boundary. Callers must
/// still pass only public diagnostics; raw backend bodies and credentials are
/// not accepted by the protocol types.
pub(crate) fn sanitize_error(message: &str) -> String {
    let mut sanitized = String::with_capacity(message.len().min(MAX_ERROR_MESSAGE_BYTES));
    for character in message.chars() {
        let character = if character.is_control() {
            ' '
        } else {
            character
        };
        if sanitized.len() + character.len_utf8() > MAX_ERROR_MESSAGE_BYTES {
            break;
        }
        sanitized.push(character);
    }
    let trimmed = sanitized.trim();
    if trimmed.is_empty() {
        "operation failed".to_string()
    } else if trimmed.len() == sanitized.len() {
        sanitized
    } else {
        trimmed.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;

    fn stub_daemon(
        path: PathBuf,
        reply: impl FnOnce(&str, &mut UnixStream) + Send + 'static,
    ) -> std::thread::JoinHandle<String> {
        let listener = UnixListener::bind(&path).expect("bind stub socket");
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            let mut reader = BufReader::new(stream.try_clone().expect("clone"));
            let mut line = String::new();
            reader.read_line(&mut line).expect("read request");
            reply(&line, &mut stream);
            line
        })
    }

    #[test]
    fn request_wire_shapes_are_exact() {
        let revision = Uuid::parse_str("550e8400-e29b-41d4-a716-446655440000").unwrap();
        let cases = [
            (
                ControlRequest::hello(),
                r#"{"protocol_version":1,"request":{"operation":"hello"}}"#,
            ),
            (
                ControlRequest::enroll_current_credential(Some(revision), None, None),
                r#"{"protocol_version":1,"request":{"operation":"enroll_current_credential","expected_session_revision":"550e8400-e29b-41d4-a716-446655440000"}}"#,
            ),
            (
                ControlRequest::enroll_current_credential(
                    None,
                    Some("subject-a".into()),
                    Some("https://api.peppy.bot".into()),
                ),
                r#"{"protocol_version":1,"request":{"operation":"enroll_current_credential","expected_pat_subject":"subject-a","expected_api_origin":"https://api.peppy.bot"}}"#,
            ),
            (
                ControlRequest::prepare_oauth_login(revision),
                r#"{"protocol_version":1,"request":{"operation":"prepare_oauth_login","expected_session_revision":"550e8400-e29b-41d4-a716-446655440000"}}"#,
            ),
            (
                ControlRequest::logout(None),
                r#"{"protocol_version":1,"request":{"operation":"logout"}}"#,
            ),
            (
                ControlRequest::status(),
                r#"{"protocol_version":1,"request":{"operation":"status"}}"#,
            ),
        ];
        for (request, golden) in cases {
            assert_eq!(serde_json::to_string(&request).unwrap(), golden);
        }
    }

    #[test]
    fn response_wire_shapes_are_exact() {
        let status = FederationStatus {
            link: PlatformLink {
                endpoint: Some("tls/hub.example:7447".into()),
                link_state: LinkState::Verified,
            },
            pinned: false,
            ..FederationStatus::default()
        };
        assert_eq!(
            serde_json::to_string(&ControlResponse::hello()).unwrap(),
            r#"{"protocol_version":1,"response":{"result":"hello"}}"#
        );
        assert_eq!(
            serde_json::to_string(&ControlResponse::new(ControlResult::OperatorManaged)).unwrap(),
            r#"{"protocol_version":1,"response":{"result":"operator_managed"}}"#
        );
        assert_eq!(
            serde_json::to_string(&ControlResponse::new(ControlResult::Status { status })).unwrap(),
            r#"{"protocol_version":1,"response":{"result":"status","status":{"controller_settled":false,"authentication":"missing","certificate":"missing","bound_core_node_name":null,"certificate_expiry_unix":null,"generation":null,"next_retry_after_secs":null,"router_apply_state":"standalone","operator_managed":false,"offline_recovery_required":false,"link":{"endpoint":"tls/hub.example:7447","link_state":"verified"},"pinned":false}}}"#
        );

        let logout = LogoutResult {
            certificate_revocation: CleanupState::Succeeded,
            oauth_revocation: CleanupState::Failed,
            router_apply: RouterApplyState::Standalone,
            local_cleanup: CleanupState::Succeeded,
            operator_action_required: false,
            target_namespace: Some("local".into()),
        };
        assert_eq!(
            serde_json::to_string(&ControlResponse::new(ControlResult::LoggedOut {
                outcome: logout
            }))
            .unwrap(),
            r#"{"protocol_version":1,"response":{"result":"logged_out","outcome":{"certificate_revocation":"succeeded","oauth_revocation":"failed","router_apply":"standalone","local_cleanup":"succeeded","operator_action_required":false,"target_namespace":"local"}}}"#
        );
    }

    #[test]
    fn unknown_fields_and_non_uuid_revisions_are_rejected() {
        for invalid in [
            r#"{"protocol_version":1,"surprise":true,"request":{"operation":"hello"}}"#,
            r#"{"protocol_version":1,"request":{"operation":"hello","surprise":true}}"#,
            r#"{"protocol_version":1,"request":{"operation":"logout","expected_session_revision":"not-a-uuid"}}"#,
            r#"{"protocol_version":1,"request":{"operation":"enroll_current_credential","expected_pat_subject":"subject-a"}}"#,
            r#"{"protocol_version":1,"request":{"operation":"enroll_current_credential","expected_api_origin":"https://api.peppy.bot"}}"#,
            r#"{"protocol_version":1,"request":{"operation":"enroll_current_credential","expected_session_revision":"550e8400-e29b-41d4-a716-446655440000","expected_pat_subject":"subject-a","expected_api_origin":"https://api.peppy.bot"}}"#,
            r#"{"protocol_version":1,"request":{"operation":"enroll_current_credential","expected_pat_subject":"","expected_api_origin":"https://api.peppy.bot"}}"#,
            r#"{"protocol_version":1,"request":{"operation":"enroll_current_credential","expected_pat_subject":"subject-a","expected_api_origin":""}}"#,
        ] {
            assert!(
                serde_json::from_str::<ControlRequest>(invalid).is_err(),
                "accepted {invalid}"
            );
        }

        for (field, value) in [
            ("expected_pat_subject", "s".repeat(1025)),
            ("expected_api_origin", "o".repeat(2049)),
        ] {
            let mut request = serde_json::json!({
                "protocol_version": 1,
                "request": {
                    "operation": "enroll_current_credential",
                    "expected_pat_subject": "subject-a",
                    "expected_api_origin": "https://api.peppy.bot",
                }
            });
            request["request"][field] = serde_json::Value::String(value);
            assert!(
                serde_json::from_value::<ControlRequest>(request).is_err(),
                "accepted over-limit {field}"
            );
        }

        for invalid in [
            r#"{"protocol_version":1,"response":{"result":"status","status":{"link":{"endpoint":null,"link_state":"not_configured"},"pinned":false}}}"#,
            r#"{"protocol_version":1,"response":{"result":"logged_out","outcome":{"certificate_revocation":"succeeded","oauth_revocation":"succeeded","router_apply":"standalone","local_cleanup":"succeeded","operator_action_required":false,"target_namespace":null,"surprise":true}}}"#,
            r#"{"protocol_version":1,"response":{"result":"logged_out","outcome":{"certificate_revocation":"maybe","oauth_revocation":"succeeded","router_apply":"standalone","local_cleanup":"succeeded","operator_action_required":false,"target_namespace":null}}}"#,
            r#"{"protocol_version":1,"response":{"result":"pinned"}}"#,
        ] {
            assert!(
                serde_json::from_str::<ControlResponse>(invalid).is_err(),
                "accepted {invalid}"
            );
        }
    }

    #[test]
    fn typed_client_parses_structured_logout_outcome() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FEDERATION_CONTROL_SOCK);
        let handle = stub_daemon(path.clone(), |_request, stream| {
            stream
                .write_all(
                    b"{\"protocol_version\":1,\"response\":{\"result\":\"logged_out\",\"outcome\":{\"certificate_revocation\":\"succeeded\",\"oauth_revocation\":\"not_needed\",\"router_apply\":\"operator_managed\",\"local_cleanup\":\"succeeded\",\"operator_action_required\":true,\"target_namespace\":\"local\"}}}\n",
                )
                .unwrap();
        });
        let revision = Uuid::parse_str("550e8400-e29b-41d4-a716-446655440000").unwrap();
        let outcome = logout(&path, Duration::from_secs(2), Some(revision)).unwrap();
        let request = handle.join().unwrap();
        assert_eq!(
            request,
            "{\"protocol_version\":1,\"request\":{\"operation\":\"logout\",\"expected_session_revision\":\"550e8400-e29b-41d4-a716-446655440000\"}}\n"
        );
        assert_eq!(
            outcome,
            LogoutResult {
                certificate_revocation: CleanupState::Succeeded,
                oauth_revocation: CleanupState::NotNeeded,
                router_apply: RouterApplyState::OperatorManaged,
                local_cleanup: CleanupState::Succeeded,
                operator_action_required: true,
                target_namespace: Some("local".into()),
            }
        );
    }

    #[test]
    fn typed_client_sends_enroll_and_parses_applied() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FEDERATION_CONTROL_SOCK);
        let handle = stub_daemon(path.clone(), |_request, stream| {
            stream
                .write_all(
                    b"{\"protocol_version\":1,\"response\":{\"result\":\"applied\",\"link\":{\"endpoint\":\"tls/hub.example:7447\",\"link_state\":\"verified\"}}}\n",
                )
                .unwrap();
        });
        let outcome = enroll_current_credential(
            &path,
            Duration::from_secs(2),
            None,
            Some("cli-subject".into()),
            Some("https://api.peppy.bot".into()),
        )
        .unwrap();
        let request = handle.join().unwrap();
        assert_eq!(
            request,
            "{\"protocol_version\":1,\"request\":{\"operation\":\"enroll_current_credential\",\"expected_pat_subject\":\"cli-subject\",\"expected_api_origin\":\"https://api.peppy.bot\"}}\n"
        );
        assert_eq!(
            outcome,
            ApplyResult::Applied(PlatformLink {
                endpoint: Some("tls/hub.example:7447".into()),
                link_state: LinkState::Verified,
            })
        );
    }

    #[test]
    fn typed_client_reports_operator_managed_apply() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FEDERATION_CONTROL_SOCK);
        let handle = stub_daemon(path.clone(), |_request, stream| {
            stream
                .write_all(
                    b"{\"protocol_version\":1,\"response\":{\"result\":\"operator_managed\"}}\n",
                )
                .unwrap();
        });
        assert_eq!(
            prepare_oauth_login(&path, Duration::from_secs(2), Uuid::new_v4()),
            Ok(ApplyResult::OperatorManaged)
        );
        handle.join().unwrap();
    }

    #[test]
    fn client_rejects_oversized_or_unterminated_responses() {
        for reply in [vec![b'x'; MAX_RESPONSE_LINE_BYTES + 1], b"{}".to_vec()] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join(FEDERATION_CONTROL_SOCK);
            let handle = stub_daemon(path.clone(), move |_request, stream| {
                stream.write_all(&reply).unwrap();
            });
            let result = hello(&path, Duration::from_secs(2));
            handle.join().unwrap();
            assert!(matches!(result, Err(ControlClientError::Transport(_))));
        }
    }

    #[test]
    fn client_rejects_unknown_response_fields() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FEDERATION_CONTROL_SOCK);
        let handle = stub_daemon(path.clone(), |_request, stream| {
            stream
                .write_all(
                    b"{\"protocol_version\":1,\"response\":{\"result\":\"hello\",\"surprise\":true}}\n",
                )
                .unwrap();
        });
        let result = hello(&path, Duration::from_secs(2));
        handle.join().unwrap();
        assert!(matches!(result, Err(ControlClientError::Transport(_))));
    }

    #[test]
    fn errors_are_control_free_utf8_bounded_and_typed() {
        let response = ControlResponse::error(
            ControlErrorCode::OperationFailed,
            format!("  bad\n{}  ", "é".repeat(MAX_ERROR_MESSAGE_BYTES)),
        );
        let ControlResult::Error { code, message } = response.response else {
            panic!("expected error")
        };
        assert_eq!(code, ControlErrorCode::OperationFailed);
        assert!(message.len() <= MAX_ERROR_MESSAGE_BYTES);
        assert!(!message.chars().any(char::is_control));
        assert!(std::str::from_utf8(message.as_bytes()).is_ok());
    }

    #[test]
    fn missing_socket_and_deadline_are_structured() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            hello(&dir.path().join("absent.sock"), Duration::from_millis(10)),
            Err(ControlClientError::DaemonNotRunning)
        );

        let path = dir.path().join(FEDERATION_CONTROL_SOCK);
        let handle = stub_daemon(path.clone(), |_request, _stream| {
            std::thread::sleep(Duration::from_millis(200));
        });
        assert_eq!(
            hello(&path, Duration::from_millis(30)),
            Err(ControlClientError::TimedOut)
        );
        handle.join().unwrap();
    }

    #[test]
    fn response_trickle_cannot_reset_the_absolute_client_deadline() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FEDERATION_CONTROL_SOCK);
        let handle = stub_daemon(path.clone(), |_request, stream| {
            let reply = b"{\"protocol_version\":1,\"response\":{\"result\":\"hello\"}}\n";
            for byte in reply {
                if stream.write_all(std::slice::from_ref(byte)).is_err() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        });

        assert_eq!(
            hello(&path, Duration::from_millis(35)),
            Err(ControlClientError::TimedOut)
        );
        handle.join().unwrap();
    }
}
