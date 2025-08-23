use super::command_pattern::{AsyncServeSubCommand, CommandContext};
use super::messaging::MessengerBackend;
use super::types::{MessagingConfiguration, Messenger};
use crate::Result;
use std::thread::{self, JoinHandle};

pub struct RouterCommand {
    context: CommandContext,
}

impl RouterCommand {
    pub fn new(context: CommandContext) -> Self {
        Self { context }
    }
}

impl AsyncServeSubCommand for RouterCommand {
    fn execute_async(&self) -> Result<JoinHandle<Result<()>>> {
        let context = self.context.clone();

        let handle = thread::spawn(move || {
            let engine_configuration = MessagingConfiguration::new(&context.host, context.port);
            let messenger = Messenger::from_config(engine_configuration);

            start_router(messenger)
        });

        Ok(handle)
    }
}

#[tokio::main]
async fn start_router(mut messenger: Messenger) -> Result<()> {
    messenger.start_router().await?;
    Ok(())
}
