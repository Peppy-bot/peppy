use super::command_pattern::{CommandContext, CompositeCommand};
use super::node_watcher_command::NodeWatcherCommand;
use super::router_command::RouterCommand;

pub struct ServeCommandBuilder {
    context: CommandContext,
    composite_command: CompositeCommand,
}

impl ServeCommandBuilder {
    pub fn new(host: String, port: u16) -> Self {
        let context = CommandContext::new(host, port);
        Self {
            context,
            composite_command: CompositeCommand::new(),
        }
    }

    pub fn with_node_watcher(mut self) -> Self {
        let watcher = Box::new(NodeWatcherCommand::new(self.context.clone()));
        self.composite_command = self.composite_command.add_async_command(watcher);
        self
    }

    pub fn with_messaging_router(mut self) -> Self {
        let router = Box::new(RouterCommand::new(self.context.clone()));
        self.composite_command = self.composite_command.add_async_command(router);
        self
    }

    pub fn build(self) -> ServeExecutor {
        ServeExecutor::new(self.composite_command)
    }
}

pub struct ServeExecutor {
    composite_command: CompositeCommand,
}

impl ServeExecutor {
    fn new(composite_command: CompositeCommand) -> Self {
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
