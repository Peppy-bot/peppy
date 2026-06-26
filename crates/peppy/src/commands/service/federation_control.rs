//! Daemon control socket that turns a `peppy auth login`/`logout` poke into an
//! *immediate* router (de)federation.
//!
//! A [`ServeAsyncCommand`] that binds the per-user Unix-domain socket
//! ([`crate::daemon_control::federation_control_socket_path`]) and, for each
//! connection, forwards a [`REFEDERATE_VERB`](crate::daemon_control::REFEDERATE_VERB)
//! request to the [`RouterFederation`](super::router_federation) loop as a
//! [`RefederateRequest`]. It waits for that poll to apply and writes the
//! resulting [`ControlResponse`] back, so the CLI learns federation is in place
//! *after* the local zenohd bounce — which is exactly why the channel is a UDS,
//! independent of the router being restarted.
//!
//! Binding is best-effort: a bind failure is logged and the task idles until
//! shutdown rather than taking the daemon down — the periodic federation poll
//! still (de)federates, only the *immediate* poke is unavailable.

use std::path::{Path, PathBuf};
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::oneshot;
use tracing::{info, warn};

use super::router_federation::{FederationOutcome, RefederateRequest, TriggerSender};
use super::serve::{ServeAsyncCommand, ServeAsyncHandle};
use crate::daemon_control::{ControlResponse, REFEDERATE_VERB};
use crate::error::Error;

/// Bound on reading a request line, so a client that connects but never writes
/// cannot hold a handler task open.
const REQUEST_READ_TIMEOUT: Duration = Duration::from_secs(5);

/// Extra time the daemon waits for the federation loop to apply, on top of the
/// configured connect timeout (which bounds the resolve). It must cover the
/// post-resolve work of a *verifying* login poke: the zenohd bounce plus the TLS
/// reachability probe ([`super::router_federation::PROBE_TIMEOUT`]). Kept smaller
/// than the client's read slack ([`crate::daemon_control::POKE_READ_SLACK`]) so
/// the daemon always replies a definite outcome before the client gives up (the
/// `ack_budget_*` test guards both relationships).
const APPLY_ACK_SLACK: Duration = Duration::from_secs(8);

impl From<FederationOutcome> for ControlResponse {
    fn from(outcome: FederationOutcome) -> Self {
        match outcome {
            FederationOutcome::Applied(applied) => ControlResponse::Ok { applied },
            FederationOutcome::Pinned => ControlResponse::Pinned,
            FederationOutcome::Failed(message) => ControlResponse::Error { message },
            FederationOutcome::Unreachable(message) => ControlResponse::Unreachable { message },
        }
    }
}

/// Background task owning the federation control socket. See the module docs.
pub(crate) struct FederationControl {
    socket_path: PathBuf,
    trigger_tx: TriggerSender,
    /// Bound on how long to wait for a poked poll to apply before replying with a
    /// timeout (so a wedged apply can't hold a connection open forever).
    connect_timeout: Duration,
}

impl FederationControl {
    pub(crate) fn new(
        socket_path: PathBuf,
        trigger_tx: TriggerSender,
        connect_timeout: Duration,
    ) -> Self {
        Self {
            socket_path,
            trigger_tx,
            connect_timeout,
        }
    }
}

impl ServeAsyncCommand for FederationControl {
    fn run(self: Box<Self>) -> ServeAsyncHandle {
        let FederationControl {
            socket_path,
            trigger_tx,
            connect_timeout,
        } = *self;
        let future = Box::pin(async move {
            // Race the accept loop against shutdown so the daemon can exit
            // promptly (the loop is otherwise infinite).
            tokio::select! {
                _ = serve_control(&socket_path, trigger_tx, connect_timeout) => {}
                res = super::shutdown_signal::shutdown_signal() => {
                    res.map_err(|e| Error::ExecutionFailed(
                        format!("federation control: failed to listen for shutdown: {e}")
                    ))?;
                }
            }
            // Best-effort cleanup so a stale socket does not linger (the next start
            // unlinks unconditionally anyway).
            let _ = std::fs::remove_file(&socket_path);
            Ok(())
        });
        // No readiness gate: binding the control socket is not a startup
        // dependency. The startup federation gate lives in `RouterFederation`.
        ServeAsyncHandle::new(future, None)
    }
}

