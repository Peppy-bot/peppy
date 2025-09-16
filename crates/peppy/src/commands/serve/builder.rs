use std::path::PathBuf;

use super::CompositeCommand;
use super::Serve;
use super::node_watcher_cmd::NodeWatcher;
use super::peppygen_cmd::InterfacesGenerator;
use crate::Result;
use config::NodeConfig;
use config::NodeConfigParser;
use pmi::{MessagingEngineContext, Messenger};

pub struct ServeCommandBuilder {
    composite_command: CompositeCommand,
    node_config: NodeConfig,
    strict: bool,
}

impl ServeCommandBuilder {
    pub fn new(root_config_path: PathBuf, strict: bool) -> Result<Self> {
        let node_config =
            NodeConfigParser::from_path(&root_config_path).map_err(crate::Error::PeppyConfig)?;
        Ok(Self {
            composite_command: CompositeCommand::default(),
            node_config,
            strict,
        })
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
    pub fn with_messaging_router(mut self, engine: String) -> Self {
        // Uses the default pubsub config or the one defined in env vars (`ZENOH_CONFIG` for zenoh)
        let context = MessagingEngineContext::new(engine, None);
        let messenger = Box::new(
            Messenger::new(context).expect("Failed to create messenger with given context"),
        );
        self.composite_command = self.composite_command.add_async_command(messenger);
        self
    }

    /// When the node_watcher detects a change, peppygen generates the new interfaces for the clients to use.
    /// peppygen does not depend on `node_watcher`, it's only one of the components that can receive a signal
    /// for code generation. Another process that can do this is `peppy node sync <path_to_config>` when nodes
    /// are outside the root_node folder and its children.
    pub fn with_peppygen(mut self) -> Self {
        let generator = Box::new(
            self.create_interfaces_generator()
                .expect("Failed to create peppygen"),
        );
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

    fn create_interfaces_generator(&self) -> Result<InterfacesGenerator> {
        InterfacesGenerator::new(
            &self.node_config.interfaces,
            &self.node_config.manifest.language,
        )
    }
}
