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

    /// The node_watcher starts from the root node and watches over the files changes in its directory and its children directories.
    /// When a change is detected on one of the peppy.json5 file, it sends a signal to the rest of the program that the configuration of
    /// a node has been updated
    pub fn with_node_watcher(mut self) -> Self {
        let watcher = Box::new(NodeWatcher {
            strict: self.strict,
        });
        self.composite_command = self.composite_command.add_async_command(watcher);
        self
    }

    /// The messaging router (Zenoh/MQTT etc...) is reponsible for message passing between the nodes and between the nodes and the peppy program
    pub fn with_messaging_router(mut self) -> Self {
        let messenger = Box::new(
            Messenger::new(self.context.clone())
                .expect("Failed to create messenger with given context"),
        );
        self.composite_command = self.composite_command.add_async_command(messenger);
        self
    }

    /// When the node_watcher detects a change, peppygen generates the new interfaces for the clients to use.
    /// peppygen does not depend on `node_watcher`, it's only one of the components that can receive a signal
    /// for code generation. Another process that can do this is `peppy node sync <path_to_config>` when nodes
    /// are outside the root_node folder and its children.
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

    pub fn build(self) -> Serve {
        Serve::new(self.composite_command)
    }
}
