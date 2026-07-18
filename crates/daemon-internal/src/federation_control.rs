//! Daemon control socket that turns a `peppy auth login`/`logout` poke into an
//! *immediate* router (de)federation.
//!
//! A [`ServeAsyncCommand`] that binds the per-user Unix-domain socket
//! ([`crate::control::federation_control_socket_path`]) and, for each
//! connection, forwards a [`REFEDERATE_VERB`](crate::control::REFEDERATE_VERB)
//! request to the [`RouterFederation`](super::router_federation) loop as a
//! [`RefederateRequest`]. It waits for that poll to apply and writes the
//! resulting [`ControlResponse`] back, so the CLI learns federation is in place
//! *after* the local zenohd bounce, which is exactly why the channel is a UDS,
//! independent of the router being restarted.
//!
//! Binding is best-effort: a bind failure is logged and the task idles until
//! shutdown rather than taking the daemon down; the periodic federation poll
//! still (de)federates, only the *immediate* poke is unavailable.

use std::path::{Path, PathBuf};
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{oneshot, watch};
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::control::{ControlResponse, REFEDERATE_VERB, STATUS_VERB};
use crate::router_federation::{FederationOutcome, FederationRequest, TriggerSender};
use crate::serve::{ServeAsyncCommand, ServeAsyncHandle};

/// Bound on reading a request line, so a client that connects but never writes
/// cannot hold a handler task open.
const REQUEST_READ_TIMEOUT: Duration = Duration::from_secs(5);

/// Extra time the daemon waits for the federation loop to apply, on top of the
/// configured connect timeout (which bounds the resolve). It must cover the
/// post-resolve work of a *verifying* login poke: the zenohd bounce plus the TLS
/// reachability probe ([`super::router_federation::PROBE_TIMEOUT`]). Kept smaller
/// than the client's read slack ([`crate::control::POKE_READ_SLACK`]) so
/// the daemon always replies a definite outcome before the client gives up (the
/// `ack_budget_*` test guards both relationships).
const APPLY_ACK_SLACK: Duration = Duration::from_secs(10);

/// Cached status requests never resolve or bounce the router, so a short bound
/// is enough even if a refederation request is immediately ahead in the queue.
const STATUS_ACK_TIMEOUT: Duration = Duration::from_secs(2);

