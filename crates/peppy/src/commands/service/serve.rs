use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::{oneshot, watch};
use tokio::task::{JoinError, JoinSet};
use tracing::{error, info, warn};

use super::Command;
use crate::context::AppContext;
use crate::error::{Error, Result};

use super::builder::ServeCommandBuilder;
pub use tokio_util::sync::CancellationToken;

/// Why a serve generation stopped running, threaded up to the in-process
/// supervisor loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ServeOutcome {
    /// A real stop (SIGINT/SIGTERM, `peppy service stop`, or all tasks done):
    /// the daemon exits 0 and the supervisor (systemd/launchd) leaves it stopped.
    Stop,
    /// A namespace change requested an in-process restart: tear the generation
    /// down and rebuild a fresh one under the same PID.
    Restart,
}

/// Non-zero exit code used ONLY for the port-stuck / flap-cap fallback, so a
/// crash-only supervisor (systemd `Restart=on-failure`, launchd `KeepAlive`
/// scoped to `SuccessfulExit=false`) recovers a daemon the in-process loop could
/// not. Distinct from a clean stop's `0` and the generic `exit(1)`.
pub(crate) const RESTART_EXIT_CODE: i32 = 75;

/// Wall-clock window and cap for the in-process flap backstop: more than
/// [`FLAP_CAP`] restarts within [`FLAP_WINDOW`] converts a same-PID busy loop
/// (otherwise invisible to systemd) into a visible `exit(RESTART_EXIT_CODE)`.
const FLAP_WINDOW: Duration = Duration::from_secs(60);
const FLAP_CAP: usize = 5;

pub(crate) type ServeFuture = Pin<Box<dyn Future<Output = Result<()>> + Send + 'static>>;

pub(crate) struct ServeAsyncHandle {
    future: ServeFuture,
    ready: Option<oneshot::Receiver<()>>,
    /// Whether a readiness gate that drops without firing aborts startup. `true`
    /// for load-bearing gates (messaging router, core node) whose failure means
    /// the daemon is broken; `false` for a best-effort gate (federation) that may
    /// *delay* startup but must never crash it.
    ready_required: bool,
}

impl ServeAsyncHandle {
    /// A handle whose readiness gate (if any) is *required*: if its sender drops
    /// without firing, startup aborts.
    pub(crate) fn new(future: ServeFuture, ready: Option<oneshot::Receiver<()>>) -> Self {
        Self {
            future,
            ready,
            ready_required: true,
        }
    }

    /// A handle whose readiness gate is *optional*: startup waits for it (so the
    /// work is in place before reporting ready) but a drop is logged and
    /// tolerated. Used for the best-effort federation gate, so a slow or failed
    /// federation degrades to "proceed standalone" rather than taking the daemon
    /// down.
    pub(crate) fn new_optional_ready(future: ServeFuture, ready: oneshot::Receiver<()>) -> Self {
        Self {
            future,
            ready: Some(ready),
            ready_required: false,
        }
    }

    fn into_parts(self) -> (ServeFuture, Option<oneshot::Receiver<()>>, bool) {
        (self.future, self.ready, self.ready_required)
    }
}

pub(crate) trait ServeAsyncCommand: Send + Sync {
    fn run(self: Box<Self>) -> ServeAsyncHandle;
}

#[derive(Default)]
pub(crate) struct CompositeCommand {
    async_commands: Vec<Box<dyn ServeAsyncCommand>>,
}

impl CompositeCommand {
    pub(crate) fn add_async_command(mut self, command: Box<dyn ServeAsyncCommand>) -> Self {
        self.async_commands.push(command);
        self
    }

    pub(crate) fn execute(self) -> Result<Vec<ServeAsyncHandle>> {
        let mut futures: Vec<ServeAsyncHandle> = Vec::new();
        for async_command in self.async_commands {
            futures.push(async_command.run());
        }

        Ok(futures)
    }
}

