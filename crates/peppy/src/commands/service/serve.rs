use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use tokio::sync::oneshot;
use tokio::task::{JoinError, JoinSet};
use tracing::{error, info, warn};

use super::Command;
use crate::context::AppContext;
use crate::error::{Error, Result};

use super::builder::ServeCommandBuilder;
pub use tokio_util::sync::CancellationToken;

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
    shutdown_token: Option<CancellationToken>,
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
        }
    }

    pub(crate) fn with_shutdown_token(mut self, token: CancellationToken) -> Self {
        self.shutdown_token = Some(token);
        self
    }

    pub(crate) fn execute(self) -> Result<()> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()?;

        let handles = self.composite_command.execute()?;
        let shutdown_token = self.shutdown_token;

        info!("Running serve command...");
        runtime.block_on(async move {
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
                        // A best-effort gate (federation) dropped without firing —
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
            loop {
                tokio::select! {
                    result = join_set.join_next() => {
                        match result {
                            Some(result) => Self::log_task_result(result),
                            None => {
                                info!("All serve handlers completed. Exiting...");
                                break;
                            }
                        }
                    }
                    _ = async {
                        if let Some(token) = &shutdown_token {
                            token.cancelled().await
                        } else {
                            std::future::pending::<()>().await
                        }
                    } => {
                        info!("Shutdown signal received");
                        info!("Waiting for serve handlers to finish...");
                        break;
                    }
                    signal = super::shutdown_signal::shutdown_signal() => {
                        match signal {
                            Ok(_) => {
                                info!("Shutdown signal received");
                                info!("Waiting for serve handlers to finish...");
                            }
                            Err(e) => {
                                return Err(Error::ExecutionFailed(format!("Failed to listen for shutdown signal: {}", e)));
                            }
                        }
                        break;
                    }
                }
            }

            // If shutdown was triggered by the cancellation token, abort remaining tasks
            // since they may also be waiting on ctrl_c which won't arrive
            if shutdown_token.is_some() {
                join_set.abort_all();
            }
            while let Some(result) = join_set.join_next().await {
                Self::log_task_result(result);
            }

            Ok::<(), Error>(())
        })?;
        Ok(())
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
        // Read the daemon-global config once at startup, creating it with
        // defaults if missing. Resolved from `PeppyDirs::default()` (the same
        // ~/.peppy the core node uses), and applied to the daemon's own session
        // and every spawned node. Fails loud on a malformed config.
        let peppy_dirs = config::consts::PeppyDirs::default();
        let peppy_config = config::peppy_config::load_or_create(&peppy_dirs).map_err(|e| {
            Error::ExecutionFailed(format!("Failed to load peppy_config.json5: {e}"))
        })?;

        let mut builder = ServeCommandBuilder::new(&ctx.root_dir)?
            .with_peppy_config(peppy_config)
            .with_messaging_router(self.messaging_engine)?
            .with_core_node(self.core_node_name, self.clock_source)?;

        if let Some(token) = self.shutdown_token {
            builder = builder.with_shutdown_token(token);
        }

        let executor = builder.build()?;

        match executor.execute() {
            Ok(()) => Ok(()),
            Err(e) => {
                error!("Serve command failed: {}", e);
                Err(e)
            }
        }
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
    /// fail daemon startup — `serve` proceeds (standalone) and the run completes.
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
