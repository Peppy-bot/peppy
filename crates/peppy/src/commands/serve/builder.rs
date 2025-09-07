use std::path::PathBuf;

use super::CompositeCommand;
use super::Serve;
use super::node_watcher_cmd::NodeWatcher;
use pmi::{MessagingEngineContext, Messenger};

pub struct ServeCommandBuilder {
    context: MessagingEngineContext,
    composite_command: CompositeCommand,
}

impl ServeCommandBuilder {
    pub fn new(engine: String, config_path: Option<PathBuf>) -> Self {
        let context = MessagingEngineContext::new(engine, config_path);
        Self {
            context,
            composite_command: CompositeCommand::default(),
        }
    }

    pub fn with_node_watcher(mut self) -> Self {
        let watcher = Box::new(NodeWatcher {});
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

    pub fn build(self) -> super::Serve {
        Serve::new(self.composite_command)
    }
}
