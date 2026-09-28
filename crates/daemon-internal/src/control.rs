//! Client and wire protocol for the daemon's *federation control socket*.
//!
//! `peppy platform enroll`/`unenroll` run in a separate, short-lived process
//! from the `serve` daemon; their only shared state is the enrollment directory
//! on disk, which the daemon reads when it builds a generation. To react
//! immediately, the command **pokes** the running daemon over a per-user
//! Unix-domain socket: it sends [`REFEDERATE_VERB`] and waits for the daemon to
//! re-read the enrollment, restart under the new identity when it changed, or
//! verify the link to the cloud router when it did not.
//!
//! The transport is a UDS rather than the daemon's Zenoh session on purpose: an
//! identity change restarts the whole generation, which would tear down a
//! Zenoh-carried ack. A UDS is independent of the router, so the `Restarting`
//! ack is flushed before the teardown begins.
//!
//! The socket path is *derived* from [`PeppyDirs`] (not stored anywhere): both
//! the daemon (the private `federation_control` module) and this client
//! resolve it the same way, so no discovery handshake is needed. A connect that
//! is refused or finds no socket simply means "no daemon running"; the
//! enrollment then takes effect the next time `serve` starts.

use std::io::{BufRead, BufReader, ErrorKind, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

use daemon_config::consts::PeppyDirs;
use serde::{Deserialize, Serialize};

/// File name of the daemon's federation control socket under the runtime dir.
pub const FEDERATION_CONTROL_SOCK: &str = "federation_control.sock";

/// The only request the control socket understands: re-read the enrollment
/// and reconcile the local router with it. One verb covers both enrolling
/// (the daemon restarts under the project) and unenrolling (it restarts under
/// `local`), and a repeat poke verifies the link.
pub const REFEDERATE_VERB: &str = "refederate";

/// How long the client waits for the daemon's ack. Kept strictly larger than
/// the daemon-side ack budget (`ACK_BUDGET`, which itself covers the poke's
/// TLS probe) so the daemon always replies a definite status (even "timed out
/// verifying") before the client gives up. (The `ack_budget_*` test guards this
/// ordering.)
pub const POKE_READ_TIMEOUT: Duration = Duration::from_secs(12);

/// Where the daemon binds (and the client connects to) the federation control
/// socket for a given [`PeppyDirs`]. Derived, never stored, so both sides agree.
pub fn federation_control_socket_path(peppy_dirs: &PeppyDirs) -> PathBuf {
    peppy_dirs
        .runtime_config_dir()
        .join(FEDERATION_CONTROL_SOCK)
}

/// The daemon's one-line JSON reply to a [`REFEDERATE_VERB`] request. Shared by
/// the daemon (which writes it) and this client (which parses it).
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ControlResponse {
    /// The router runs what the enrollment prescribes: `Some(locator)` dialing
    /// the project's cloud router with the link verified, `None` not enrolled.
    Ok { applied: Option<String> },
    /// An operator-pinned `ZENOH_CONFIG` owns the router config; not auto-managed.
    Pinned,
    /// The router is enrolled, but the mutual-TLS link to the project's cloud
    /// router could not be established or validated.
    Unreachable { message: String },
    /// The daemon could not answer (the enrollment on disk is unreadable, or
    /// the reconcile timed out).
    Error { message: String },
    /// The enrollment changed the daemon's identity (its router id or session
    /// namespace), neither of which can change while live, so the daemon is
    /// restarting its whole generation. It flushes this ack and only then tears
    /// down; the CLI polls the (path-stable) control socket until the daemon
    /// is back under the expected identity.
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
    /// The daemon acked: `Some(locator)` enrolled with the link verified,
    /// `None` not enrolled.
    Applied(Option<String>),
    /// Operator-pinned `ZENOH_CONFIG` owns the router config (not auto-managed).
    Pinned,
    /// The daemon acked an error (the enrollment is unreadable, or the
    /// reconcile timed out).
    DaemonError(String),
    /// The router is enrolled but the mutual-TLS link to the project's cloud
    /// router does not validate (an expired certificate, an untrusted issuer,
    /// an unreachable router).
    Unreachable(String),
    /// No running daemon to poke (no socket, or the connection was refused).
    /// The enrollment takes effect the next time `serve` starts.
    DaemonNotRunning,
    /// Connected, but the daemon did not ack within the read deadline.
    TimedOut,
    /// The enrollment changed the daemon's identity, so the daemon acked and
    /// is restarting its whole generation. The caller then polls until the
    /// daemon is back under the expected identity.
    Restarting,
}

/// Pokes the running daemon over `socket_path` to re-read its enrollment and
/// reconcile, blocking until it acks or `read_timeout` elapses.
///
/// Best effort by design: a poke failure must never fail the calling command, so
/// a missing/refused socket maps to [`PokeOutcome::DaemonNotRunning`] and any
/// other I/O error to a benign outcome rather than an `Err`.
pub fn poke_refederate(socket_path: &Path, read_timeout: Duration) -> PokeOutcome {
    match poke_inner(socket_path, read_timeout) {
        Ok(outcome) => outcome,
        Err(e) => match e.kind() {
            // A read/write timeout surfaces as WouldBlock/TimedOut on a socket
            // with a deadline set.
            ErrorKind::WouldBlock | ErrorKind::TimedOut => PokeOutcome::TimedOut,
            // No socket file, or nothing listening: no daemon to poke.
            _ => PokeOutcome::DaemonNotRunning,
        },
    }
}

fn poke_inner(socket_path: &Path, read_timeout: Duration) -> std::io::Result<PokeOutcome> {
    let mut stream = UnixStream::connect(socket_path)?;
    stream.set_read_timeout(Some(read_timeout))?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;

    stream.write_all(format!("{REFEDERATE_VERB}\n").as_bytes())?;
    stream.flush()?;

    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    let read = reader.read_line(&mut line)?;
    if read == 0 {
        // The daemon hung up before replying (e.g. it was shutting down).
        return Ok(PokeOutcome::DaemonNotRunning);
    }
    let response: ControlResponse = serde_json::from_str(line.trim())
        .map_err(|e| std::io::Error::new(ErrorKind::InvalidData, e))?;
    Ok(match response {
        ControlResponse::Ok { applied } => PokeOutcome::Applied(applied),
        ControlResponse::Pinned => PokeOutcome::Pinned,
        ControlResponse::Unreachable { message } => PokeOutcome::Unreachable(message),
        ControlResponse::Error { message } => PokeOutcome::DaemonError(message),
        ControlResponse::Restarting => PokeOutcome::Restarting,
    })
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
            PokeOutcome::Applied(Some("tls/cap.example:7443".to_string()))
        );
    }

    #[test]
    fn poke_parses_defederated_and_pinned_and_error() {
        for (reply, expected) in [
            (
                "{\"status\":\"ok\",\"applied\":null}\n",
                PokeOutcome::Applied(None),
            ),
            ("{\"status\":\"pinned\"}\n", PokeOutcome::Pinned),
            (
                "{\"status\":\"error\",\"message\":\"boom\"}\n",
                PokeOutcome::DaemonError("boom".to_string()),
            ),
            (
                "{\"status\":\"unreachable\",\"message\":\"received fatal alert: UnknownCA\"}\n",
                PokeOutcome::Unreachable("received fatal alert: UnknownCA".to_string()),
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
}