/// Binds the socket and accepts poke connections until cancelled. A bind failure
/// is non-fatal: log it and idle until shutdown rather than aborting the daemon.
async fn serve_control(socket_path: &Path, trigger_tx: TriggerSender, connect_timeout: Duration) {
    let listener = match bind_listener(socket_path) {
        Ok(listener) => listener,
        Err(e) => {
            warn!(
                error = %e,
                path = %socket_path.display(),
                "federation control: could not bind control socket; login/logout pokes \
                 will not be applied immediately (federation still updates on its own poll)"
            );
            // Hold `trigger_tx` alive (so the federation loop's channel stays
            // open) and wait for the shutdown race above to cancel us.
            let _keep_sender_alive = trigger_tx;
            std::future::pending::<()>().await;
            return;
        }
    };
    info!(
        path = %socket_path.display(),
        "federation control: listening for login/logout federation pokes"
    );
    loop {
        match listener.accept().await {
            Ok((stream, _addr)) => {
                let trigger_tx = trigger_tx.clone();
                tokio::spawn(async move {
                    if let Err(e) = handle_conn(stream, trigger_tx, connect_timeout).await {
                        warn!(error = %e, "federation control: error handling a poke");
                    }
                });
            }
            Err(e) => warn!(error = %e, "federation control: accept failed; continuing"),
        }
    }
}

