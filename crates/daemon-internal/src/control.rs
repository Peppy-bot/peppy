//! Client and wire protocol for the daemon's *federation control socket*.
//!
//! `peppy platform login`/`logout` run in a separate, short-lived process from
//! the `serve` daemon; their only shared state is the on-disk credentials file,
//! which the daemon would otherwise only re-read on its periodic poll (so
//! federation would lag a login by up to that interval). To apply it
//! immediately, the command **pokes** the running daemon over a per-user
//! Unix-domain socket: it sends [`REFEDERATE_VERB`] and waits for the daemon to
//! re-resolve and (de)federate, so federation is in place by the time the
//! command returns.
//!
//! The transport is a UDS rather than the daemon's Zenoh session on purpose: the
//! federation apply *bounces the local zenohd*, which would tear down a Zenoh-
//! carried ack mid-operation. A UDS is independent of zenohd, so the ack (sent
//! after the bounce) reliably reports the post-apply state.
//!
//! The socket path is *derived* from [`PeppyDirs`] (not stored anywhere): both
//! the daemon (the private `federation_control` module) and this client
//! resolve it the same way, so no discovery handshake is needed. A connect that
//! is refused or finds no socket simply means "no daemon running"; the command
//! succeeds and federation is applied the next time `serve` starts.

use std::io::{BufRead, BufReader, ErrorKind, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

use daemon_config::consts::PeppyDirs;
use serde::{Deserialize, Serialize};

/// File name of the daemon's federation control socket under the runtime dir.
pub const FEDERATION_CONTROL_SOCK: &str = "federation_control.sock";

/// The only request the control socket understands: re-resolve the platform
/// upstream and (de)federate the local router to match the current credentials.
/// One verb covers both login (resolves to an upstream) and logout (resolves to
/// none, de-federating).
pub const REFEDERATE_VERB: &str = "refederate";

/// Reads the daemon's cached federation state without resolving, rewriting the
/// router config, or restarting zenohd.
pub const STATUS_VERB: &str = "status";

/// Extra time the client waits for the daemon's ack on top of the configured
/// federation connect timeout. Kept strictly larger than the daemon-side ack
/// budget (`APPLY_ACK_SLACK`, which itself covers the verifying poke's TLS probe)
/// so the daemon always replies a definite status (even "timed out applying")
/// before the client gives up, turning a slow apply into a definite status rather
/// than a client-side timeout. (The `ack_budget_*` test guards this ordering.)
pub const POKE_READ_SLACK: Duration = Duration::from_secs(11);

/// Where the daemon binds (and the client connects to) the federation control
/// socket for a given [`PeppyDirs`]. Derived, never stored, so both sides agree.
pub fn federation_control_socket_path(peppy_dirs: &PeppyDirs) -> PathBuf {
    peppy_dirs
        .runtime_config_dir()
        .join(FEDERATION_CONTROL_SOCK)
}

/// Health of the daemon's single platform link as it last saw it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LinkState {
    /// No upstream resolved (logged out, or nothing pulled yet); the managed
    /// router is standalone.
    #[default]
    NotConfigured,
    /// Rendered into the router config but not yet checked by an explicit
    /// verifying federation request from this daemon generation.
    Unverified,
    /// A verifying poke confirmed the link's TLS handshake validates.
    Verified,
    /// The upstream was applied but the last verifying poke failed with this
    /// human-readable reason, so federation is not actually in effect.
    Error(String),
}

/// The platform link a refederation poke reports: the applied upstream
/// endpoint (when any) and its verification state.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlatformLink {
    pub endpoint: Option<String>,
    pub link_state: LinkState,
}

/// Cached federation state returned by [`query_status`]: the platform link
/// plus whether an operator-pinned `ZENOH_CONFIG` owns the router config.
/// The link is flattened on the wire, so the JSON shape is unchanged from
/// when the fields were spelled out here.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FederationStatus {
    #[serde(flatten)]
    pub link: PlatformLink,
    pub pinned: bool,
}

/// The daemon's one-line JSON reply to a control-socket request.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ControlResponse {
    /// The refederation ran: the platform link now applied (or cleared) and
    /// its verification state.
    Ok(PlatformLink),
    /// Cached state for a [`STATUS_VERB`] request.
    FederationStatus(FederationStatus),
    /// An operator-pinned `ZENOH_CONFIG` owns the router config; not auto-managed.
    Pinned,
    /// The daemon attempted the apply and it failed (e.g. backend unreachable
    /// within the federation timeout).
    Error { message: String },
    /// The credentials changed the daemon's *namespace*, which is immutable for
    /// a live session, so the daemon is restarting its whole generation to
    /// re-open every session under `target_namespace`. The daemon flushes this
    /// ack and only then tears down; the CLI polls the (path-stable) control
    /// socket until the daemon is back under exactly that namespace.
    Restarting { target_namespace: String },
}