impl From<FederationOutcome> for ControlResponse {
    fn from(outcome: FederationOutcome) -> Self {
        match outcome {
            FederationOutcome::Applied(applied) => ControlResponse::Ok(applied),
            FederationOutcome::Pinned => ControlResponse::Pinned,
            FederationOutcome::Failed(message) => ControlResponse::Error { message },
            FederationOutcome::Unreachable { reason, applied } => ControlResponse::Unreachable {
                message: reason,
                applied,
            },
            FederationOutcome::Restart => ControlResponse::Restarting,
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
        connect_timeout: Duration,
        restart_tx: watch::Sender<bool>,
        teardown_token: CancellationToken,
    ) -> Self {
        Self {
            socket_path,
            trigger_tx,
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
            connect_timeout,
            restart_tx,
            teardown_token,
        } = *self;
        let future = Box::pin(async move {
            // Race the accept loop against shutdown (a real signal or an in-process
            // restart via the shared token) so the daemon can exit promptly (the
            // loop is otherwise infinite).
            tokio::select! {
                _ = serve_control(&socket_path, trigger_tx, connect_timeout, restart_tx) => {}
                _ = crate::shutdown_signal::shutdown_or_token(&teardown_token) => {}
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
async fn serve_control(
    socket_path: &Path,
    trigger_tx: TriggerSender,
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
    accept_loop(listener, trigger_tx, connect_timeout, restart_tx).await;
}

/// Accepts poke connections on an already-bound listener until cancelled.
/// Split from [`serve_control`] so callers that need a ready-to-connect socket
/// (the tests) can bind first and only then run the loop: once
/// [`bind_listener`] returns, connects succeed and queue in the backlog even
/// before this loop is scheduled.
async fn accept_loop(
    listener: UnixListener,
    trigger_tx: TriggerSender,
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
                let trigger_tx = trigger_tx.clone();
                let restart_tx = restart_tx.clone();
                tokio::spawn(async move {
                    if let Err(e) =
                        handle_conn(stream, trigger_tx, connect_timeout, restart_tx).await
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
    restart_tx: watch::Sender<bool>,
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

    if line.trim() == STATUS_VERB {
        let deadline = tokio::time::Instant::now() + STATUS_ACK_TIMEOUT;
        let (ack_tx, ack_rx) = oneshot::channel();
        match tokio::time::timeout_at(
            deadline,
            trigger_tx.send(FederationRequest::Status { ack: ack_tx }),
        )
        .await
        {
            Ok(Ok(())) => {}
            Ok(Err(_)) => {
                return write_response(
                    &mut write_half,
                    ControlResponse::error("federation task not running"),
                )
                .await;
            }
            Err(_) => {
                return write_response(
                    &mut write_half,
                    ControlResponse::error("timed out reading federation status"),
                )
                .await;
            }
        }
        let response = match tokio::time::timeout_at(deadline, ack_rx).await {
            Ok(Ok(status)) => ControlResponse::FederationStatus(status),
            Ok(Err(_)) => ControlResponse::error("federation task dropped the status request"),
            Err(_) => ControlResponse::error("timed out reading federation status"),
        };
        return write_response(&mut write_half, response).await;
    }

    if line.trim() != REFEDERATE_VERB {
        return write_response(&mut write_half, ControlResponse::error("unknown command")).await;
    }

    // Forward the poke and await the applied outcome. Bound the wait so a wedged
    // apply replies with a timeout rather than holding the connection open.
    let deadline = tokio::time::Instant::now() + connect_timeout + APPLY_ACK_SLACK;
    let (ack_tx, ack_rx) = oneshot::channel();
    match tokio::time::timeout_at(
        deadline,
        trigger_tx.send(FederationRequest::Refederate { ack: ack_tx }),
    )
    .await
    {
        Ok(Ok(())) => {}
        Ok(Err(_)) => {
            return write_response(
                &mut write_half,
                ControlResponse::error("federation task not running"),
            )
            .await;
        }
        Err(_) => {
            return write_response(
                &mut write_half,
                ControlResponse::error("timed out applying federation"),
            )
            .await;
        }
    }
    let response = match tokio::time::timeout_at(deadline, ack_rx).await {
        Ok(Ok(outcome)) => ControlResponse::from(outcome),
        Ok(Err(_)) => ControlResponse::error("federation task dropped the request"),
        Err(_elapsed) => ControlResponse::error("timed out applying federation"),
    };

    // The namespace changed: attempt to write+flush the `Restarting` ack FIRST,
    // then raise the in-process restart signal. A successful flush is a real
    // happens-before edge, so a connected CLI reads the ack before teardown can
    // affect the connection. If the client disconnected before the reply, the
    // write error must not suppress the restart required for namespace safety.
    let trigger_restart = matches!(response, ControlResponse::Restarting);
    let write_result = write_response(&mut write_half, response).await;
    if trigger_restart {
        let _ = restart_tx.send(true);
    }
    write_result
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
    use crate::control::{
        AppliedFederation, FEDERATION_CONTROL_SOCK, FederationStatus, PeerLinkState, PeerReport,
        PokeOutcome, QueryStatusOutcome, poke_refederate, query_status,
    };
    use tokio::sync::mpsc;

    /// The daemon ack budget must cover the verifying poke's post-resolve work
    /// (the TLS probe + bounce), and the client must always outlast the daemon so
    /// it receives a definite reply rather than a client-side timeout. Guards the
    /// constants from drifting back into the pre-probe sizing.
    #[test]
    fn ack_budget_covers_the_verify_probe_and_client_outlasts_daemon() {
        use crate::control::POKE_READ_SLACK;
        use crate::router_federation::{APPLY_TIMEOUT, PROBE_TIMEOUT};
        assert!(
            APPLY_TIMEOUT + PROBE_TIMEOUT < APPLY_ACK_SLACK,
            "the daemon ack slack must cover the router apply and verify probe"
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
        let resp = ControlResponse::from(FederationOutcome::Unreachable {
            reason: "received fatal alert: UnknownCA".to_string(),
            applied: AppliedFederation {
                backend: Some("tls/cap:7443".to_string()),
                peers: Vec::new(),
            },
        });
        match resp {
            ControlResponse::Unreachable { message, applied } => {
                assert_eq!(message, "received fatal alert: UnknownCA");
                assert_eq!(applied.backend.as_deref(), Some("tls/cap:7443"));
                assert!(applied.peers.is_empty());
            }
            other => panic!("expected Unreachable, got {other:?}"),
        }
    }

    /// A poke from the (sync) CLI client crosses the real control socket, reaches
    /// the trigger channel, and the federation loop's ack comes back to the
    /// client: the end-to-end glue between [`crate::control`] and the
    /// federation loop.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn poke_crosses_the_socket_and_acks() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join(FEDERATION_CONTROL_SOCK);
        let (trigger_tx, mut trigger_rx) = mpsc::channel::<FederationRequest>(8);

        // Stand in for the federation loop: ack a canned applied outcome.
        let consumer = tokio::spawn(async move {
            if let Some(FederationRequest::Refederate { ack }) = trigger_rx.recv().await {
                let _ = ack.send(FederationOutcome::Applied(AppliedFederation {
                    backend: Some("tls/cap:7443".to_string()),
                    peers: Vec::new(),
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
        let control = tokio::spawn(accept_loop(
            listener,
            trigger_tx,
            Duration::from_secs(5),
            restart_tx,
        ));

        // Drive the blocking client off the async workers.
        let socket_for_client = socket.clone();
        let outcome = tokio::task::spawn_blocking(move || {
            poke_refederate(&socket_for_client, Duration::from_secs(5))
        })
        .await
        .unwrap();

        assert_eq!(
            outcome,
            PokeOutcome::Applied(AppliedFederation {
                backend: Some("tls/cap:7443".to_string()),
                peers: Vec::new(),
            })
        );

        control.abort();
        consumer.abort();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn status_crosses_the_socket_without_refederating() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join(FEDERATION_CONTROL_SOCK);
        let (trigger_tx, mut trigger_rx) = mpsc::channel::<FederationRequest>(8);
        let expected = FederationStatus {
            backend: None,
            peers: vec![PeerReport {
                endpoint: "tls/peer:7449".to_string(),
                state: PeerLinkState::Error("UnknownIssuer".to_string()),
            }],
            listen_endpoint: Some("tls/0.0.0.0:7449".to_string()),
            pinned: false,
        };
        let response = expected.clone();
        let consumer = tokio::spawn(async move {
            match trigger_rx.recv().await {
                Some(FederationRequest::Status { ack }) => {
                    let _ = ack.send(response);
                }
                Some(FederationRequest::Refederate { .. }) => {
                    panic!("status request must not refederate")
                }
                None => panic!("control channel closed"),
            }
        });
        let listener = bind_listener(&socket).expect("bind control socket");
        let (restart_tx, _restart_rx) = watch::channel(false);
        let control = tokio::spawn(accept_loop(
            listener,
            trigger_tx,
            Duration::from_secs(5),
            restart_tx,
        ));

        let socket_for_client = socket.clone();
        let outcome = tokio::task::spawn_blocking(move || {
            query_status(&socket_for_client, Duration::from_secs(5))
        })
        .await
        .unwrap();

        assert_eq!(outcome, QueryStatusOutcome::Status(expected));
        control.abort();
        consumer.abort();
    }

    #[tokio::test]
    async fn status_deadline_covers_enqueue_and_ack_waits() {
        let (server, mut client) = UnixStream::pair().expect("create control socket pair");
        let (trigger_tx, mut trigger_rx) = mpsc::channel::<FederationRequest>(1);
        let (queued_ack, _queued_rx) = oneshot::channel();
        trigger_tx
            .send(FederationRequest::Status { ack: queued_ack })
            .await
            .expect("prefill trigger channel");
        let (restart_tx, _restart_rx) = watch::channel(false);
        let handler = tokio::spawn(handle_conn(
            server,
            trigger_tx,
            Duration::from_secs(1),
            restart_tx,
        ));

        let test_deadline =
            tokio::time::Instant::now() + STATUS_ACK_TIMEOUT + Duration::from_millis(500);
        client
            .write_all(format!("{STATUS_VERB}\n").as_bytes())
            .await
            .expect("send status request");

        // Keep the queue full for half the budget. The remaining acknowledgement
        // wait must use the other half, not start a fresh STATUS_ACK_TIMEOUT.
        tokio::time::sleep(STATUS_ACK_TIMEOUT / 2).await;
        drop(
            trigger_rx
                .recv()
                .await
                .expect("remove the prefilled request"),
        );

        let mut reader = BufReader::new(client);
        let mut line = String::new();
        tokio::time::timeout_at(test_deadline, reader.read_line(&mut line))
            .await
            .expect("enqueue and ack waits share one status deadline")
            .expect("read timeout response");
        assert!(line.contains("timed out reading federation status"));
        handler.await.expect("handler does not panic").unwrap();
    }

    /// An unknown verb is rejected without ever poking the federation loop.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn unknown_verb_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join(FEDERATION_CONTROL_SOCK);
        let (trigger_tx, mut trigger_rx) = mpsc::channel::<FederationRequest>(8);

        // Bind-before-client, as in `poke_crosses_the_socket_and_acks`.
        let listener = bind_listener(&socket).expect("bind control socket");
        let (restart_tx, _restart_rx) = watch::channel(false);
        let control = tokio::spawn(accept_loop(
            listener,
            trigger_tx,
            Duration::from_secs(5),
            restart_tx,
        ));

        let socket_for_client = socket.clone();
        let reply = tokio::task::spawn_blocking(move || {
            use std::io::{BufRead, BufReader, Write};
            use std::os::unix::net::UnixStream;
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

    /// Once the federation loop detects a namespace change, a client that exits
    /// before reading the ack must not strand the daemon in its old namespace.
    #[tokio::test]
    async fn restart_is_signaled_when_the_client_disconnects_before_the_ack() {
        let (server, mut client) = UnixStream::pair().expect("create control socket pair");
        let (trigger_tx, mut trigger_rx) = mpsc::channel::<FederationRequest>(1);
        let (restart_tx, mut restart_rx) = watch::channel(false);

        let handler = tokio::spawn(handle_conn(
            server,
            trigger_tx,
            Duration::from_secs(1),
            restart_tx,
        ));
        client
            .write_all(format!("{REFEDERATE_VERB}\n").as_bytes())
            .await
            .expect("send refederate request");
        drop(client);

        let request = tokio::time::timeout(Duration::from_secs(1), trigger_rx.recv())
            .await
            .expect("handler forwards request promptly")
            .expect("request channel remains open");
        let FederationRequest::Refederate { ack } = request else {
            panic!("expected a refederate request");
        };
        ack.send(FederationOutcome::Restart)
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
}
