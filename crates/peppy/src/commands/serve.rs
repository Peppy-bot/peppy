mod builder;
mod node_watcher;

use std::path::PathBuf;
use tokio::task::JoinHandle;
use tracing::{error, info};

use super::Command;
use crate::{Error, Result};

use builder::ServeCommandBuilder;
use pmi::{Messenger, MessengerBackend};

pub trait ServeSyncCommand: Send + Sync {
    fn execute(&self) -> Result<()>;
}

pub trait ServeAsyncCommand: Send + Sync {
    fn execute_async(&self) -> Result<JoinHandle<Result<()>>>;
}

impl ServeAsyncCommand for Messenger {
    fn execute_async(&self) -> Result<JoinHandle<Result<()>>> {
        let context = self.context.clone();

        let handle = tokio::spawn(async move {
            let mut messenger =
                Messenger::new(context).map_err(Error::PeppyMessagingInterfaceError)?;

            // Starts the zenoh router
            messenger
                .init()
                .await
                .map_err(Error::PeppyMessagingInterfaceError)?;

            // Keep the messenger alive until shutdown signal (Ctrl+C)
            tokio::signal::ctrl_c().await.map_err(|e| {
                Error::ExecutionFailed(format!("Failed to listen for ctrl-c: {}", e))
            })?;

            info!("Shutting down messenger...");
            messenger
                .shutdown()
                .await
                .map_err(Error::PeppyMessagingInterfaceError)?;

            Ok(())
        });

        Ok(handle)
    }
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

    pub fn execute(self) -> Result<Vec<JoinHandle<Result<()>>>> {
        for command in &self.commands {
            command.execute()?;
        }

        let mut handles = Vec::new();
        for async_command in &self.async_commands {
            handles.push(async_command.execute_async()?);
        }

        Ok(handles)
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
        // Create the tokio runtime first
        let runtime =
            tokio::runtime::Runtime::new().map_err(|e| Error::ExecutionFailed(e.to_string()))?;

        // Enter the runtime context before executing commands that use tokio::spawn
        let _guard = runtime.enter();
        let handles = self.composite_command.execute()?;

        // Block on all async tasks
        info!("Running serve command...");
        runtime.block_on(async {
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
    pub config_path: Option<PathBuf>,
}

impl Command for ServeCommand {
    fn execute(self) -> Result<()> {
        // TODO Run a separate thread that listen to Zenoh communication so that it can internally create a map of those communication between nodes
        // TODO Run a separate thread that is a web server API that display the node communication, node list etc...
        let executor = ServeCommandBuilder::new(self.engine, self.config_path)
            .with_node_watcher()
            .with_messaging_router()
            // Future commands can be added here:
            // .with_async_command(Arc::new(ZenohListenerCommand::new(...)))
            // .with_async_command(Arc::new(WebApiCommand::new(...)))
            .build();

        if let Err(e) = executor.execute() {
            error!("Serve command failed: {}", e);
        }
        Ok(())
    }
}
