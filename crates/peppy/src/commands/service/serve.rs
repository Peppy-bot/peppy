use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use tokio::sync::oneshot;
use tokio::task::{JoinError, JoinSet};
use tracing::{error, info};

use super::Command;
use crate::context::AppContext;
use crate::error::{Error, Result};

pub use super::builder::ServeCommandBuilder;
pub use tokio_util::sync::CancellationToken;

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
    async_commands: Vec<Box<dyn ServeAsyncCommand>>,
}

impl CompositeCommand {
    pub fn add_async_command(mut self, command: Box<dyn ServeAsyncCommand>) -> Self {
        self.async_commands.push(command);
        self
    }

    pub fn execute(self) -> Result<Vec<ServeAsyncHandle>> {
        let mut futures: Vec<ServeAsyncHandle> = Vec::new();
        for async_command in self.async_commands {
            futures.push(async_command.run());
        }

        Ok(futures)
    }
}

pub struct Serve {
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

    pub fn new(composite_command: CompositeCommand) -> Self {
        Self {
            composite_command,
            shutdown_token: None,
        }
    }

    pub fn with_shutdown_token(mut self, token: CancellationToken) -> Self {
        self.shutdown_token = Some(token);
        self
    }

    pub fn execute(self) -> Result<()> {
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
        let mut builder = ServeCommandBuilder::new(&ctx.root_dir)?
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
