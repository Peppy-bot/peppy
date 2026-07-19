//! Daemon control socket that turns a `peppy platform login`/`logout` poke into
//! an *immediate* router (de)federation.
//!
//! A [`ServeAsyncCommand`] that binds the per-user Unix-domain socket
//! ([`crate::control::federation_control_socket_path`]) and, for each
//! connection, forwards a [`REFEDERATE_VERB`](crate::control::REFEDERATE_VERB)
//! request to the [`RouterFederation`](super::router_federation) loop. It waits
//! for that poll to apply and writes the resulting [`ControlResponse`] back, so
//! the CLI learns federation is in place *after* the local zenohd bounce, which
//! is exactly why the channel is a UDS, independent of the router being
//! restarted. [`STATUS_VERB`](crate::control::STATUS_VERB) requests are
//! answered inline from the federation loop's status watch, so they can never
//! queue behind an in-flight apply.
//!
//! Binding is best-effort: a bind failure is logged and the task idles until
//! shutdown rather than taking the daemon down; the periodic federation poll
//! still (de)federates, only the *immediate* poke is unavailable.

use std::path::{Path, PathBuf};
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::mpsc::error::TrySendError;
use tokio::sync::{oneshot, watch};
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::control::{
    ControlResponse, DEFEDERATE_VERB, FederationStatus, REFEDERATE_VERB, STATUS_VERB,
};
use crate::router_federation::{FederationOutcome, FederationTrigger, TriggerSender};
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

impl From<FederationOutcome> for ControlResponse {
    fn from(outcome: FederationOutcome) -> Self {
        match outcome {
            FederationOutcome::Applied(link) => ControlResponse::Ok(link),
            FederationOutcome::Pinned => ControlResponse::Pinned,
            FederationOutcome::Failed(message) => ControlResponse::Error { message },
            FederationOutcome::Restart { target_namespace } => ControlResponse::Restarting {
                target_namespace: target_namespace.as_str().to_string(),
            },
        }
    }
}

/// Background task owning the federation control socket. See the module docs.
pub(crate) struct FederationControl {
    socket_path: PathBuf,
    trigger_tx: TriggerSender,
    /// The federation loop's published status, answered inline to
    /// [`STATUS_VERB`] requests.
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
        // dependency. The startup federation gate lives in `RouterFederation`.
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

/// Services one poke connection: read the request, forward a refederation
/// request to the federation loop (or answer status from the watch), await the
/// outcome (bounded), and reply.
async fn handle_conn(
    stream: UnixStream,
    trigger_tx: TriggerSender,
    status_rx: watch::Receiver<FederationStatus>,
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
        // Answered straight from the federation loop's published cache: no
        // resolve, no router bounce, and no queueing behind an in-flight apply.
        let status = status_rx.borrow().clone();
        return write_response(&mut write_half, ControlResponse::FederationStatus(status)).await;
    }

    let force_standalone = match line.trim() {
        REFEDERATE_VERB => false,
        DEFEDERATE_VERB => true,
        _ => {
            return write_response(&mut write_half, ControlResponse::error("unknown command"))
                .await;
        }
    };

    // Forward the poke and await the applied outcome. The queue holds at most
    // one waiting poke (see `router_federation::trigger_channel`), so a second
    // concurrent poke is rejected as busy instead of piling up. Bound the ack
    // wait so a wedged apply replies with a timeout rather than holding the
    // connection open.
    let deadline = tokio::time::Instant::now() + connect_timeout + APPLY_ACK_SLACK;
    let (ack_tx, ack_rx) = oneshot::channel();
    let trigger = if force_standalone {
        FederationTrigger::Defederate(ack_tx)
    } else {
        FederationTrigger::Refederate(ack_tx)
    };
    match trigger_tx.try_send(trigger) {
        Ok(()) => {}
        Err(TrySendError::Full(_)) => {
            return write_response(
                &mut write_half,
                ControlResponse::error("federation task is busy"),
            )
            .await;
        }
        Err(TrySendError::Closed(_)) => {
            return write_response(
                &mut write_half,
                ControlResponse::error("federation task not running"),
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
    let trigger_restart = matches!(response, ControlResponse::Restarting { .. });
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
        FEDERATION_CONTROL_SOCK, FederationStatus, LinkState, PlatformLink, PokeOutcome,
        QueryStatusOutcome, poke_refederate, query_status,
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

    /// A federation probe failure crosses the wire as an errored link inside
    /// the `ok` ack, distinct from a plain `error`, so the CLI can word it
    /// specifically.
    #[test]
    fn a_link_error_outcome_maps_into_the_ok_ack() {
        let resp = ControlResponse::from(FederationOutcome::Applied(PlatformLink {
            endpoint: Some("tls/hub:7447".to_string()),
            link_state: LinkState::Error("received fatal alert: UnknownCA".to_string()),
        }));
        match resp {
            ControlResponse::Ok(link) => {
                assert_eq!(link.endpoint.as_deref(), Some("tls/hub:7447"));
                assert_eq!(
                    link.link_state,
                    LinkState::Error("received fatal alert: UnknownCA".to_string())
                );
            }
            other => panic!("expected Ok, got {other:?}"),
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
        let (trigger_tx, mut trigger_rx) = mpsc::channel::<FederationTrigger>(8);

        // Stand in for the federation loop: ack a canned applied outcome.
        let consumer = tokio::spawn(async move {
            if let Some(FederationTrigger::Refederate(ack)) = trigger_rx.recv().await {
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
            poke_refederate(&socket_for_client, Duration::from_secs(5))
        })
        .await
        .unwrap();

        assert_eq!(
            outcome,
            PokeOutcome::Applied(PlatformLink {
                endpoint: Some("tls/hub:7447".to_string()),
                link_state: LinkState::Verified,
            })
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
        let outcome = tokio::task::spawn_blocking(move || {
            query_status(&socket_for_client, Duration::from_secs(5))
        })
        .await
        .unwrap();

        assert_eq!(outcome, QueryStatusOutcome::Status(expected));
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
            .try_send(FederationTrigger::Refederate(queued_ack))
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
        };
        let (_status_tx, status_rx) = watch::channel(expected.clone());
        let handler = tokio::spawn(handle_conn(
            server,
            trigger_tx,
            status_rx,
            Duration::from_secs(1),
            restart_tx,
        ));

        client
            .write_all(format!("{STATUS_VERB}\n").as_bytes())
            .await
            .expect("send status request");

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
            .try_send(FederationTrigger::Refederate(queued_ack))
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

        client
            .write_all(format!("{REFEDERATE_VERB}\n").as_bytes())
            .await
            .expect("send refederate request");

        let mut reader = BufReader::new(client);
        let mut line = String::new();
        tokio::time::timeout(Duration::from_secs(1), reader.read_line(&mut line))
            .await
            .expect("a full queue must be rejected promptly")
            .expect("read busy response");
        assert!(line.contains("federation task is busy"), "reply: {line}");
        handler.await.expect("handler does not panic").unwrap();
    }

    /// An unknown verb is rejected without ever poking the federation loop.
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
        client
            .write_all(format!("{REFEDERATE_VERB}\n").as_bytes())
            .await
            .expect("send refederate request");
        drop(client);

        let trigger = tokio::time::timeout(Duration::from_secs(1), trigger_rx.recv())
            .await
            .expect("handler forwards request promptly")
            .expect("request channel remains open");
        let FederationTrigger::Refederate(ack) = trigger else {
            panic!("restart test sent an unexpected forced-standalone request")
        };
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
}