impl ControlResponse {
    pub fn error(message: impl Into<String>) -> Self {
        Self::Error {
            message: message.into(),
        }
    }
}

/// What [`poke_refederate`] could determine about the daemon's federation state.
#[derive(Debug, PartialEq, Eq)]
pub enum PokeOutcome {
    /// The daemon acked with the applied platform link and its state.
    Applied(PlatformLink),
    /// Operator-pinned `ZENOH_CONFIG` owns the router config (not auto-managed).
    Pinned,
    /// The daemon acked an error (e.g. the backend was unreachable in time), or
    /// replied with malformed data.
    DaemonError(String),
    /// No running daemon to poke (no socket, or the connection was refused).
    /// Federation will be applied the next time `serve` starts.
    DaemonNotRunning,
    /// Connected, but the daemon did not ack within the read deadline.
    TimedOut,
    /// The credentials changed the daemon's namespace, so the daemon acked and
    /// is restarting its whole generation. The caller then polls until the
    /// daemon is back under `target_namespace`.
    Restarting { target_namespace: String },
}

/// What [`query_status`] could determine about the daemon's cached state.
#[derive(Debug, PartialEq, Eq)]
pub enum QueryStatusOutcome {
    Status(FederationStatus),
    DaemonError(String),
    DaemonNotRunning,
    TimedOut,
    /// The daemon acked that it is mid-restart into `target_namespace` (a
    /// status query racing a namespace change).
    Restarting {
        target_namespace: String,
    },
}

/// The transport-level failures every control-socket verb classifies the same
/// way, mapped into each verb's outcome enum via `From`.
enum TransportFailure {
    /// A read/write timeout: surfaces as WouldBlock/TimedOut on a socket with
    /// a deadline set.
    TimedOut,
    /// No socket file, or nothing listening: no daemon to reach.
    DaemonNotRunning,
    /// Any other I/O failure, carried as a message.
    Error(String),
}

impl TransportFailure {
    fn classify(error: &std::io::Error) -> Self {
        match error.kind() {
            ErrorKind::WouldBlock | ErrorKind::TimedOut => Self::TimedOut,
            ErrorKind::NotFound | ErrorKind::ConnectionRefused => Self::DaemonNotRunning,
            _ => Self::Error(error.to_string()),
        }
    }
}

impl From<TransportFailure> for PokeOutcome {
    fn from(failure: TransportFailure) -> Self {
        match failure {
            TransportFailure::TimedOut => Self::TimedOut,
            TransportFailure::DaemonNotRunning => Self::DaemonNotRunning,
            TransportFailure::Error(message) => Self::DaemonError(message),
        }
    }
}

impl From<TransportFailure> for QueryStatusOutcome {
    fn from(failure: TransportFailure) -> Self {
        match failure {
            TransportFailure::TimedOut => Self::TimedOut,
            TransportFailure::DaemonNotRunning => Self::DaemonNotRunning,
            TransportFailure::Error(message) => Self::DaemonError(message),
        }
    }
}

/// Pokes the running daemon over `socket_path` to re-resolve and (re)apply
/// federation, blocking until it acks or `read_timeout` elapses.
///
/// Best effort by design: a poke failure must never fail the calling command, so
/// a missing/refused socket maps to [`PokeOutcome::DaemonNotRunning`] and any
/// other I/O error to a definite outcome rather than an `Err`.
pub fn poke_refederate(socket_path: &Path, read_timeout: Duration) -> PokeOutcome {
    match request(socket_path, read_timeout, REFEDERATE_VERB) {
        Ok(ControlResponse::Ok(link)) => PokeOutcome::Applied(link),
        Ok(ControlResponse::Pinned) => PokeOutcome::Pinned,
        Ok(ControlResponse::Error { message }) => PokeOutcome::DaemonError(message),
        Ok(ControlResponse::Restarting { target_namespace }) => {
            PokeOutcome::Restarting { target_namespace }
        }
        Ok(ControlResponse::FederationStatus(_)) => {
            PokeOutcome::DaemonError("daemon returned status state to a refederate request".into())
        }
        Err(e) => TransportFailure::classify(&e).into(),
    }
}

/// Queries cached federation state without triggering a router rewrite.
pub fn query_status(socket_path: &Path, read_timeout: Duration) -> QueryStatusOutcome {
    match request(socket_path, read_timeout, STATUS_VERB) {
        Ok(ControlResponse::FederationStatus(status)) => QueryStatusOutcome::Status(status),
        Ok(ControlResponse::Error { message }) => QueryStatusOutcome::DaemonError(message),
        Ok(ControlResponse::Restarting { target_namespace }) => {
            QueryStatusOutcome::Restarting { target_namespace }
        }
        Ok(_) => QueryStatusOutcome::DaemonError(
            "daemon returned a refederation reply to a status request".into(),
        ),
        Err(e) => TransportFailure::classify(&e).into(),
    }
}

