use super::master_node::MasterNodeRunner;
use super::messaging_router::MessagingRouter;
use super::serve::{CompositeCommand, Serve, ServeHandle};
use super::test_support::ServeTestConfig;
use crate::daemon_state::DaemonState;
use crate::error::{Error, Result};
use pmi::Messenger;
use pmi::MessengerAdapter;
use pmi::MockAdapter;
#[cfg(feature = "zenoh")]
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
    /// Override for DaemonState file path (used in tests for isolation)
    daemon_state_path: Option<PathBuf>,
    /// Override for git hash in DaemonState (used in tests)
    git_hash_override: Option<String>,
    /// Cached messaging port (set when messenger is configured from test config)
    messaging_port_override: Option<u16>,
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
            daemon_state_path: None,
            git_hash_override: None,
            messaging_port_override: None,
        })
    }

    pub fn with_shutdown_token(mut self, token: CancellationToken) -> Self {
        self.shutdown_token = Some(token);
        self
    }

    /// The messaging router (Zenoh/MQTT etc...) is reponsible for message passing between the nodes and between the nodes and the peppy program
    pub fn with_messaging_router(mut self, engine: String) -> Result<Self> {
        // Skip router setup if messenger is already configured (e.g., from test config)
        if self.messenger.is_some() {
            return Ok(self);
        }

        let engine = engine.to_lowercase();
        let listening_port = extract_messaging_port();
        let adapter = match engine.as_str() {
            #[cfg(feature = "zenoh")]
            "zenoh" => {
                let adapter = ZenohAdapter::with_router(
                    ZenohNetProtocol::Tcp,
                    config::consts::DEFAULT_MESSAGING_HOST,
                    listening_port,
                )?;
                MessengerAdapter::Zenoh(adapter)
            }
            #[cfg(not(feature = "zenoh"))]
            "zenoh" => {
                warn!(target: "peppy::serve", "Zenoh feature not enabled, using mock");
                MessengerAdapter::Mock(MockAdapter::default())
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

    /// Returns a clone of the shared messenger, if one has been configured.
    /// This is useful for tests that need to share the messenger with other components.
    pub fn messenger(&self) -> Option<Arc<Mutex<Messenger>>> {
        self.messenger.clone()
    }

    /// Configures the builder for test mode with the provided configuration.
    ///
    /// When test config is provided:
    /// - Uses the pre-configured messenger instead of starting a new router
    /// - Uses the provided git hash for DaemonState
    pub fn with_test_config(mut self, config: ServeTestConfig) -> Self {
        // Store the messaging port before moving the messenger
        if let Some(port) = config.messaging_port() {
            self.messaging_port_override = Some(port);
        }
        if let Some(messenger) = config.messenger {
            self.messenger = Some(messenger);
            // Create a pre-satisfied messaging_ready channel since router is already started
            let (_tx, rx) = watch::channel(true);
            self.messaging_ready = Some(rx);
        }
        if let Some(git_hash) = config.git_hash {
            self.git_hash_override = Some(git_hash);
        }
        self
    }

    /// Overrides the DaemonState file path.
    ///
    /// This is useful for tests that need to isolate DaemonState to a temporary directory.
    pub fn with_daemon_state_path(mut self, path: PathBuf) -> Self {
        self.daemon_state_path = Some(path);
        self
    }

    /// Sets the messaging port override.
    ///
    /// This is useful when the port is known ahead of time (e.g., from a test configuration).
    pub fn with_messaging_port(mut self, port: u16) -> Self {
        self.messaging_port_override = Some(port);
        self
    }

    /// Builds the serve command and returns a handle with access to internals.
    ///
    /// This is useful for tests that need access to the messenger after building.
    pub fn build_with_handle(self) -> Result<ServeHandle> {
        let messenger = self.messenger.clone();
        let master_node_name = self
            .master_node_name
            .clone()
            .unwrap_or_else(|| "master".to_string());
        // Use cached port if available (from test config), otherwise try to get it from messenger
        let messaging_port = self.messaging_port_override.unwrap_or(0);

        let serve = self.build()?;

        let messenger = messenger.ok_or(Error::MissingMessagingRouter)?;

        Ok(ServeHandle::new(
            serve,
            messenger,
            master_node_name,
            messaging_port,
        ))
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
                let git_hash = self.git_hash_override.as_deref().unwrap_or(GIT_HASH);
                // Use cached port if available (from test config), otherwise get from messenger
                let messaging_port = self
                    .messaging_port_override
                    .unwrap_or_else(|| messenger.blocking_lock().messaging_port());
                let daemon_state = DaemonState::new(&master_node_name, messaging_port, git_hash);

                // Use override path if provided, otherwise default behavior
                let state_path = if let Some(path) = &self.daemon_state_path {
                    DaemonState::write_to(path, &daemon_state).map_err(|e| {
                        Error::ExecutionFailed(format!("Failed to write daemon state: {}", e))
                    })?;
                    path.clone()
                } else {
                    daemon_state.write().map_err(|e| {
                        Error::ExecutionFailed(format!("Failed to write daemon state: {}", e))
                    })?
                };
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
