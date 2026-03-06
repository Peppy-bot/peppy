use std::marker::PhantomData;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use super::CancellationToken;
use tokio::sync::oneshot;
use tracing::info;

use crate::error::{Error, Result};
use crate::runtime::TaskHandle;
use crate::runtime::node_runner::NodeRunner;
use crate::runtime::processor::Processor;
use crate::services::health::listen_for_node_health;
use crate::services::ready::listen_for_node_ready;
use crate::services::shutdown::listen_for_shutdown;
use config::consts::{DEFAULT_MESSAGING_HOST, DEFAULT_MESSAGING_PORT, NODE_CONFIG_FILE};

/// Resolved execution mode for the node runtime
#[derive(Debug, Clone)]
pub(crate) enum ExecutionMode {
    /// Daemon mode - managed by CLI via PEPPY_RUNTIME_CONFIG
    Daemon,
    /// Standalone mode with configuration
    Standalone(StandaloneConfig),
}

/// Configuration for standalone execution.
///
/// All fields are optional with sensible defaults:
/// - `messaging_host`: DEFAULT_ZENOH_HOST ("127.0.0.1")
/// - `messaging_port`: DEFAULT_ZENOH_PORT (7448)
/// - `instance_id`: "standalone"
/// - `node_name`: from peppy.json5 manifest
/// - `parameters`: empty (must be provided if node requires them)
#[derive(Debug, Clone, Default)]
pub struct StandaloneConfig {
    /// Runtime parameters (if None, defaults to empty)
    pub parameters: Option<serde_json::Value>,
    /// Node name override (if None, uses peppy.json5 manifest name)
    pub node_name: Option<String>,
    /// Instance ID (defaults to "standalone")
    pub instance_id: Option<String>,
    /// Messaging host (defaults to DEFAULT_ZENOH_HOST)
    pub messaging_host: Option<String>,
    /// Messaging port (defaults to DEFAULT_ZENOH_PORT)
    pub messaging_port: Option<u16>,
}

impl StandaloneConfig {
    pub fn new() -> Self {
        Self::default()
    }

    /// Set runtime parameters from any serializable type.
    ///
    /// # Example
    /// ```ignore
    /// #[derive(serde::Serialize)]
    /// struct MyParams {
    ///     threshold: f64,
    ///     enabled: bool,
    /// }
    ///
    /// let config = StandaloneConfig::new()
    ///     .with_parameters(&MyParams { threshold: 0.5, enabled: true });
    /// ```
    pub fn with_parameters<T: serde::Serialize>(mut self, params: &T) -> Self {
        self.parameters =
            Some(serde_json::to_value(params).expect("parameters must be serializable"));
        self
    }

    /// Set runtime parameters from a raw JSON value.
    pub fn with_parameters_json(mut self, params: serde_json::Value) -> Self {
        self.parameters = Some(params);
        self
    }

    /// Set instance ID (defaults to "standalone")
    pub fn with_instance_id(mut self, id: impl Into<String>) -> Self {
        self.instance_id = Some(id.into());
        self
    }

    /// Set node name (defaults to peppy.json5 manifest name)
    pub fn with_node_name(mut self, name: impl Into<String>) -> Self {
        self.node_name = Some(name.into());
        self
    }

    /// Set messaging host (defaults to DEFAULT_MESSAGING_HOST)
    pub fn with_messaging_host(mut self, host: impl Into<String>) -> Self {
        self.messaging_host = Some(host.into());
        self
    }

    /// Set messaging port (defaults to DEFAULT_MESSAGING_PORT)
    pub fn with_messaging_port(mut self, port: u16) -> Self {
        self.messaging_port = Some(port);
        self
    }

    /// Set both messaging host and port
    pub fn with_messaging(mut self, host: impl Into<String>, port: u16) -> Self {
        self.messaging_host = Some(host.into());
        self.messaging_port = Some(port);
        self
    }

    pub(crate) fn messaging_host_or_default(&self) -> String {
        self.messaging_host
            .clone()
            .unwrap_or_else(|| DEFAULT_MESSAGING_HOST.to_string())
    }

    pub(crate) fn messaging_port_or_default(&self) -> u16 {
        self.messaging_port.unwrap_or(DEFAULT_MESSAGING_PORT)
    }
}

/// Builder for configuring and running a Peppy node.
///
/// The builder automatically detects execution mode:
/// - If `PEPPY_RUNTIME_CONFIG` is set (by CLI), runs in daemon mode
/// - Otherwise, runs in standalone mode with the provided config (or defaults)
pub struct NodeBuilder<Params> {
    standalone_config: Option<StandaloneConfig>,
    peppy_config_path: PathBuf,
    _params: PhantomData<Params>,
}