pub(crate) struct Serve {
    composite_command: CompositeCommand,
    /// External shutdown injection (tests / embedders): when `Some` and
    /// cancelled, the coordinator records [`ServeOutcome::Stop`]. `None` in
    /// production (the CLI path).
    shutdown_token: Option<CancellationToken>,
    /// The shared token every serve task observes for teardown. The coordinator
    /// cancels it on its way out so each task runs its real graceful teardown
    /// (close session, stop_router, SIGKILL nodes, unlink the socket) rather than
    /// being aborted. For a generation built by [`ServeCommandBuilder`] this is
    /// the token cloned into the tasks; a bare [`Serve::new`] (tests) creates a
    /// fresh one its tasks do not observe.
    teardown_token: CancellationToken,
    /// In-process restart channel: a `true` from [`super::federation_control`]'s
    /// `handle_conn` (after it flushed the `Restarting` ack) makes the
    /// coordinator record [`ServeOutcome::Restart`]. `None` when no federation
    /// control task exists (mock engine).
    restart_rx: Option<watch::Receiver<bool>>,
}

/// The serve command is the command that runs as a daemon in systemd and maintains a "node stack" (a graph representation of nodes)
/// It's installed using the `install` command.
/// It operates as follow:
/// 1. Starts a zenohd separate process
/// 2. Creates an internal "node stack" (a graph of nodes that depends on each other)
/// 3. Starts a "core node" that listen for incoming commands
impl Serve {
    fn log_task_result(result: std::result::Result<Result<()>, JoinError>) {
        match result {
            Err(e) => error!("Task panicked: {:?}", e),
            Ok(Err(e)) => error!("Command error: {}", e),
            Ok(Ok(())) => {}
        }
    }

    pub(crate) fn new(composite_command: CompositeCommand) -> Self {
        Self {
            composite_command,
            shutdown_token: None,
            teardown_token: CancellationToken::new(),
            restart_rx: None,
        }
    }

    pub(crate) fn with_shutdown_token(mut self, token: CancellationToken) -> Self {
        self.shutdown_token = Some(token);
        self
    }

    /// Sets the shared token the generation's tasks observe (so the coordinator
    /// can cancel it to unpark them for graceful teardown).
    pub(crate) fn with_teardown_token(mut self, token: CancellationToken) -> Self {
        self.teardown_token = token;
        self
    }

    /// Arms the in-process restart channel the federation control handler signals.
    pub(crate) fn with_restart_rx(mut self, rx: watch::Receiver<bool>) -> Self {
        self.restart_rx = Some(rx);
        self
    }

    pub(crate) fn execute(self) -> Result<ServeOutcome> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()?;

        let handles = self.composite_command.execute()?;
        let external_shutdown = self.shutdown_token;
        let teardown_token = self.teardown_token;
        let mut restart_rx = self.restart_rx;

