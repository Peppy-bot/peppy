use std::path::Path;
use std::sync::Arc;

use super::CompositeCommand;
use super::Serve;
use super::master_node::MasterNodeRunner;
use super::messaging_router::MessagingRouter;
use crate::{AppContext, Error, Result};
use node_stack::NodeStack;
use pmi::Messenger;
use pmi::MessengerAdapter;
use pmi::MockAdapter;
use pmi::ZenohAdapter;
use tokio::sync::Mutex;
use tracing::warn;

pub struct ServeCommandBuilder {
    composite_command: CompositeCommand,
    messenger: Option<Arc<Mutex<Messenger>>>,
    node_stack: NodeStack,
    master_node_requested: bool,
}

impl ServeCommandBuilder {
    pub fn new() -> Result<Self> {
        Ok(Self {
            composite_command: CompositeCommand::default(),
            messenger: None,
            node_stack: NodeStack::new(),
            master_node_requested: false,
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
        let messenger = Arc::new(Mutex::new(Messenger::new(adapter)));
        self.messenger = Some(Arc::clone(&messenger));
        self.composite_command = self
            .composite_command
            .add_async_command(Box::new(MessagingRouter::new(messenger)));
        self
    }

    pub fn with_master_node(mut self) -> Result<Self> {
        self.master_node_requested = true;
        Ok(self)
    }

    pub fn with_node_stack(self, ctx: &AppContext) -> Self {
        ctx.set_node_stack(self.node_stack.clone());
        self
    }

    pub fn build(mut self) -> Result<Serve> {
        if self.master_node_requested {
            if let Some(messenger) = &self.messenger {
                let master_node = MasterNodeRunner::new(Arc::clone(messenger));
                self.node_stack.push_config(master_node.config().clone());
                self.composite_command = self
                    .composite_command
                    .add_async_command(Box::new(master_node));
            } else {
                warn!("Commands listener requires a messaging router");
                return Err(Error::MissingMessagingRouter);
            }
        }

        Ok(Serve::new(self.composite_command))
    }
}
