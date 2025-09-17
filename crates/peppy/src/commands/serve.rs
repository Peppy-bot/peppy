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
    fn run(&self) -> ServeFuture;
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
        for command in &self.commands {
            command.execute()?;
        }

        let mut futures: Vec<ServeFuture> = Vec::new();
        for async_command in &self.async_commands {
            futures.push(async_command.run());
        }

        Ok(futures)
    }
}

pub struct Serve {
    composite_command: CompositeCommand,
}

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
    pub strict: bool,
}

impl Command for ServeCommand {
    fn execute(self, ctx: &AppContext) -> Result<()> {
        // TODO: Only one instance of `serve` can run on a given machine (prod or dev included). Check the port and PID to make sure there isn't more than one instance running
        let executor = ServeCommandBuilder::new(self.root_config_path, self.strict)?
            .with_node_watcher(ctx)
            .with_messaging_router(self.engine)
            .with_peppygen()
            .with_root_node()
            .build();

        if let Err(e) = executor.execute() {
            error!("Serve command failed: {}", e);
        }
        Ok(())
    }
}