        info!("Running serve command...");
        let outcome = runtime.block_on(async move {
            let mut join_set = JoinSet::new();
            let mut readiness = Vec::new();
            for handle in handles {
                let (future, ready, ready_required) = handle.into_parts();
                if let Some(rx) = ready {
                    readiness.push((rx, ready_required));
                }
                join_set.spawn(future);
            }

            for (ready, required) in readiness {
                if ready.await.is_err() {
                    if !required {
                        // A best-effort gate (federation) dropped without firing,
                        // e.g. its task exited early or shutdown raced startup.
                        // Proceed (standalone) rather than failing the daemon.
                        warn!(
                            "A serve handler dropped its optional readiness gate; \
                             continuing without it"
                        );
                        continue;
                    }
                    let join_result = join_set.join_next().await;
                    let err = match join_result {
                        Some(Ok(Ok(()))) => Error::ExecutionFailed(
                            "Serve handler exited before signaling readiness".into(),
                        ),
                        Some(Ok(Err(e))) => e,
                        Some(Err(join_err)) => Error::ExecutionFailed(format!(
                            "Serve handler panicked before signaling readiness: {}",
                            join_err
                        )),
                        None => Error::ExecutionFailed(
                            "Serve handler dropped before signaling readiness".into(),
                        ),
                    };
                    return Err(err);
                }
            }

            info!("Serve command initialized!");
            // The coordinator: the authoritative observer of the OS shutdown
            // signal, the external injection, and the in-process restart channel.
            // On any of them it records the reason and breaks; tasks observe only
            // the shared `teardown_token`, which the coordinator cancels below so
            // each runs its real graceful teardown (no force-abort).
            //
            // The signal future is created ONCE, outside the loop: signal streams
            // are edge-triggered, so a listener recreated per iteration loses a
            // SIGINT that raced a task completion. The other branches are
            // state-based (JoinSet, cancellation token, watch channel) and safe to
            // recreate. Never re-polled after completion: its branch always breaks.
            let shutdown = super::shutdown_signal::shutdown_signal();
            tokio::pin!(shutdown);
            let reason = loop {
                tokio::select! {
                    result = join_set.join_next() => {
                        match result {
                            Some(result) => Self::log_task_result(result),
                            None => {
                                info!("All serve handlers completed. Exiting...");
                                break ServeOutcome::Stop;
                            }
                        }
                    }
                    _ = async {
                        match &external_shutdown {
                            Some(token) => token.cancelled().await,
                            None => std::future::pending::<()>().await,
                        }
                    } => {
                        info!("External shutdown requested");
                        break ServeOutcome::Stop;
                    }
                    signal = &mut shutdown => {
                        match signal {
                            Ok(_) => {
                                info!("Shutdown signal received");
                                break ServeOutcome::Stop;
                            }
                            Err(e) => {
                                return Err(Error::ExecutionFailed(format!(
                                    "Failed to listen for shutdown signal: {}", e
                                )));
                            }
                        }
                    }
                    _ = async {
                        match &mut restart_rx {
                            Some(rx) => {
                                // Only an explicit `true` is a restart request. A
                                // closed channel (the federation control task drops
                                // its sender during a signal-driven teardown) must
                                // NOT be read as one, or ctrl+C turns into a restart.
                                if rx.wait_for(|restart| *restart).await.is_err() {
                                    std::future::pending::<()>().await;
                                }
                            }
                            None => std::future::pending::<()>().await,
                        }
                    } => {
                        info!("In-process restart signal received (namespace change)");
                        break ServeOutcome::Restart;
                    }
                }
            };

            info!("Tearing down serve handlers (reason: {reason:?})...");
            // Unpark every task that observes the shared token so they run their
            // real graceful teardown rather than waiting on a signal that (for a
            // restart) will never arrive. Idempotent if already cancelled.
            teardown_token.cancel();
            while let Some(result) = join_set.join_next().await {
                Self::log_task_result(result);
            }

            Ok::<ServeOutcome, Error>(reason)
        })?;
        Ok(outcome)
    }
}

pub struct ServeCommand {
    pub messaging_engine: String,
    pub core_node_name: Option<String>,
    pub clock_source: super::ClockSource,
    pub shutdown_token: Option<CancellationToken>,
}

impl Command for ServeCommand {
    fn execute(self, ctx: &Arc<AppContext>) -> Result<()> {
        // In-process supervised restart loop: a namespace change (login/logout)
        // tears down the current generation and rebuilds a fresh one under the
        // SAME PID, with no execv and no external supervisor, so the switch is
        // uniform across a systemd install, a launchd install, and a bare
        // `peppy service serve` terminal. A real stop (SIGTERM / `service stop`)
        // returns `Stop` and this loop exits 0; the crash-only supervisor never
        // restarts a clean exit.
        let mut flap = FlapWindow::new();
        loop {
            match self.run_one_generation(ctx)? {
                ServeOutcome::Stop => return Ok(()),
                ServeOutcome::Restart => {
                    finalize_before_restart();
                    if flap.record_and_is_flapping() {
                        error!(
                            "daemon restarted more than {FLAP_CAP} times within {:?}; \
                             exiting ({RESTART_EXIT_CODE}) for the supervisor to recover",
                            FLAP_WINDOW
                        );
                        std::process::exit(RESTART_EXIT_CODE);
                    }
                    info!("Rebuilding the daemon generation under the new namespace...");
                }
            }
        }
    }
}

