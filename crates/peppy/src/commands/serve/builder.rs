use std::path::Path;
use std::sync::Arc;

use super::CompositeCommand;
use super::Serve;
use super::commands_listener::CommandsListener;
use super::messaging_router::MessagingRouter;
use crate::{AppContext, Result};
use node_stack::NodeStack;
use pmi::Messenger;
use pmi::MessengerAdapter;
use pmi::MockAdapter;
use pmi::ZenohAdapter;
use tokio::sync::Mutex;
use tracing::info;
use tracing::warn;

pub struct ServeCommandBuilder {
    composite_command: CompositeCommand,
    messenger: Option<Arc<Mutex<Messenger>>>,
}

impl ServeCommandBuilder {
    pub fn new() -> Result<Self> {
        Ok(Self {
            composite_command: CompositeCommand::default(),
            messenger: None,
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

    pub fn with_commands_listener(mut self) -> Self {
        if let Some(messenger) = &self.messenger {
            self.composite_command = self
                .composite_command
                .add_async_command(Box::new(CommandsListener::new(Arc::clone(messenger))));
            info!("Commands listener started!");
        } else {
            warn!("Commands listener requires a messaging router; skipping");
        }
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
