//! Client and wire protocol for the daemon's *federation control socket*.
//!
//! `peppy auth login`/`logout` run in a separate, short-lived process from the
//! `serve` daemon; their only shared state is the on-disk credentials file, which
//! the daemon would otherwise only re-read on its periodic poll (so federation
//! would lag a login by up to that interval). To apply it immediately, the
//! command **pokes** the running daemon over a per-user Unix-domain socket: it
//! sends [`REFEDERATE_VERB`] and waits for the daemon to re-resolve and
//! (de)federate, so federation is in place by the time the command returns.
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

/// The only request the control socket understands: re-resolve the caller's
/// upstream and (de)federate the local router to match the current credentials.
/// One verb covers both login (resolves to an upstream) and logout (resolves to
/// none ⇒ de-federate).
pub const REFEDERATE_VERB: &str = "refederate";

/// Reads the daemon's cached federation state without resolving, rewriting the
/// router config, or restarting zenohd.
pub const STATUS_VERB: &str = "status";

/// Cached status marker for a peer rendered at daemon startup but not yet
/// checked by an explicit verifying federation request.
pub const UNVERIFIED_PEER_REASON: &str = "not yet verified by this daemon generation";

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

/// The daemon's one-line JSON reply to a [`REFEDERATE_VERB`] request. Shared by
/// the daemon (which writes it) and this client (which parses it).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeerReportWire {
    pub endpoint: String,
    #[serde(default)]
    pub error: Option<String>,
}

/// Applied state returned by a refederation poke.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppliedFederation {
    pub backend: Option<String>,
    pub peers: Vec<PeerReportWire>,
}

/// Cached federation state returned by [`query_status`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FederationStatus {
    pub backend: Option<String>,
    #[serde(default)]
    pub peers: Vec<PeerReportWire>,
    pub listen_endpoint: Option<String>,
    pub pinned: bool,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ControlResponse {
    /// Federation is in effect: `Some(ep)` federated to `ep`, `None`
    /// de-federated.
    Ok {
        applied: Option<String>,
        #[serde(default)]
        peers: Vec<PeerReportWire>,
    },
    /// Cached state for a [`STATUS_VERB`] request.
    FederationStatus {
        backend: Option<String>,
        #[serde(default)]
        peers: Vec<PeerReportWire>,
        listen_endpoint: Option<String>,
        pinned: bool,
    },
    /// An operator-pinned `ZENOH_CONFIG` owns the router config; not auto-managed.
    Pinned,
    /// The config was applied (the local router was federated), but the TLS link
    /// to the per-user cloud router could not be established/validated, so
    /// federation with platform-backend is not actually in effect.
    Unreachable {
        message: String,
        applied: Option<String>,
        peers: Vec<PeerReportWire>,
    },
    /// The daemon attempted the apply and it failed (e.g. backend unreachable
    /// within the federation timeout).
    Error { message: String },
    /// The credentials changed the daemon's *organization namespace*, which is
    /// immutable for a live session, so the daemon is restarting its whole
    /// generation to re-open every session under the new namespace. The daemon
    /// flushes this ack and only then tears down; the CLI polls the (path-stable)
    /// control socket until the daemon is back under the expected namespace.
    Restarting,
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
    /// The daemon acked with the applied backend and user-peer state.
    Applied(AppliedFederation),
    /// Operator-pinned `ZENOH_CONFIG` owns the router config (not auto-managed).
    Pinned,
    /// The daemon acked an error (e.g. the backend was unreachable in time).
    DaemonError(String),
    /// The daemon federated the local router but the TLS link to the per-user
    /// cloud router does not validate (e.g. UnknownCA); federation with
    /// platform-backend is not in effect.
    Unreachable {
        reason: String,
        applied: AppliedFederation,
    },
    /// No running daemon to poke (no socket, or the connection was refused).
    /// Federation will be applied the next time `serve` starts.
    DaemonNotRunning,
    /// Connected, but the daemon did not ack within the read deadline.
    TimedOut,
    /// The credentials changed the daemon's organization namespace, so the daemon
    /// acked and is restarting its whole generation. The caller then polls until
    /// the daemon is back under the expected namespace.
    Restarting,
}

/// What [`query_status`] could determine about the daemon's cached state.
#[derive(Debug, PartialEq, Eq)]
pub enum QueryStatusOutcome {
    Status(FederationStatus),
    DaemonError(String),
    DaemonNotRunning,
    TimedOut,
}

