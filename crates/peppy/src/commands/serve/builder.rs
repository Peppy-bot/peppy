use std::path::PathBuf;

use crate::InterfacesGenerator;

use super::CompositeCommand;
use super::Serve;
use super::node_watcher_cmd::NodeWatcher;
use pmi::{MessagingEngineContext, Messenger};

pub struct ServeCommandBuilder {
    context: MessagingEngineContext,
    composite_command: CompositeCommand,
    strict: bool,
}

impl ServeCommandBuilder {
    pub fn new(engine: String, config_path: Option<PathBuf>, strict: bool) -> Self {
        let context = MessagingEngineContext::new(engine, config_path);
        Self {
            context,
            composite_command: CompositeCommand::default(),
            strict,
        }
    }

    pub fn with_node_watcher(mut self) -> Self {
        let watcher = Box::new(NodeWatcher {
            strict: self.strict,
        });
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

    /// When the node_watcher detects a change, peppygen generates the new interfaces for the clients to use.
    /// peppygen does not depend on `node_watcher`, it's only one of the components that can send a signal to
    /// peppygen for code generation. Another process that can do this is `peppy node sync <path_to_config>`
    /// when nodes are outside the root_node folder and its children.
    pub fn with_peppygen(mut self) -> Self {
        let generator = Box::new(InterfacesGenerator::new().expect("Failed to create peppygen"));
        self.composite_command = self.composite_command.add_async_command(generator);
        self
    }

    pub fn with_root_node(mut self) -> Self {
        // TODO start a peppy root node using the `peppycl` crate based on the config file present in the same folder where the `serve` command is run
        // The root_node starts after the messaging_router
        self
    }

    pub fn build(self) -> super::Serve {
        Serve::new(self.composite_command)
    }
}
