use std::path::Path;

use super::CompositeCommand;
use super::Serve;
use crate::{AppContext, Result};
use node_stack::NodeStack;
use pmi::Messenger;
use pmi::MessengerAdapter;
use pmi::MockAdapter;
use pmi::ZenohAdapter;
use tracing::warn;

pub struct ServeCommandBuilder {
    composite_command: CompositeCommand,
}

impl ServeCommandBuilder {
    pub fn new() -> Result<Self> {
        Ok(Self {
            composite_command: CompositeCommand::default(),
        })
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

    pub fn with_node_stack(self, ctx: &AppContext) -> Self {
        ctx.set_node_stack(NodeStack::new());
        self
    }

    pub fn build(self) -> Serve {
        Serve::new(self.composite_command)
    }
}