impl ServeCommand {
    /// Builds and runs one daemon generation, returning why it stopped. Each call
    /// is a clean generation: fresh sessions, a fresh `CoreNode` (so its
    /// declaration guard re-runs), and the namespace + federation gate re-resolved
    /// from the credentials at the top of the build.
    fn run_one_generation(&self, ctx: &Arc<AppContext>) -> Result<ServeOutcome> {
        // Read the daemon-global config, creating it with defaults if missing.
        // Resolved from `PeppyDirs::default()` (the same ~/.peppy the core node
        // uses), applied to the daemon's own session and every spawned node.
        let peppy_dirs = daemon_config::consts::PeppyDirs::default();
        let peppy_config =
            daemon_config::peppy_config::load_or_create(&peppy_dirs).map_err(|e| {
                Error::ExecutionFailed(format!("Failed to load peppy_config.json5: {e}"))
            })?;

        let mut builder = ServeCommandBuilder::new(&ctx.root_dir)?
            .with_peppy_config(peppy_config)
            .with_messaging_router(self.messaging_engine.clone())?
            .with_core_node(self.core_node_name.clone(), self.clock_source)?;

        if let Some(token) = &self.shutdown_token {
            builder = builder.with_shutdown_token(token.clone());
        }

        let executor = builder.build()?;
        executor
            .execute()
            .inspect_err(|e| error!("Serve command failed: {}", e))
    }
}

/// In-process flap backstop: more than [`FLAP_CAP`] restarts within
/// [`FLAP_WINDOW`] is a flap, converted into a visible `exit(RESTART_EXIT_CODE)`
/// so a same-PID busy loop is not invisible to systemd.
struct FlapWindow {
    restarts: Vec<Instant>,
}

impl FlapWindow {
    fn new() -> Self {
        Self {
            restarts: Vec::new(),
        }
    }

    /// Records a restart and reports whether the recent rate exceeds the cap.
    fn record_and_is_flapping(&mut self) -> bool {
        let now = Instant::now();
        self.restarts
            .retain(|t| now.duration_since(*t) < FLAP_WINDOW);
        self.restarts.push(now);
        self.restarts.len() > FLAP_CAP
    }
}

/// Between-generations finalizer for a restart: reap straggler children, then
/// confirm the messaging port is free before the next `start_router`. A still-
/// bound port the loop cannot recover from exits with [`RESTART_EXIT_CODE`].
fn finalize_before_restart() {
    // Node children were reaped only by detached exit-watcher tasks that died with
    // the just-dropped runtime; because the process is long-lived (no execv/exit
    // re-parenting to init) an un-reaped child becomes a persistent zombie. A
    // single WNOHANG pass can miss a killed-but-not-yet-zombie child, so loop with
    // a short sleep until no children remain or the deadline.
    reap_stragglers(Duration::from_secs(2));

    // Verify the messaging listen endpoint is free before the next generation's
    // start_router. TCP-only today (the local router listens on tcp/); if the old
    // zenohd has not released the port after a short bounded retry the in-process
    // loop cannot recover, so exit for the supervisor rather than spin.
    let port = super::builder::extract_messaging_port();
    if !wait_port_free(port, Duration::from_secs(5)) {
        error!(
            port,
            "messaging port still bound after the previous generation tore down; \
             exiting ({RESTART_EXIT_CODE}) for the supervisor to recover"
        );
        std::process::exit(RESTART_EXIT_CODE);
    }
}

