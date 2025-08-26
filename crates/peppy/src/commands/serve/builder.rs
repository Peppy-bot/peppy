use super::messaging::Messenger;
use super::node_watcher::NodeWatcherCommand;
use super::types::Serve;
use super::types::{CommandContext, CompositeCommand};

pub struct ServeCommandBuilder {
    context: CommandContext,
    composite_command: CompositeCommand,
}

impl ServeCommandBuilder {
    pub fn new(engine: String, host: Option<String>, port: Option<u16>) -> Self {
        let context = CommandContext::new(host, port, engine);
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
        let messenger = Box::new(
            Messenger::new(self.context.clone())
                .expect("Failed to create messenger with given context"),
        );
        self.composite_command = self.composite_command.add_async_command(messenger);
        self
    }

    pub fn build(self) -> Serve {
        Serve::new(self.composite_command)
    }
}
