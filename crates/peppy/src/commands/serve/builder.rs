use std::path::Path;
use std::sync::Arc;

use super::CompositeCommand;
use super::Serve;
use super::daemon_state::DaemonState;
use super::master_node::MasterNodeRunner;
use super::messaging_router::MessagingRouter;
use crate::error::{Error, Result};
use pmi::Messenger;
use pmi::MessengerAdapter;
use pmi::MockAdapter;
use pmi::ZenohAdapter;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

pub struct ServeCommandBuilder {
    composite_command: CompositeCommand,
    messenger: Option<Arc<Mutex<Messenger>>>,
    master_node_requested: bool,
    master_node_name: Option<String>,
    shutdown_token: Option<CancellationToken>,
}

impl ServeCommandBuilder {
    pub fn new() -> Result<Self> {
        Ok(Self {
            composite_command: CompositeCommand::default(),
            messenger: None,
            master_node_requested: false,
            master_node_name: None,
            shutdown_token: None,
        })
    }

    pub fn with_shutdown_token(mut self, token: CancellationToken) -> Self {
        self.shutdown_token = Some(token);
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
        let messenger = Arc::new(Mutex::new(Messenger::new(adapter)));
        self.messenger = Some(Arc::clone(&messenger));
        self.composite_command = self
            .composite_command
            .add_async_command(Box::new(MessagingRouter::new(messenger)));
        self
    }

    pub fn with_master_node(mut self, master_name: Option<String>) -> Result<Self> {
        self.master_node_requested = true;
        self.master_node_name = master_name;
        Ok(self)
    }

    /// Returns a clone of the shared messenger, if one has been configured.
    /// This is useful for tests that need to share the messenger with other components.
    pub fn messenger(&self) -> Option<Arc<Mutex<Messenger>>> {
        self.messenger.clone()
    }

    pub fn build(mut self) -> Result<Serve> {
        if self.master_node_requested {
            if let Some(messenger) = &self.messenger {
                let master_node =
                    MasterNodeRunner::new(Arc::clone(messenger), self.master_node_name.clone());

                // Write the daemon state file with the master node name
                let master_node_name = master_node.node_name().to_string();
                let daemon_state = DaemonState::new(&master_node_name);
                let state_path = daemon_state.write().map_err(|e| {
                    Error::ExecutionFailed(format!("Failed to write daemon state: {}", e))
                })?;
                info!(
                    "Wrote daemon state to {} with master_node_name={}",
                    state_path.display(),
                    master_node_name
                );

                self.composite_command = self
                    .composite_command
                    .add_async_command(Box::new(master_node));
            } else {
                warn!("Commands listener requires a messaging router");
                return Err(Error::MissingMessagingRouter);
            }
        }

        let serve = Serve::new(self.composite_command);
        let serve = match self.shutdown_token {
            Some(token) => serve.with_shutdown_token(token),
            None => serve,
        };
        Ok(serve)
    }
}
