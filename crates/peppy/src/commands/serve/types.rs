use crate::Result;
use std::thread::JoinHandle;

pub trait ServeSubCommand: Send + Sync {
    fn execute(&self) -> Result<()>;
}

pub trait AsyncServeSubCommand: Send + Sync {
    fn execute_async(&self) -> Result<JoinHandle<Result<()>>>;
}

#[derive(Clone)]
pub struct CommandContext {
    pub host: String,
    pub port: u16,
}

impl CommandContext {
    pub fn new(host: String, port: u16) -> Self {
        Self { host, port }
    }
}

pub struct CompositeCommand {
    commands: Vec<Box<dyn ServeSubCommand>>,
    async_commands: Vec<Box<dyn AsyncServeSubCommand>>,
}

impl CompositeCommand {
    pub fn new() -> Self {
        Self {
            commands: Vec::new(),
            async_commands: Vec::new(),
        }
    }

    pub fn add_command(mut self, command: Box<dyn ServeSubCommand>) -> Self {
        self.commands.push(command);
        self
    }

    pub fn add_async_command(mut self, command: Box<dyn AsyncServeSubCommand>) -> Self {
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
                Err(e) => eprintln!("Thread panicked: {:?}", e),
                Ok(Err(e)) => eprintln!("Command error: {}", e),
                Ok(Ok(())) => {}
            }
        }

        Ok(())
    }
}
