mod builder;
mod commands_listener;
mod messaging_router;
mod pid_lock;

use std::future::Future;
use std::io::{self, Write};
use std::pin::Pin;
use tokio::sync::oneshot;
use tokio::task::{JoinError, JoinSet};
use tracing::{error, info};

use super::Command;
use crate::{AppContext, Error, Result};

use builder::ServeCommandBuilder;
use pid_lock::{PidLock, PidLockError};

pub use pid_lock::PID_FILE_ENV;

const EXISTING_INSTANCE_PROMPT: &str =
    "An instance of peppy already exists on this machine. Reset the node stack? [y/n] ";
pub const PROMPT_ANSWER_ENV: &str = "PEPPY_SERVE_PROMPT_ANSWER";

pub trait ServeSyncCommand: Send + Sync {
    fn execute(&self) -> Result<()>;
}

pub type ServeFuture = Pin<Box<dyn Future<Output = Result<()>> + Send + 'static>>;

pub struct ServeAsyncHandle {
    future: ServeFuture,
    ready: Option<oneshot::Receiver<()>>,
}

impl ServeAsyncHandle {
    pub fn new(future: ServeFuture, ready: Option<oneshot::Receiver<()>>) -> Self {
        Self { future, ready }
    }

    fn into_parts(self) -> (ServeFuture, Option<oneshot::Receiver<()>>) {
        (self.future, self.ready)
    }
}

pub trait ServeAsyncCommand: Send + Sync {
    fn run(self: Box<Self>) -> ServeAsyncHandle;
}

#[derive(Default)]
pub struct CompositeCommand {
    commands: Vec<Box<dyn ServeSyncCommand>>,
    async_commands: Vec<Box<dyn ServeAsyncCommand>>,
}

impl CompositeCommand {
    pub fn _add_command(mut self, command: Box<dyn ServeSyncCommand>) -> Self {
        self.commands.push(command);
        self
    }

    pub fn add_async_command(mut self, command: Box<dyn ServeAsyncCommand>) -> Self {
        self.async_commands.push(command);
        self
    }

    pub fn execute(self) -> Result<Vec<ServeAsyncHandle>> {
        for command in self.commands {
            command.execute()?;
        }

        let mut futures: Vec<ServeAsyncHandle> = Vec::new();
        for async_command in self.async_commands {
            futures.push(async_command.run());
        }

        Ok(futures)
    }
}

pub struct Serve {
    composite_command: CompositeCommand,
}

/// The serve command is the command that runs as a daemon in systemd and maintains a "node stack" (a graph representation of nodes)
/// It's installed using the `install` command.
/// It operates as follow:
/// 1. Starts a zenohd separate process
/// 2. Creates an internal "node stack" (a graph of nodes that depends on each other)
impl Serve {
    fn log_task_result(result: std::result::Result<Result<()>, JoinError>) {
        match result {
            Err(e) => error!("Task panicked: {:?}", e),
            Ok(Err(e)) => error!("Command error: {}", e),
            Ok(Ok(())) => {}
        }
    }

    pub fn new(composite_command: CompositeCommand) -> Self {
        Self { composite_command }
    }

    pub fn execute(self) -> Result<()> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()?;

        let handles = self.composite_command.execute()?;

        info!("Running serve command...");
        runtime.block_on(async move {
            let mut join_set = JoinSet::new();
            let mut readiness = Vec::new();
            for handle in handles {
                let (future, ready) = handle.into_parts();
                if let Some(rx) = ready {
                    readiness.push(rx);
                }
                join_set.spawn(future);
            }

            for ready in readiness {
                if ready.await.is_err() {
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
                    signal = tokio::signal::ctrl_c() => {
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
}

impl Command for ServeCommand {
    fn execute(self, ctx: &AppContext) -> Result<()> {
        let _pid_lock = match PidLock::acquire() {
            Ok(lock) => lock,
            Err(PidLockError::AlreadyRunning(pid)) => {
                let reset_requested = prompt_existing_instance()?;
                info!(
                    existing_pid = pid,
                    reset_requested, "Existing peppy instance detected"
                );
                return Err(Error::ExecutionFailed(format!(
                    "Serve command already running (PID {pid})"
                )));
            }
            Err(PidLockError::Io(err)) => return Err(err.into()),
        };

        let executor = ServeCommandBuilder::new()?
            .with_messaging_router(self.messaging_engine)
            .with_commands_listener()
            .with_node_stack(ctx)
            .build();

        match executor.execute() {
            Ok(()) => Ok(()),
            Err(e) => {
                error!("Serve command failed: {}", e);
                Err(e)
            }
        }
    }
}

fn prompt_existing_instance() -> Result<bool> {
    if let Ok(value) = std::env::var(PROMPT_ANSWER_ENV) {
        return Ok(matches!(
            value.trim().to_lowercase().as_str(),
            "y" | "yes" | "true" | "1"
        ));
    }

    print!("{}", EXISTING_INSTANCE_PROMPT);
    io::stdout().flush()?;

    let mut input = String::new();
    io::stdin().read_line(&mut input)?;

    Ok(matches!(
        input.trim().to_lowercase().as_str(),
        "y" | "yes" | "true" | "1"
    ))
}