/// Pokes the running daemon over `socket_path` to re-resolve and (re)apply
/// federation, blocking until it acks or `read_timeout` elapses.
///
/// Best effort by design: a poke failure must never fail the calling command, so
/// a missing/refused socket maps to [`PokeOutcome::DaemonNotRunning`] and any
/// other I/O error to a benign outcome rather than an `Err`.
pub fn poke_refederate(socket_path: &Path, read_timeout: Duration) -> PokeOutcome {
    match request(socket_path, read_timeout, REFEDERATE_VERB) {
        Ok(ControlResponse::Ok { applied, peers }) => PokeOutcome::Applied(AppliedFederation {
            backend: applied,
            peers,
        }),
        Ok(ControlResponse::Pinned) => PokeOutcome::Pinned,
        Ok(ControlResponse::Unreachable {
            message,
            applied,
            peers,
        }) => PokeOutcome::Unreachable {
            reason: message,
            applied: AppliedFederation {
                backend: applied,
                peers,
            },
        },
        Ok(ControlResponse::Error { message }) => PokeOutcome::DaemonError(message),
        Ok(ControlResponse::Restarting) => PokeOutcome::Restarting,
        Ok(ControlResponse::FederationStatus { .. }) => {
            PokeOutcome::DaemonError("daemon returned status state to a refederate request".into())
        }
        Err(e) => match e.kind() {
            // A read/write timeout surfaces as WouldBlock/TimedOut on a socket
            // with a deadline set.
            ErrorKind::WouldBlock | ErrorKind::TimedOut => PokeOutcome::TimedOut,
            // No socket file, or nothing listening: no daemon to poke.
            _ => PokeOutcome::DaemonNotRunning,
        },
    }
}

/// Queries cached federation state without triggering a router rewrite.
pub fn query_status(socket_path: &Path, read_timeout: Duration) -> QueryStatusOutcome {
    match request(socket_path, read_timeout, STATUS_VERB) {
        Ok(ControlResponse::FederationStatus {
            backend,
            peers,
            listen_endpoint,
            pinned,
        }) => QueryStatusOutcome::Status(FederationStatus {
            backend,
            peers,
            listen_endpoint,
            pinned,
        }),
        Ok(ControlResponse::Error { message }) => QueryStatusOutcome::DaemonError(message),
        Ok(_) => QueryStatusOutcome::DaemonError(
            "daemon returned a refederation reply to a status request".into(),
        ),
        Err(e) => match e.kind() {
            ErrorKind::WouldBlock | ErrorKind::TimedOut => QueryStatusOutcome::TimedOut,
            _ => QueryStatusOutcome::DaemonNotRunning,
        },
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
    fn poke_sends_refederate_and_parses_ok() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FEDERATION_CONTROL_SOCK);
        let handle = stub_daemon(path.clone(), |_req, stream| {
            stream
                .write_all(b"{\"status\":\"ok\",\"applied\":\"tls/cap.example:7443\"}\n")
                .unwrap();
        });

        let outcome = poke_refederate(&path, Duration::from_secs(5));
        let request = handle.join().unwrap();

        assert_eq!(request, REFEDERATE_VERB);
        assert_eq!(
            outcome,
            PokeOutcome::Applied(AppliedFederation {
                backend: Some("tls/cap.example:7443".to_string()),
                peers: Vec::new(),
            })
        );
    }

    #[test]
    fn poke_parses_defederated_and_pinned_and_error() {
        for (reply, expected) in [
            (
                "{\"status\":\"ok\",\"applied\":null}\n",
                PokeOutcome::Applied(AppliedFederation {
                    backend: None,
                    peers: Vec::new(),
                }),
            ),
            ("{\"status\":\"pinned\"}\n", PokeOutcome::Pinned),
            (
                "{\"status\":\"error\",\"message\":\"boom\"}\n",
                PokeOutcome::DaemonError("boom".to_string()),
            ),
            (
                "{\"status\":\"unreachable\",\"message\":\"received fatal alert: UnknownCA\",\"applied\":\"tls/cap.example:7443\",\"peers\":[]}\n",
                PokeOutcome::Unreachable {
                    reason: "received fatal alert: UnknownCA".to_string(),
                    applied: AppliedFederation {
                        backend: Some("tls/cap.example:7443".to_string()),
                        peers: Vec::new(),
                    },
                },
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

    #[test]
    fn ok_without_peers_uses_the_empty_default() {
        let response: ControlResponse =
            serde_json::from_str(r#"{"status":"ok","applied":"tls/cap.example:7443"}"#)
                .expect("ok response parses");
        match response {
            ControlResponse::Ok { applied, peers } => {
                assert_eq!(applied.as_deref(), Some("tls/cap.example:7443"));
                assert!(peers.is_empty());
            }
            other => panic!("expected ok response, got {other:?}"),
        }
    }

    #[test]
    fn query_status_sends_status_and_parses_cached_state() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FEDERATION_CONTROL_SOCK);
        let handle = stub_daemon(path.clone(), |_req, stream| {
            stream
                .write_all(
                    b"{\"status\":\"federation_status\",\"backend\":null,\"peers\":[{\"endpoint\":\"tls/peer:7449\",\"error\":null}],\"listen_endpoint\":\"tls/0.0.0.0:7449\",\"pinned\":false}\n",
                )
                .unwrap();
        });

        let outcome = query_status(&path, Duration::from_secs(5));
        let request = handle.join().unwrap();

        assert_eq!(request, STATUS_VERB);
        assert_eq!(
            outcome,
            QueryStatusOutcome::Status(FederationStatus {
                backend: None,
                peers: vec![PeerReportWire {
                    endpoint: "tls/peer:7449".to_string(),
                    error: None,
                }],
                listen_endpoint: Some("tls/0.0.0.0:7449".to_string()),
                pinned: false,
            })
        );
    }
}