/// Creates the runtime dir, removes any stale socket from a prior (crashed)
/// daemon, binds, and restricts the socket to the owner. A leftover AF_UNIX path
/// is never connectable, so unconditional unlink-before-bind is safe.
fn bind_listener(socket_path: &Path) -> std::io::Result<UnixListener> {
    if let Some(parent) = socket_path.parent() {
        std::fs::create_dir_all(parent)?;
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

/// Services one poke connection: read the request, forward a [`RefederateRequest`]
/// to the federation loop, await the outcome (bounded), and reply.
async fn handle_conn(
    stream: UnixStream,
    trigger_tx: TriggerSender,
    connect_timeout: Duration,
) -> std::io::Result<()> {
    let (read_half, mut write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);

    let mut line = String::new();
    match tokio::time::timeout(REQUEST_READ_TIMEOUT, reader.read_line(&mut line)).await {
        Ok(Ok(0)) => return Ok(()), // client hung up before sending a request
        Ok(Ok(_)) => {}
        Ok(Err(e)) => return Err(e),
        Err(_elapsed) => return Ok(()), // client connected but never sent a line
    }

    if line.trim() != REFEDERATE_VERB {
        return write_response(&mut write_half, ControlResponse::error("unknown command")).await;
    }

    // Forward the poke and await the applied outcome. Bound the wait so a wedged
    // apply replies with a timeout rather than holding the connection open.
    let (ack_tx, ack_rx) = oneshot::channel();
    if trigger_tx
        .send(RefederateRequest { ack: ack_tx })
        .await
        .is_err()
    {
        return write_response(
            &mut write_half,
            ControlResponse::error("federation task not running"),
        )
        .await;
    }
    let response = match tokio::time::timeout(connect_timeout + APPLY_ACK_SLACK, ack_rx).await {
        Ok(Ok(outcome)) => ControlResponse::from(outcome),
        Ok(Err(_)) => ControlResponse::error("federation task dropped the request"),
        Err(_elapsed) => ControlResponse::error("timed out applying federation"),
    };
    write_response(&mut write_half, response).await
}

/// Writes one JSON response line and flushes.
async fn write_response(
    write_half: &mut (impl AsyncWriteExt + Unpin),
    response: ControlResponse,
) -> std::io::Result<()> {
    let mut line = serde_json::to_string(&response)
        .unwrap_or_else(|_| r#"{"status":"error","message":"serialize failed"}"#.to_string());
    line.push('\n');
    write_half.write_all(line.as_bytes()).await?;
    write_half.flush().await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon_control::{FEDERATION_CONTROL_SOCK, PokeOutcome, poke_refederate};
    use tokio::sync::mpsc;

    /// The daemon ack budget must cover the verifying poke's post-resolve work
    /// (the TLS probe + bounce), and the client must always outlast the daemon so
    /// it receives a definite reply rather than a client-side timeout. Guards the
    /// constants from drifting back into the pre-probe sizing.
    #[test]
    fn ack_budget_covers_the_verify_probe_and_client_outlasts_daemon() {
        use super::super::router_federation::PROBE_TIMEOUT;
        use crate::daemon_control::POKE_READ_SLACK;
        assert!(
            PROBE_TIMEOUT < APPLY_ACK_SLACK,
            "the daemon ack slack must cover the verify probe (plus the bounce)"
        );
        assert!(
            APPLY_ACK_SLACK < POKE_READ_SLACK,
            "the client must outlast the daemon so it gets a definite reply"
        );
    }

    /// A federation probe failure crosses the wire as the `unreachable` status,
    /// distinct from a plain `error`, so the CLI can word it specifically.
    #[test]
    fn unreachable_outcome_maps_to_the_unreachable_response() {
        let resp = ControlResponse::from(FederationOutcome::Unreachable(
            "received fatal alert: UnknownCA".to_string(),
        ));
        match resp {
            ControlResponse::Unreachable { message } => {
                assert_eq!(message, "received fatal alert: UnknownCA")
            }
            other => panic!("expected Unreachable, got {other:?}"),
        }
    }

    /// A poke from the (sync) CLI client crosses the real control socket, reaches
    /// the trigger channel, and the federation loop's ack comes back to the
    /// client — the end-to-end glue between [`crate::daemon_control`] and the
    /// federation loop.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn poke_crosses_the_socket_and_acks() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join(FEDERATION_CONTROL_SOCK);
        let (trigger_tx, mut trigger_rx) = mpsc::channel::<RefederateRequest>(8);

        // Stand in for the federation loop: ack a canned applied outcome.
        let consumer = tokio::spawn(async move {
            if let Some(req) = trigger_rx.recv().await {
                let _ = req
                    .ack
                    .send(FederationOutcome::Applied(Some("tls/cap:7443".to_string())));
            }
        });

        // The control listener, bound on the temp socket.
        let socket_for_listener = socket.clone();
        let control = tokio::spawn(async move {
            serve_control(&socket_for_listener, trigger_tx, Duration::from_secs(5)).await;
        });

        // Drive the blocking client off the async workers; it retries connect
        // briefly to let the listener bind.
        let socket_for_client = socket.clone();
        let outcome = tokio::task::spawn_blocking(move || {
            for _ in 0..100 {
                if socket_for_client.exists() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            poke_refederate(&socket_for_client, Duration::from_secs(5))
        })
        .await
        .unwrap();

        assert_eq!(
            outcome,
            PokeOutcome::Applied(Some("tls/cap:7443".to_string()))
        );

        control.abort();
        consumer.abort();
    }

    /// An unknown verb is rejected without ever poking the federation loop.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn unknown_verb_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join(FEDERATION_CONTROL_SOCK);
        let (trigger_tx, mut trigger_rx) = mpsc::channel::<RefederateRequest>(8);

        let socket_for_listener = socket.clone();
        let control = tokio::spawn(async move {
            serve_control(&socket_for_listener, trigger_tx, Duration::from_secs(5)).await;
        });

        let socket_for_client = socket.clone();
        let reply = tokio::task::spawn_blocking(move || {
            use std::io::{BufRead, BufReader, Write};
            use std::os::unix::net::UnixStream;
            for _ in 0..100 {
                if socket_for_client.exists() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            let mut stream = UnixStream::connect(&socket_for_client).unwrap();
            stream.write_all(b"bogus\n").unwrap();
            let mut reader = BufReader::new(stream);
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            line
        })
        .await
        .unwrap();

        assert!(
            reply.contains("error"),
            "unknown verb ⇒ error reply: {reply}"
        );
        // The federation loop was never poked.
        assert!(trigger_rx.try_recv().is_err());
        control.abort();
    }
}
