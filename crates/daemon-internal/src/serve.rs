use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::time::{Duration, Instant};

use tokio::sync::{oneshot, watch};
use tokio::task::{JoinError, JoinSet};
use tracing::{error, info};

use crate::builder::ServeCommandBuilder;
use crate::error::{Error, Result};
use crate::shutdown_signal::ShutdownSignal;
use daemon_config::consts::PeppyDirs;
use tokio_util::sync::CancellationToken;

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

/// How the startup of a serve generation ended.
enum Startup {
    /// Every readiness gate fired.
    Ready,
    /// A shutdown signal arrived before every readiness gate fired.
    ShutdownSignal,
    /// A readiness gate dropped without firing: the handler behind it failed.
    HandlerFailed(Error),
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
    /// The readiness gate `serve` waits on before reporting ready, when the
    /// task has one (the messaging router, the core node). A gate whose sender
    /// drops without firing aborts startup: the daemon is broken.
    ready: Option<oneshot::Receiver<()>>,
}

impl ServeAsyncHandle {
    pub(crate) fn new(future: ServeFuture, ready: Option<oneshot::Receiver<()>>) -> Self {
        Self { future, ready }
    }

    fn into_parts(self) -> (ServeFuture, Option<oneshot::Receiver<()>>) {
        (self.future, self.ready)
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
        let restart_rx = self.restart_rx;

        info!("Running serve command...");
        let outcome = runtime.block_on(async move {
            // The coordinator is the one observer of the OS shutdown signals,
            // and a signal reaches only the listeners that exist when it
            // arrives. So the coordinator listens before any handler runs, and
            // a signal at any point of the run stops it, the startup included.
            let mut shutdown_signal = ShutdownSignal::listen().map_err(|e| {
                Error::ExecutionFailed(format!("Failed to listen for shutdown signal: {e}"))
            })?;

            let mut join_set = JoinSet::new();
            let mut readiness = Vec::new();
            for handle in handles {
                let (future, ready) = handle.into_parts();
                if let Some(rx) = ready {
                    readiness.push(rx);
                }
                join_set.spawn(future);
            }

            let startup =
                Self::wait_for_startup(readiness, &mut join_set, &mut shutdown_signal).await;
            let reason = match startup {
                Startup::Ready => {
                    info!("Serve command initialized!");
                    Self::run_until_stop(
                        &mut join_set,
                        &mut shutdown_signal,
                        external_shutdown,
                        restart_rx,
                    )
                    .await
                }
                Startup::ShutdownSignal => {
                    info!("Shutdown signal received during startup");
                    Ok(ServeOutcome::Stop)
                }
                Startup::HandlerFailed(error) => Err(error),
            };

            match &reason {
                Ok(outcome) => info!("Tearing down serve handlers (reason: {outcome:?})..."),
                Err(error) => error!("Tearing down serve handlers after a handler failed: {error}"),
            }
            // Unpark every task that observes the shared token so they run their
            // real graceful teardown (stop_session, stop_router, unlink the
            // control socket) rather than being force-dropped when the runtime
            // exits. Idempotent if already cancelled.
            teardown_token.cancel();
            while let Some(result) = join_set.join_next().await {
                Self::log_task_result(result);
            }

            reason
        })?;
        Ok(outcome)
    }

    /// Waits until every readiness gate fires, a gate drops without firing,
    /// or a shutdown signal arrives.
    async fn wait_for_startup(
        readiness: Vec<oneshot::Receiver<()>>,
        join_set: &mut JoinSet<Result<()>>,
        shutdown_signal: &mut ShutdownSignal,
    ) -> Startup {
        for ready in readiness {
            let gate = tokio::select! {
                gate = ready => gate,
                _ = shutdown_signal.recv() => return Startup::ShutdownSignal,
            };
            if gate.is_ok() {
                continue;
            }
            let error = match join_set.join_next().await {
                Some(Ok(Ok(()))) => {
                    Error::ExecutionFailed("Serve handler exited before signaling readiness".into())
                }
                Some(Ok(Err(e))) => e,
                Some(Err(join_err)) => Error::ExecutionFailed(format!(
                    "Serve handler panicked before signaling readiness: {}",
                    join_err
                )),
                None => Error::ExecutionFailed(
                    "Serve handler dropped before signaling readiness".into(),
                ),
            };
            return Startup::HandlerFailed(error);
        }
        Startup::Ready
    }

