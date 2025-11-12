use std::path::Path;
use std::path::PathBuf;

use super::CompositeCommand;
use super::Serve;
use super::node_watcher_cmd::NodeWatcher;
use crate::{AppContext, Result};
use config::peppy_config::{PeppyLauncher, PeppyLauncherParser};
use pmi::Messenger;
use pmi::MessengerAdapter;
use pmi::MockAdapter;
use pmi::ZenohAdapter;
use tracing::warn;

pub struct ServeCommandBuilder {
    composite_command: CompositeCommand,
    launcher_config: PeppyLauncher,
}

impl ServeCommandBuilder {
    pub fn new(peppy_config_path: PathBuf) -> Result<Self> {
        let peppy_config = PeppyLauncherParser::from_path(&peppy_config_path)
            .map_err(crate::Error::PeppyConfig)?;
        Ok(Self {
            composite_command: CompositeCommand::default(),
            launcher_config: peppy_config,
        })
    }

    /// When a change is detected on one of the peppy.json5 file, it sends a signal to the rest of the program that the configuration of
    /// a node has been updated
    pub fn with_node_watcher(mut self, ctx: &AppContext) -> Self {
        let watcher = Box::new(NodeWatcher::new(ctx));
        self.composite_command = self.composite_command.add_async_command(watcher);
        self
    }

    /// The messaging router (Zenoh/MQTT etc...) is reponsible for message passing between the nodes and between the nodes and the peppy program
    pub fn with_messaging_router(mut self, engine: String) -> Self {
        let engine = engine.to_lowercase();
        let adapter = match engine.as_str() {
            "zenoh" => {
                MessengerAdapter::Zenoh(ZenohAdapter::from_zenohd_config(None::<&Path>).unwrap())
            }
            "mock" => MessengerAdapter::Mock(MockAdapter::default()),
            other => {
                warn!(target: "peppy::serve", "Unsupported messaging engine '{}', using mock", other);
                MessengerAdapter::Mock(MockAdapter::default())
            }
        };
        let messenger = Box::new(Messenger::new(adapter));

        self.composite_command = self.composite_command.add_async_command(messenger);
        self
    }

    /// When the node_watcher detects a change, peppygen generates the new interfaces for the clients to use.
    /// peppygen does not depend on `node_watcher`, it's only one of the components that can receive a signal
    /// for code generation. Another process that can do this is `peppy node sync <path_to_config>` when nodes
    /// are outside the peppy config root folder and its children.
    pub fn with_peppygen(mut self, ctx: &AppContext) -> Self {
        // let generator = Box::new(
        //     InterfacesGenerator::new(ctx, self.peppy_config.interfaces.clone())
        //         .expect("Failed to create peppygen"),
        // );
        // self.composite_command = self.composite_command.add_async_command(generator);
        todo!("Finish");
        self
    }

    pub fn build(self) -> Serve {
        Serve::new(self.composite_command)
    }
}