impl<Params> NodeBuilder<Params>
where
    Params: crate::DeserializeOwned + crate::JsonSchema,
{
    /// Create a new NodeBuilder
    pub fn new() -> Self {
        Self {
            standalone_config: None,
            peppy_config_path: PathBuf::from(NODE_CONFIG_FILE),
            _params: PhantomData,
        }
    }

    /// Configure standalone mode with custom settings.
    ///
    /// This config is used as a fallback when not running in daemon mode.
    /// If the CLI launches this node (setting `PEPPY_RUNTIME_CONFIG`),
    /// daemon mode takes precedence and this config is ignored.
    pub fn standalone(mut self, config: StandaloneConfig) -> Self {
        self.standalone_config = Some(config);
        self
    }

    /// Use a custom peppy.json5 path
    pub fn with_config_path(mut self, path: impl Into<PathBuf>) -> Self {
        self.peppy_config_path = path.into();
        self
    }

    /// Initialize and return context for manual async execution.
    ///
    /// Use this when you need:
    /// - Full debugger/breakpoint support
    /// - Custom async runtime configuration
    /// - More control over the execution flow
    pub fn init(self) -> Result<NodeContext<Params>> {
        let resolved_mode = self.resolve_mode();
        let processor = match &resolved_mode {
            ExecutionMode::Daemon => Processor::new_daemon(&self.peppy_config_path)?,
            ExecutionMode::Standalone(config) => {
                Processor::new_standalone(&self.peppy_config_path, config)?
            }
        };

        Ok(NodeContext {
            processor,
            mode: resolved_mode,
            cancellation_token: None,
            _params: PhantomData,
        })
    }

    /// Run with a closure pattern.
    ///
    /// Creates a Tokio runtime internally. For custom runtime configuration
    /// or better debugging support, use `init()` instead.
    pub fn run<F, Fut>(self, setup_fn: F) -> Result<()>
    where
        F: FnOnce(Params, Arc<NodeRunner>) -> Fut,
        Fut: std::future::Future<Output = Result<()>>,
    {
        let context = self.init()?;
        context.run_with_closure(setup_fn)
    }

    fn resolve_mode(&self) -> ExecutionMode {
        // Daemon mode takes precedence - CLI sets PEPPY_RUNTIME_CONFIG
        // This allows nodes to specify .standalone(config) as a fallback
        // while still running in daemon mode when launched by the CLI
        if std::env::var(config::consts::RUNTIME_CONFIG_VAR_NAME).is_ok() {
            return ExecutionMode::Daemon;
        }

        ExecutionMode::Standalone(self.standalone_config.clone().unwrap_or_default())
    }
}

impl<Params> Default for NodeBuilder<Params>
where
    Params: crate::DeserializeOwned + crate::JsonSchema,
{
    fn default() -> Self {
        Self::new()
    }
}

/// Initialized node context for manual async execution.
///
/// Returned by `NodeBuilder::init()`. Provides access to create the
/// node runner and retrieve parameters.
pub struct NodeContext<Params> {
    processor: Processor,
    mode: ExecutionMode,
    cancellation_token: Option<CancellationToken>,
    _params: PhantomData<Params>,
}

