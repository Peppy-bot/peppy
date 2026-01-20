use super::master_node::MasterNodeRunner;
use super::messaging_router::MessagingRouter;
use super::serve::{CompositeCommand, Serve};
use crate::daemon_state::DaemonState;
use crate::error::{Error, Result};
use pmi::Messenger;
use pmi::MessengerAdapter;
use pmi::MockAdapter;
use pmi::{ZenohAdapter, ZenohNetProtocol};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{Mutex, watch};
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

/// The git hash embedded at compile time by build.rs
const GIT_HASH: &str = env!("PEPPY_GIT_HASH");

const DEFAULT_NODE_STARTUP_TIMEOUT: Duration = Duration::from_secs(600); // 10 minutes
const DEFAULT_NODE_START_HEALTH_TIMEOUT: Duration = Duration::from_secs(15);

pub struct ServeCommandBuilder {
    composite_command: CompositeCommand,
    messenger: Option<Arc<Mutex<Messenger>>>,
    messaging_ready: Option<watch::Receiver<bool>>,
    master_node_requested: bool,
    master_node_name: Option<String>,
    shutdown_token: Option<CancellationToken>,
    root_dir: PathBuf,
}

impl ServeCommandBuilder {
    pub fn new(root_dir: impl Into<PathBuf>) -> Result<Self> {
        Ok(Self {
            composite_command: CompositeCommand::default(),
            messenger: None,
            messaging_ready: None,
            master_node_requested: false,
            master_node_name: None,
            shutdown_token: None,
            root_dir: root_dir.into(),
        })
    }

    pub fn with_shutdown_token(mut self, token: CancellationToken) -> Self {
        self.shutdown_token = Some(token);
        self
    }

    /// The messaging router (Zenoh/MQTT etc...) is reponsible for message passing between the nodes and between the nodes and the peppy program
    pub fn with_messaging_router(mut self, engine: String) -> Result<Self> {
        let engine = engine.to_lowercase();
        let listening_port = extract_messaging_port();
        let adapter = match engine.as_str() {
            "zenoh" => {
                let adapter = ZenohAdapter::with_router(
                    ZenohNetProtocol::Tcp,
                    config::consts::DEFAULT_MESSAGING_HOST,
                    listening_port,
                )?;
                MessengerAdapter::Zenoh(adapter)
            }
            "mock" => MessengerAdapter::Mock(MockAdapter::default()),
            other => {
                warn!(target: "peppy::serve", "Unsupported messaging engine '{}', using mock", other);
                MessengerAdapter::Mock(MockAdapter::default())
            }
        };
        let messenger = Arc::new(Mutex::new(Messenger::new(adapter)));
        let (messaging_ready_tx, messaging_ready_rx) = watch::channel(false);
        self.messenger = Some(Arc::clone(&messenger));
        self.messaging_ready = Some(messaging_ready_rx);
        self.composite_command =
            self.composite_command
                .add_async_command(Box::new(MessagingRouter::new(
                    messenger,
                    messaging_ready_tx,
                )));
        Ok(self)
    }

    pub fn with_master_node(mut self, master_name: Option<String>) -> Result<Self> {
        self.master_node_requested = true;
        self.master_node_name = master_name;
        Ok(self)
    }

    pub fn build(mut self) -> Result<Serve> {
        if self.master_node_requested {
            if let Some(messenger) = &self.messenger {
                let master_node = MasterNodeRunner::new(
                    Arc::clone(messenger),
                    self.master_node_name.clone(),
                    DEFAULT_NODE_STARTUP_TIMEOUT,
                    DEFAULT_NODE_START_HEALTH_TIMEOUT,
                    self.root_dir.clone(),
                    self.messaging_ready.clone(),
                );

                // Write the daemon state file with the master node name
                let master_node_name = master_node.node_name().to_string();
                let daemon_state = DaemonState::new(
                    &master_node_name,
                    messenger.blocking_lock().messaging_port(),
                    GIT_HASH,
                );
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

/// Extracts the messaging port from the environment variable, falling back to the default port.
fn extract_messaging_port() -> u16 {
    std::env::var(config::consts::PEPPY_MESSAGING_PORT_VAR_NAME)
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(config::consts::DEFAULT_MESSAGING_PORT)
}
