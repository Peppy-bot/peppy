use super::messaging::{Engine, Messenger, MessengerBackend};
use super::types::{CommandContext, ServeAsyncCommand};
use crate::Result;
use std::thread::{self, JoinHandle};

pub struct RouterCommand {
    context: CommandContext,
}

impl RouterCommand {
    pub fn new(context: CommandContext) -> Self {
        Self { context }
    }

    #[tokio::main]
    async fn start_router(mut messenger: Messenger) -> Result<()> {
        messenger.start_router().await?;
        Ok(())
    }
}

impl ServeAsyncCommand for RouterCommand {
    fn execute_async(&self) -> Result<JoinHandle<Result<()>>> {
        let context = self.context.clone();

        let handle = thread::spawn(move || {
            let engine = Engine::from_str_with_config(&context.engine, context.host, context.port)?;
            let messenger = Messenger::from_engine(engine);

            RouterCommand::start_router(messenger)
        });

        Ok(handle)
    }
}