impl<Params> NodeContext<Params>
where
    Params: crate::DeserializeOwned + crate::JsonSchema,
{
    /// Create the NodeRunner, connecting to the messaging system.
    ///
    /// If a cancellation token was set via `with_cancellation_token()`, it will be
    /// used by the NodeRunner. Otherwise, a new token is created.
    pub async fn create_node_runner(&self) -> Result<Arc<NodeRunner>> {
        let token = self.cancellation_token.clone().unwrap_or_default();
        let node_runner =
            NodeRunner::with_cancellation_token(self.processor.clone(), token).await?;
        Ok(Arc::new(node_runner))
    }

    /// Set a cancellation token for the NodeRunner.
    ///
    /// This is useful when using `init()` for manual async execution and you want
    /// to control the cancellation token yourself.
    pub fn with_cancellation_token(mut self, token: CancellationToken) -> Self {
        self.cancellation_token = Some(token);
        self
    }

    /// Deserialize and return the parameters
    pub fn parameters(&self) -> Result<Params> {
        crate::config::deserialize_parameters(self.processor.input_arguments())
    }

    /// Check if running in standalone mode
    pub fn is_standalone(&self) -> bool {
        matches!(self.mode, ExecutionMode::Standalone(_))
    }

    /// Check if running in daemon mode
    pub fn is_daemon(&self) -> bool {
        matches!(self.mode, ExecutionMode::Daemon)
    }

    /// Get the messaging host
    pub fn messaging_host(&self) -> &str {
        self.processor.messaging_host()
    }

    /// Get the messaging port
    pub fn messaging_port(&self) -> u16 {
        self.processor.messaging_port()
    }

    /// Get the node name
    pub fn node_name(&self) -> &str {
        self.processor.node_name()
    }

    /// Get the instance ID
    pub fn instance_id(&self) -> &str {
        self.processor.bound_instance_id()
    }

    fn run_with_closure<F, Fut>(mut self, setup_fn: F) -> Result<()>
    where
        F: FnOnce(Params, Arc<NodeRunner>) -> Fut,
        Fut: std::future::Future<Output = Result<()>>,
    {
        let rt = tokio::runtime::Runtime::new().map_err(|source| Error::RuntimeInitialization {
            context: "node runner".to_string(),
            source,
        })?;

        rt.block_on(async move {
            let parameters: Params = self.parameters()?;

            if self.is_standalone() {
                return self.run_standalone(parameters, setup_fn).await;
            }

            // Daemon mode: full service lifecycle
            // Create cancellation token for daemon mode so it can be triggered on shutdown
            let cancellation_token = CancellationToken::new();
            self.cancellation_token = Some(cancellation_token.clone());

            let node_runner = self.create_node_runner().await?;
            info!(
                "Running in daemon mode [{}:{}] as '{}/{}'",
                self.messaging_host(),
                self.messaging_port(),
                self.node_name(),
                self.instance_id(),
            );

            let pre_setup = start_pre_setup_services(Arc::clone(&node_runner)).await?;
            let mut shutdown_rx = pre_setup.shutdown_rx;

            tokio::select! {
                result = setup_fn(parameters, Arc::clone(&node_runner)) => {
                    result?;
                }
                _ = &mut shutdown_rx => {
                    info!("Shutdown requested during setup");
                    cancellation_token.cancel();
                    return Ok(());
                }
            }
            run_post_setup_services(
                node_runner,
                pre_setup.ready_handle,
                pre_setup.shutdown_handle,
                shutdown_rx,
                cancellation_token,
            )
            .await
        })
    }

    /// Run in standalone mode with Ctrl+C signal handling.
    ///
    /// Sets up:
    /// - A cancellation token that is cancelled on Ctrl+C
    /// - Graceful shutdown waiting for tasks to observe cancellation
    async fn run_standalone<F, Fut>(mut self, parameters: Params, setup_fn: F) -> Result<()>
    where
        F: FnOnce(Params, Arc<NodeRunner>) -> Fut,
        Fut: std::future::Future<Output = Result<()>>,
    {
        // Create cancellation token for standalone mode
        let cancellation_token = CancellationToken::new();
        self.cancellation_token = Some(cancellation_token.clone());

        let node_runner = self.create_node_runner().await?;

        info!(
            "Running in standalone mode [{}:{}] as '{}/{}'",
            self.messaging_host(),
            self.messaging_port(),
            self.node_name(),
            self.instance_id(),
        );

        // Spawn Ctrl+C signal handler
        let shutdown_token = cancellation_token.clone();
        tokio::spawn(async move {
            match tokio::signal::ctrl_c().await {
                Ok(()) => {
                    info!("Received Ctrl+C, initiating graceful shutdown...");
                    shutdown_token.cancel();
                }
                Err(e) => {
                    tracing::error!("Failed to listen for Ctrl+C signal: {}", e);
                }
            }
        });

        // Run the user's setup function
        setup_fn(parameters, Arc::clone(&node_runner)).await?;

        // Wait for Ctrl+C (cancellation signal) before exiting
        info!("Node running. Press Ctrl+C to shutdown.");
        cancellation_token.cancelled().await;

        // Give spawned tasks time to observe cancellation and clean up
        info!("Shutting down...");
        tokio::time::sleep(Duration::from_millis(100)).await;

        Ok(())
    }
}

struct PreSetupHandles {
    ready_handle: TaskHandle<Result<()>>,
    shutdown_handle: TaskHandle<Result<()>>,
    shutdown_rx: oneshot::Receiver<()>,
}

async fn start_pre_setup_services(node_runner: Arc<NodeRunner>) -> Result<PreSetupHandles> {
    let processor = node_runner.processor();

    let ready_handle = listen_for_node_ready(
        node_runner.messenger(),
        processor.bound_core_node(),
        processor.bound_instance_id(),
        processor.node_name(),
    )
    .await?;

    let (shutdown_handle, shutdown_rx) = listen_for_shutdown(
        node_runner.messenger(),
        processor.bound_core_node(),
        processor.bound_instance_id(),
        processor.node_name(),
    )
    .await?;

    Ok(PreSetupHandles {
        ready_handle,
        shutdown_handle,
        shutdown_rx,
    })
}

async fn run_post_setup_services(
    node_runner: Arc<NodeRunner>,
    ready_handle: TaskHandle<Result<()>>,
    shutdown_handle: TaskHandle<Result<()>>,
    mut shutdown_rx: oneshot::Receiver<()>,
    cancellation_token: CancellationToken,
) -> Result<()> {
    let processor = node_runner.processor();

    let health_handle = listen_for_node_health(
        node_runner.messenger(),
        processor.bound_core_node(),
        processor.bound_instance_id(),
        processor.node_name(),
    )
    .await?;

    let handles = vec![ready_handle, health_handle, shutdown_handle];

    tokio::select! {
        result = wait_for_handles(handles) => {
            result?;
        }
        _ = &mut shutdown_rx => {
            info!("Received shutdown request");
            cancellation_token.cancel();
        }
    }

    info!("Node shutting down");
    Ok(())
}

async fn wait_for_handles(handles: Vec<TaskHandle<Result<()>>>) -> Result<()> {
    futures::future::try_join_all(handles)
        .await
        .map_err(|e| Error::RuntimeInitialization {
            context: "service task panicked".to_string(),
            source: std::io::Error::other(e),
        })?
        .into_iter()
        .collect::<Result<Vec<_>>>()?;
    Ok(())
}
