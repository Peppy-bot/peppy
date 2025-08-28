use crate::Result;
use std::path::PathBuf;
use std::thread::JoinHandle;
use tracing::error;

pub trait ServeSyncCommand: Send + Sync {
    fn execute(&self) -> Result<()>;
}

pub trait ServeAsyncCommand: Send + Sync {
    fn execute_async(&self) -> Result<JoinHandle<Result<()>>>;
}

#[derive(Clone)]
pub struct CommandContext {
    pub engine: String,
    pub config_path: Option<PathBuf>,
}

impl CommandContext {
    pub fn new(engine: String, config_path: Option<PathBuf>) -> Self {
        Self {
            engine,
            config_path,
        }
    }
}

pub struct CompositeCommand {
    commands: Vec<Box<dyn ServeSyncCommand>>,
    async_commands: Vec<Box<dyn ServeAsyncCommand>>,
}

impl CompositeCommand {
    pub fn new() -> Self {
        Self {
            commands: Vec::new(),
            async_commands: Vec::new(),
        }
    }

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

    pub fn execute(self) -> crate::Result<()> {
        let handles = self.composite_command.execute()?;

        for handle in handles {
            match handle.join() {
                Err(e) => error!("Thread panicked: {:?}", e),
                Ok(Err(e)) => error!("Command error: {}", e),
                Ok(Ok(())) => {}
            }
        }

        Ok(())
    }
}
