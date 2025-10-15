mod builder;
mod messenger_cmd;
mod node_watcher_cmd;
mod peppygen_cmd;

use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use tokio::task::JoinHandle;
use tracing::{error, info};

use super::Command;
use crate::{AppContext, Result};

use builder::ServeCommandBuilder;

pub trait ServeSyncCommand: Send + Sync {
    fn execute(&self) -> Result<()>;
}

pub type ServeFuture = Pin<Box<dyn Future<Output = Result<()>> + Send + 'static>>;

pub trait ServeAsyncCommand: Send + Sync {
    fn run(self: Box<Self>) -> ServeFuture;
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

    pub fn execute(self) -> Result<Vec<ServeFuture>> {
        for command in self.commands {
            command.execute()?;
        }

        let mut futures: Vec<ServeFuture> = Vec::new();
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
/// It operates as follow:
/// 1. Starts a zenohd separate process
/// 2. Open up the `peppy.json5` on disk where it's launched (or specified with `--node-config`)
/// 3. Look for the `deployments` key inside `peppy.json5`
/// 4. Starting from the `peppy.json5` root configuration file, look for all the nodes in the children folders and create the initial node stack
/// 5. If `deployments` is present, resolve the dependencies based on the initial node stack.
/// 6. If the non-optional `deployments` cannot be resolved, the `serve` command terminates with an error.
impl Serve {
    pub fn new(composite_command: CompositeCommand) -> Self {
        Self { composite_command }
    }

    pub fn execute(self) -> Result<()> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all() // enable I/O and time drivers
            .build()?;

        let futures = self.composite_command.execute()?;

        let mut handles: Vec<JoinHandle<Result<()>>> = Vec::with_capacity(futures.len());
        for fut in futures {
            handles.push(runtime.spawn(fut));
        }

        // Block on all async tasks
        info!("Running serve command...");
        runtime.block_on(async move {
            for handle in handles {
                match handle.await {
                    Err(e) => error!("Task panicked: {:?}", e),
                    Ok(Err(e)) => error!("Command error: {}", e),
                    Ok(Ok(())) => {}
                }
            }
        });

        Ok(())
    }
}

pub struct ServeCommand {
    pub engine: String,
    pub root_config_path: PathBuf,
}

impl Command for ServeCommand {
    fn execute(self, ctx: &AppContext) -> Result<()> {
        // TODO: Only one instance of `serve` can run on a given machine (prod or dev included). Check the port and PID to make sure there isn't more than one instance running
        let executor = ServeCommandBuilder::new(self.root_config_path)?
            .with_node_watcher(ctx)
            .with_messaging_router(self.engine)
            .with_peppygen(ctx)
            .build();

        if let Err(e) = executor.execute() {
            error!("Serve command failed: {}", e);
        }
        Ok(())
    }
}