    /// The coordinator after startup: the authoritative observer of the OS
    /// shutdown signal, the external injection, and the in-process restart
    /// channel. Returns the reason of the first of them, or the error of a
    /// handler that fails mid-run. Tasks observe only the shared
    /// `teardown_token`, which the caller cancels next so each runs its real
    /// graceful teardown (no force-abort).
    ///
    /// Every branch is safe to recreate on each iteration: the JoinSet, the
    /// cancellation token and the watch channel hold state, and
    /// [`ShutdownSignal::recv`] is cancel-safe, so a signal that raced a task
    /// completion waits for the next iteration.
    async fn run_until_stop(
        join_set: &mut JoinSet<Result<()>>,
        shutdown_signal: &mut ShutdownSignal,
        external_shutdown: Option<CancellationToken>,
        mut restart_rx: Option<watch::Receiver<bool>>,
    ) -> Result<ServeOutcome> {
        loop {
            tokio::select! {
                result = join_set.join_next() => {
                    match result {
                        // A handler that returns an error mid-run is a
                        // broken daemon: tear the generation down and exit
                        // non-zero so the supervisor sees it, instead of
                        // logging once and running on half-alive.
                        Some(Ok(Err(e))) => return Err(e),
                        Some(result) => Self::log_task_result(result),
                        None => {
                            info!("All serve handlers completed. Exiting...");
                            return Ok(ServeOutcome::Stop);
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
                    return Ok(ServeOutcome::Stop);
                }
                _ = shutdown_signal.recv() => {
                    info!("Shutdown signal received");
                    return Ok(ServeOutcome::Stop);
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
                    return Ok(ServeOutcome::Restart);
                }
            }
        }
    }
}

/// Everything a daemon run needs from its embedding binary.
pub struct ServeOptions {
    /// The working root the core node resolves node sources against.
    pub root_dir: PathBuf,
    /// Messaging engine: `"zenoh"` or `"mock"` (an unknown value warns and
    /// falls back to mock).
    pub messaging_engine: String,
    /// Explicit core-node name; `None` falls back to `core_node_name` in
    /// `peppy_config.json5`, then to a machine-specific derivation.
    pub core_node_name: Option<String>,
    /// The binary's compile-time git hash, recorded in the daemon state file.
    /// Taken as data so this library reads no build-time env of its own.
    pub git_hash: String,
    /// The peppy data root: the singleton lock, `peppy_config.json5`, the
    /// daemon state file, and the core node's storage all live under it.
    /// Resolved once by the embedding binary so every consumer of the run
    /// agrees by construction. The CLI passes [`PeppyDirs::default`]; tests
    /// pass a per-test temp root so a daemon under test never reads (or
    /// mutates) the machine's real peppy home.
    pub peppy_dirs: PeppyDirs,
    /// External shutdown injection (tests / embedders): when `Some` and
    /// cancelled, the run stops cleanly. `None` in production (the CLI path).
    pub shutdown_token: Option<CancellationToken>,
}

/// Runs the daemon until a real stop (SIGINT/SIGTERM, external token, or all
/// tasks done).
///
/// In-process supervised restart loop: an identity change (enroll/unenroll)
/// tears down the current generation and rebuilds a fresh one under the
/// SAME PID, with no execv and no external supervisor, so the switch is
/// uniform across a systemd install, a launchd install, and a bare
/// `peppy service serve` terminal. A real stop (SIGTERM / `service stop`)
/// returns `Ok(())` and the process exits 0; the crash-only supervisor never
/// restarts a clean exit.
///
/// Refuses to start with [`Error::AlreadyRunning`] when another daemon holds
/// the singleton lock for the same peppy data root.
///
/// This function may terminate the process directly with
/// `RESTART_EXIT_CODE` (`75`) instead of returning: when restarts flap past
/// the in-process cap, or when the messaging port stays bound between
/// generations, the loop cannot recover and exits for the external supervisor
/// (systemd `Restart=on-failure` / launchd `KeepAlive`) to take over.
pub fn serve(options: ServeOptions) -> Result<()> {
    // One daemon per peppy data root: held for the WHOLE process lifetime,
    // above the restart loop, so an in-process restart never opens a window
    // for a second daemon. Every exit path releases it (kernel flock),
    // including SIGKILL and the process::exit calls below.
    let _singleton_lock = crate::daemon_lock::acquire_daemon_singleton_lock(&options.peppy_dirs)?;
    let mut flap = FlapWindow::new();
    loop {
        let (outcome, router_adopted) = run_one_generation(&options)?;
        match outcome {
            ServeOutcome::Stop => return Ok(()),
            ServeOutcome::Restart => {
                finalize_before_restart(router_adopted);
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

/// Builds and runs one daemon generation, returning why it stopped. Each call
/// is a clean generation: fresh sessions, a fresh `CoreNode` (so its
/// declaration guard re-runs), and the namespace and router identity
/// re-resolved from the enrollment at the top of the build.
fn run_one_generation(options: &ServeOptions) -> Result<(ServeOutcome, bool)> {
    // Read the daemon-global config, creating it with defaults if missing,
    // applied to the daemon's own session and every spawned node.
    let peppy_config = daemon_config::peppy_config::load_or_create(&options.peppy_dirs)
        .map_err(|e| Error::ExecutionFailed(format!("Failed to load peppy_config.json5: {e}")))?;

    let mut builder = ServeCommandBuilder::new(
        &options.root_dir,
        options.git_hash.clone(),
        options.peppy_dirs.clone(),
    )?
    .with_peppy_config(peppy_config)
    .with_messaging_router(options.messaging_engine.clone())?
    .with_core_node(options.core_node_name.clone())?;

    if let Some(token) = &options.shutdown_token {
        builder = builder.with_shutdown_token(token.clone());
    }

    let messenger = builder.messenger_handle();
    let executor = builder.build()?;
    let outcome = executor
        .execute()
        .inspect_err(|e| error!("Serve command failed: {}", e))?;
    let router_adopted = messenger
        .map(|messenger| messenger.blocking_lock().router_is_adopted())
        .unwrap_or(false);
    Ok((outcome, router_adopted))
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
/// confirm a managed router released the messaging port before the next
/// `start_router`. An adopted router remains outside peppy's lifecycle across
/// generations and may be local or remote.
fn finalize_before_restart(router_adopted: bool) {
    // Node children were reaped only by detached exit-watcher tasks that died with
    // the just-dropped runtime; because the process is long-lived (no execv/exit
    // re-parenting to init) an un-reaped child becomes a persistent zombie. A
    // single WNOHANG pass can miss a killed-but-not-yet-zombie child, so loop with
    // a short sleep until no children remain or the deadline.
    reap_stragglers(Duration::from_secs(2));

    if router_adopted {
        info!(
            "adopted external router remains operator-managed; skipping the managed-port-free wait"
        );
        return;
    }

    // Verify the messaging listen endpoint is free before the next generation's
    // start_router. TCP-only today (the local router listens on tcp/); if the old
    // zenohd has not released the port after a short bounded retry the in-process
    // loop cannot recover, so exit for the supervisor rather than spin.
    let port = crate::builder::extract_messaging_port();
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

    /// Every coordinator of this test process sees a shutdown signal that any
    /// test delivers to the process, so the tests that run a coordinator take
    /// turns.
    static COORDINATOR_TURN: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Runs `serve` while no other test of this process runs a coordinator.
    fn execute_alone(serve: Serve) -> Result<ServeOutcome> {
        let _turn = COORDINATOR_TURN
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        serve.execute()
    }

    /// A test async command that fires (or drops) its readiness gate then exits.
    struct FakeReady {
        fire: bool,
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
            ServeAsyncHandle::new(future, Some(ready_rx))
        }
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
        let outcome = execute_alone(Serve::new(composite).with_restart_rx(restart_rx))
            .expect("serve run failed");
        assert_eq!(outcome, ServeOutcome::Stop);
    }

    /// A real `true` on the restart channel still requests a restart.
    #[test]
    fn restart_signal_requests_a_restart() {
        let (restart_tx, restart_rx) = watch::channel(false);
        let _ = restart_tx.send(true);
        let composite = CompositeCommand::default().add_async_command(Box::new(FakeWork));
        let outcome = execute_alone(Serve::new(composite).with_restart_rx(restart_rx))
            .expect("serve run failed");
        assert_eq!(outcome, ServeOutcome::Restart);
    }

    /// A handler that fails after readiness (fires its gate, then errors),
    /// the shape of any serve task dying mid-run (e.g. the core node's
    /// listener wait surfacing a dead listener's error).
    struct FailsAfterReady;

    impl ServeAsyncCommand for FailsAfterReady {
        fn run(self: Box<Self>) -> ServeAsyncHandle {
            let (ready_tx, ready_rx) = oneshot::channel();
            let future: ServeFuture = Box::pin(async move {
                let _ = ready_tx.send(());
                tokio::time::sleep(Duration::from_millis(50)).await;
                Err(Error::ExecutionFailed("post-readiness failure".into()))
            });
            ServeAsyncHandle::new(future, Some(ready_rx))
        }
    }

    /// A handler that runs until the shared teardown token fires, pinning that
    /// a failed sibling tears the whole generation down (the run would hang
    /// here otherwise instead of returning the error).
    struct RunsUntilTeardown {
        token: CancellationToken,
    }

    impl ServeAsyncCommand for RunsUntilTeardown {
        fn run(self: Box<Self>) -> ServeAsyncHandle {
            let token = self.token;
            let future: ServeFuture = Box::pin(async move {
                token.cancelled().await;
                Ok(())
            });
            ServeAsyncHandle::new(future, None)
        }
    }

    /// A handler error after readiness must fail the whole run (non-zero exit,
    /// so a late boot refusal is visible to the supervisor and to tests
    /// waiting on the process), tearing the remaining handlers down rather
    /// than leaving the daemon half-alive.
    #[test]
    fn post_readiness_handler_error_fails_the_run_and_tears_down() {
        let teardown = CancellationToken::new();
        let composite = CompositeCommand::default()
            .add_async_command(Box::new(FailsAfterReady))
            .add_async_command(Box::new(RunsUntilTeardown {
                token: teardown.clone(),
            }));
        let err = execute_alone(Serve::new(composite).with_teardown_token(teardown))
            .expect_err("a handler failing mid-run must fail the serve run");
        assert!(
            err.to_string().contains("post-readiness failure"),
            "the run must surface the failing handler's error: {err}"
        );
    }

    /// A required readiness gate that drops without firing still aborts startup.
    #[test]
    fn required_gate_drop_fails_startup() {
        let composite =
            CompositeCommand::default().add_async_command(Box::new(FakeReady { fire: false }));
        assert!(
            execute_alone(Serve::new(composite)).is_err(),
            "a dropped required gate must fail startup"
        );
    }

    /// A handler that delivers `signal` to this process as soon as it starts,
    /// fires its readiness gate when `fires_its_gate` is set, and runs until
    /// the shared teardown token fires.
    struct SignalsThisProcess {
        signal: rustix::process::Signal,
        fires_its_gate: bool,
        token: CancellationToken,
    }

    impl ServeAsyncCommand for SignalsThisProcess {
        fn run(self: Box<Self>) -> ServeAsyncHandle {
            let (ready_tx, ready_rx) = oneshot::channel();
            let future: ServeFuture = Box::pin(async move {
                rustix::process::kill_process(rustix::process::getpid(), self.signal)
                    .expect("deliver the signal to this process");
                // A gate that does not fire stays open until the teardown, so
                // only the signal can end the startup.
                let _unfired_gate = if self.fires_its_gate {
                    let _ = ready_tx.send(());
                    None
                } else {
                    Some(ready_tx)
                };
                self.token.cancelled().await;
                Ok(())
            });
            ServeAsyncHandle::new(future, Some(ready_rx))
        }
    }

    /// A shutdown signal that arrives while the handlers start stops the run,
    /// also when every readiness gate fires right after it. A supervisor that
    /// stops the daemon as soon as it reports ready sends its signal while the
    /// coordinator still finishes its startup.
    #[test]
    fn a_shutdown_signal_before_readiness_stops_the_run() {
        let teardown = CancellationToken::new();
        let composite =
            CompositeCommand::default().add_async_command(Box::new(SignalsThisProcess {
                signal: rustix::process::Signal::TERM,
                fires_its_gate: true,
                token: teardown.clone(),
            }));
        let outcome = execute_alone(Serve::new(composite).with_teardown_token(teardown))
            .expect("serve run failed");
        assert_eq!(outcome, ServeOutcome::Stop);
    }

    /// A shutdown signal stops a run whose startup never ends.
    #[test]
    fn a_shutdown_signal_during_startup_stops_the_run() {
        let teardown = CancellationToken::new();
        let composite =
            CompositeCommand::default().add_async_command(Box::new(SignalsThisProcess {
                signal: rustix::process::Signal::INT,
                fires_its_gate: false,
                token: teardown.clone(),
            }));
        let outcome = execute_alone(Serve::new(composite).with_teardown_token(teardown))
            .expect("serve run failed");
        assert_eq!(outcome, ServeOutcome::Stop);
    }
}