/// Reaps zombie children with a bounded blocking `waitpid(-1, WNOHANG)` loop
/// until no children remain (`ECHILD`) or `deadline` elapses.
fn reap_stragglers(deadline: Duration) {
    use rustix::process::{WaitOptions, wait};
    let start = Instant::now();
    loop {
        match wait(WaitOptions::NOHANG) {
            // Reaped one; keep draining the rest immediately.
            Ok(Some(_)) => {}
            // Children exist but none is a zombie yet: wait briefly and retry.
            Ok(None) => {
                if start.elapsed() >= deadline {
                    break;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            // ECHILD (no children left) or any other error: nothing to reap.
            Err(_) => break,
        }
    }
}

/// Whether the local messaging port has been released by the old router. Probes
/// by connecting to loopback: a refused connect means nothing is listening.
/// Retries until free or `deadline`. TCP-only (documented assumption).
fn wait_port_free(port: u16, deadline: Duration) -> bool {
    use std::net::{Ipv4Addr, SocketAddr, TcpStream};
    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    let start = Instant::now();
    loop {
        if TcpStream::connect_timeout(&addr, Duration::from_millis(200)).is_err() {
            return true;
        }
        if start.elapsed() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A test async command that fires (or drops) its readiness gate then exits,
    /// with the gate marked required or optional.
    struct FakeReady {
        fire: bool,
        required: bool,
    }

    impl ServeAsyncCommand for FakeReady {
        fn run(self: Box<Self>) -> ServeAsyncHandle {
            let (ready_tx, ready_rx) = oneshot::channel();
            let fire = self.fire;
            let future: ServeFuture = Box::pin(async move {
                if fire {
                    let _ = ready_tx.send(());
                } else {
                    drop(ready_tx);
                }
                Ok(())
            });
            if self.required {
                ServeAsyncHandle::new(future, Some(ready_rx))
            } else {
                ServeAsyncHandle::new_optional_ready(future, ready_rx)
            }
        }
    }

    /// A best-effort (optional) readiness gate that drops without firing must not
    /// fail daemon startup; `serve` proceeds (standalone) and the run completes.
    #[test]
    fn optional_gate_drop_does_not_fail_startup() {
        let composite = CompositeCommand::default()
            .add_async_command(Box::new(FakeReady {
                fire: true,
                required: true,
            }))
            .add_async_command(Box::new(FakeReady {
                fire: false,
                required: false,
            }));
        Serve::new(composite)
            .execute()
            .expect("a dropped optional gate must not fail startup");
    }

    /// A short-lived task with no readiness gate, so the run ends when it does.
    struct FakeWork;

    impl ServeAsyncCommand for FakeWork {
        fn run(self: Box<Self>) -> ServeAsyncHandle {
            let future: ServeFuture = Box::pin(async move {
                tokio::time::sleep(Duration::from_millis(50)).await;
                Ok(())
            });
            ServeAsyncHandle::new(future, None)
        }
    }

    /// Dropping the restart sender (which happens whenever the federation
    /// control task tears down on a real shutdown signal) must NOT be read as a
    /// restart request: the run ends with `Stop`, or ctrl+C would restart the
    /// daemon instead of killing it.
    #[test]
    fn dropped_restart_sender_is_not_a_restart() {
        let (restart_tx, restart_rx) = watch::channel(false);
        drop(restart_tx);
        let composite = CompositeCommand::default().add_async_command(Box::new(FakeWork));
        let outcome = Serve::new(composite)
            .with_restart_rx(restart_rx)
            .execute()
            .expect("serve run failed");
        assert_eq!(outcome, ServeOutcome::Stop);
    }

    /// A real `true` on the restart channel still requests a restart.
    #[test]
    fn restart_signal_requests_a_restart() {
        let (restart_tx, restart_rx) = watch::channel(false);
        let _ = restart_tx.send(true);
        let composite = CompositeCommand::default().add_async_command(Box::new(FakeWork));
        let outcome = Serve::new(composite)
            .with_restart_rx(restart_rx)
            .execute()
            .expect("serve run failed");
        assert_eq!(outcome, ServeOutcome::Restart);
    }

    /// A required readiness gate that drops without firing still aborts startup.
    #[test]
    fn required_gate_drop_fails_startup() {
        let composite = CompositeCommand::default().add_async_command(Box::new(FakeReady {
            fire: false,
            required: true,
        }));
        assert!(
            Serve::new(composite).execute().is_err(),
            "a dropped required gate must fail startup"
        );
    }
}