fn request(
    socket_path: &Path,
    read_timeout: Duration,
    verb: &str,
) -> std::io::Result<ControlResponse> {
    let mut stream = UnixStream::connect(socket_path)?;
    stream.set_read_timeout(Some(read_timeout))?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;

    stream.write_all(format!("{verb}\n").as_bytes())?;
    stream.flush()?;

    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    let read = reader.read_line(&mut line)?;
    if read == 0 {
        // The daemon hung up before replying (e.g. it was shutting down).
        return Err(std::io::Error::new(
            ErrorKind::ConnectionAborted,
            "daemon closed the control connection before replying",
        ));
    }
    serde_json::from_str(line.trim()).map_err(|e| std::io::Error::new(ErrorKind::InvalidData, e))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;

    /// Spawns a one-shot stub daemon on `path` that reads the request line and
    /// runs `reply` with it, returning the request the stub observed.
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
            reply(line.trim(), &mut stream);
            line.trim().to_string()
        })
    }

    #[test]
    fn poke_sends_refederate_and_parses_the_ack() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FEDERATION_CONTROL_SOCK);
        let handle = stub_daemon(path.clone(), |_req, stream| {
            stream
                .write_all(
                    b"{\"status\":\"ok\",\"endpoint\":\"tls/hub.example:7447\",\"link_state\":\"verified\"}\n",
                )
                .unwrap();
        });

        let outcome = poke_refederate(&path, Duration::from_secs(5));
        let request = handle.join().unwrap();

        assert_eq!(request, REFEDERATE_VERB);
        assert_eq!(
            outcome,
            PokeOutcome::Applied(PlatformLink {
                endpoint: Some("tls/hub.example:7447".to_string()),
                link_state: LinkState::Verified,
            })
        );
    }

    #[test]
    fn poke_parses_defederated_pinned_error_and_link_error() {
        for (reply, expected) in [
            (
                "{\"status\":\"ok\",\"endpoint\":null,\"link_state\":\"not_configured\"}\n",
                PokeOutcome::Applied(PlatformLink {
                    endpoint: None,
                    link_state: LinkState::NotConfigured,
                }),
            ),
            ("{\"status\":\"pinned\"}\n", PokeOutcome::Pinned),
            (
                "{\"status\":\"error\",\"message\":\"boom\"}\n",
                PokeOutcome::DaemonError("boom".to_string()),
            ),
            (
                "{\"status\":\"ok\",\"endpoint\":\"tls/hub.example:7447\",\"link_state\":{\"error\":\"received fatal alert: UnknownCA\"}}\n",
                PokeOutcome::Applied(PlatformLink {
                    endpoint: Some("tls/hub.example:7447".to_string()),
                    link_state: LinkState::Error("received fatal alert: UnknownCA".to_string()),
                }),
            ),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join(FEDERATION_CONTROL_SOCK);
            let handle = stub_daemon(path.clone(), move |_req, stream| {
                stream.write_all(reply.as_bytes()).unwrap();
            });
            let outcome = poke_refederate(&path, Duration::from_secs(5));
            handle.join().unwrap();
            assert_eq!(outcome, expected, "reply {reply:?}");
        }
    }

    #[test]
    fn poke_without_a_socket_reports_not_running() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FEDERATION_CONTROL_SOCK);
        // No listener bound: connect is refused / the path does not exist.
        assert_eq!(
            poke_refederate(&path, Duration::from_secs(1)),
            PokeOutcome::DaemonNotRunning
        );
    }

    #[test]
    fn poke_times_out_when_daemon_never_replies() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FEDERATION_CONTROL_SOCK);
        // Stub accepts and reads the request but never writes a reply, then
        // sleeps past the client's deadline before dropping the connection.
        let handle = stub_daemon(path.clone(), |_req, _stream| {
            std::thread::sleep(Duration::from_millis(400));
        });
        let outcome = poke_refederate(&path, Duration::from_millis(150));
        handle.join().unwrap();
        assert_eq!(outcome, PokeOutcome::TimedOut);
    }

    /// The ack and status wire shapes, pinned exactly: the platform link is
    /// `endpoint` + typed `link_state`, and the restarting ack carries the
    /// target namespace.
    #[test]
    fn platform_status_wire_shape_is_stable() {
        let ack = ControlResponse::Ok(PlatformLink {
            endpoint: Some("tls/hub.example:7447".to_string()),
            link_state: LinkState::Error("UnknownIssuer".to_string()),
        });
        assert_eq!(
            serde_json::to_string(&ack).unwrap(),
            r#"{"status":"ok","endpoint":"tls/hub.example:7447","link_state":{"error":"UnknownIssuer"}}"#
        );

        let status = ControlResponse::FederationStatus(FederationStatus {
            link: PlatformLink {
                endpoint: Some("tls/hub.example:7447".to_string()),
                link_state: LinkState::Verified,
            },
            pinned: false,
        });
        assert_eq!(
            serde_json::to_string(&status).unwrap(),
            r#"{"status":"federation_status","endpoint":"tls/hub.example:7447","link_state":"verified","pinned":false}"#
        );

        let restarting = ControlResponse::Restarting {
            target_namespace: "550e8400-e29b-41d4-a716-446655440000".to_string(),
        };
        assert_eq!(
            serde_json::to_string(&restarting).unwrap(),
            r#"{"status":"restarting","target_namespace":"550e8400-e29b-41d4-a716-446655440000"}"#
        );
    }

    #[test]
    fn restarting_ack_carries_the_target_namespace() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FEDERATION_CONTROL_SOCK);
        let handle = stub_daemon(path.clone(), |_req, stream| {
            stream
                .write_all(
                    b"{\"status\":\"restarting\",\"target_namespace\":\"550e8400-e29b-41d4-a716-446655440000\"}\n",
                )
                .unwrap();
        });

        let outcome = poke_refederate(&path, Duration::from_secs(5));
        handle.join().unwrap();

        assert_eq!(
            outcome,
            PokeOutcome::Restarting {
                target_namespace: "550e8400-e29b-41d4-a716-446655440000".to_string()
            }
        );
    }

    #[test]
    fn query_status_sends_status_and_parses_the_platform_state() {
        fn status(endpoint: Option<&str>, link_state: LinkState, pinned: bool) -> FederationStatus {
            FederationStatus {
                link: PlatformLink {
                    endpoint: endpoint.map(str::to_string),
                    link_state,
                },
                pinned,
            }
        }
        for (reply, expected) in [
            (
                "{\"status\":\"federation_status\",\"endpoint\":null,\"link_state\":\"not_configured\",\"pinned\":false}\n",
                status(None, LinkState::NotConfigured, false),
            ),
            (
                "{\"status\":\"federation_status\",\"endpoint\":\"tls/hub.example:7447\",\"link_state\":\"unverified\",\"pinned\":false}\n",
                status(Some("tls/hub.example:7447"), LinkState::Unverified, false),
            ),
            (
                "{\"status\":\"federation_status\",\"endpoint\":\"tls/hub.example:7447\",\"link_state\":\"verified\",\"pinned\":true}\n",
                status(Some("tls/hub.example:7447"), LinkState::Verified, true),
            ),
            (
                "{\"status\":\"federation_status\",\"endpoint\":\"tls/hub.example:7447\",\"link_state\":{\"error\":\"UnknownCA\"},\"pinned\":false}\n",
                status(
                    Some("tls/hub.example:7447"),
                    LinkState::Error("UnknownCA".to_string()),
                    false,
                ),
            ),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join(FEDERATION_CONTROL_SOCK);
            let handle = stub_daemon(path.clone(), move |req, stream| {
                assert_eq!(req, STATUS_VERB);
                stream.write_all(reply.as_bytes()).unwrap();
            });

            let outcome = query_status(&path, Duration::from_secs(5));
            handle.join().unwrap();
            assert_eq!(
                outcome,
                QueryStatusOutcome::Status(expected),
                "reply {reply:?}"
            );
        }
    }

    #[test]
    fn query_status_reports_an_invalid_reply_as_a_daemon_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FEDERATION_CONTROL_SOCK);
        let handle = stub_daemon(path.clone(), |_req, stream| {
            stream.write_all(b"not-json\n").unwrap();
        });

        let outcome = query_status(&path, Duration::from_secs(5));
        handle.join().unwrap();

        assert!(matches!(outcome, QueryStatusOutcome::DaemonError(message) if !message.is_empty()));
    }

    #[test]
    fn query_status_reports_an_aborted_reply_as_a_daemon_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FEDERATION_CONTROL_SOCK);
        let handle = stub_daemon(path.clone(), |_req, _stream| {});

        let outcome = query_status(&path, Duration::from_secs(5));
        handle.join().unwrap();

        assert!(matches!(outcome, QueryStatusOutcome::DaemonError(message)
            if message.contains("closed the control connection")));
    }

    #[test]
    fn query_status_without_a_socket_reports_not_running() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FEDERATION_CONTROL_SOCK);

        assert_eq!(
            query_status(&path, Duration::from_secs(1)),
            QueryStatusOutcome::DaemonNotRunning
        );
    }

    #[test]
    fn query_status_with_a_refused_socket_reports_not_running() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FEDERATION_CONTROL_SOCK);
        drop(UnixListener::bind(&path).expect("bind stale socket"));

        assert_eq!(
            query_status(&path, Duration::from_secs(1)),
            QueryStatusOutcome::DaemonNotRunning
        );
    }
}
