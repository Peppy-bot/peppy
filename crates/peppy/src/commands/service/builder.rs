use super::core_node::CoreNodeRunner;
use super::messaging_router::MessagingRouter;
use super::serve::{CompositeCommand, Serve};
use crate::daemon_state::DaemonState;
use crate::error::{Error, Result};
use config::peppy_config::PeppyConfig;
use pmi::Messenger;
use pmi::MessengerAdapter;
use pmi::MockAdapter;
use pmi::SubscriberBufferSizes;
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
    core_node_requested: bool,
    core_node_name: Option<String>,
    clock_source: super::ClockSource,
    shutdown_token: Option<CancellationToken>,
    root_dir: PathBuf,
    peppy_config: PeppyConfig,
}

impl ServeCommandBuilder {
    pub fn new(root_dir: impl Into<PathBuf>) -> Result<Self> {
        Ok(Self {
            composite_command: CompositeCommand::default(),
            messenger: None,
            messaging_ready: None,
            core_node_requested: false,
            core_node_name: None,
            clock_source: super::ClockSource::default(),
            shutdown_token: None,
            root_dir: root_dir.into(),
            peppy_config: PeppyConfig::default(),
        })
    }

    /// Supplies the daemon-global config (messaging mode + peer buffer sizes)
    /// read once at startup. Must be called before [`with_messaging_router`]
    /// (Self::with_messaging_router) so the daemon's own session is built in the
    /// configured mode.
    pub fn with_peppy_config(mut self, peppy_config: PeppyConfig) -> Self {
        self.peppy_config = peppy_config;
        self
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
                // Reconnecting session: if the router watchdog respawns zenohd,
                // the daemon's own session re-establishes (and re-declares the
                // core node's services) instead of going silent. The session's
                // mode (peer vs router-relay) and buffer sizes come from the
                // daemon-global config read at startup.
                let buffer_sizes = SubscriberBufferSizes {
                    standard: self.peppy_config.peer.standard_buffer_size,
                    high_throughput: self.peppy_config.peer.high_throughput_buffer_size,
                };
                let adapter = ZenohAdapter::with_router(
                    ZenohNetProtocol::Tcp,
                    "0.0.0.0",
                    listening_port,
                    self.peppy_config.mode.gossip(),
                    buffer_sizes,
                )?
                .with_session_reconnect();
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

    pub fn with_core_node(
        mut self,
        core_node_name: Option<String>,
        clock_source: super::ClockSource,
    ) -> Result<Self> {
        self.core_node_requested = true;
        self.core_node_name = core_node_name;
        self.clock_source = clock_source;
        Ok(self)
    }

    pub fn build(mut self) -> Result<Serve> {
        if self.core_node_requested {
            if let Some(messenger) = &self.messenger {
                let core_node = CoreNodeRunner::new(
                    Arc::clone(messenger),
                    self.core_node_name.clone(),
                    DEFAULT_NODE_STARTUP_TIMEOUT,
                    DEFAULT_NODE_START_HEALTH_TIMEOUT,
                    self.root_dir.clone(),
                    self.messaging_ready.clone(),
                    self.clock_source,
                    self.peppy_config,
                );

                // Write the daemon state file with the core node name
                let core_node_name = core_node.node_name().to_string();
                let daemon_state = DaemonState::new(
                    &core_node_name,
                    messenger.blocking_lock().messaging_port(),
                    GIT_HASH,
                );
                let state_path = daemon_state.write().map_err(|e| {
                    Error::ExecutionFailed(format!("Failed to write daemon state: {}", e))
                })?;
                info!(
                    "Daemon state written to {} with core_node_name={}",
                    state_path.display(),
                    core_node_name
                );

                self.composite_command = self
                    .composite_command
                    .add_async_command(Box::new(core_node));
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
