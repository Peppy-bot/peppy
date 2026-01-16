use std::marker::PhantomData;
use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tracing::info;

use crate::error::{Error, Result};
use crate::runtime::node_runner::NodeRunner;
use crate::runtime::processor::Processor;
use crate::services::health::listen_for_node_health;
use crate::services::ready::listen_for_node_ready;
use crate::services::shutdown::listen_for_shutdown;
use config::consts::{DEFAULT_ZENOH_HOST, DEFAULT_ZENOH_PORT, NODE_CONFIG_FILE};

/// Execution mode for the node
#[derive(Debug, Clone)]
pub enum ExecutionMode {
    /// Auto-detect based on PEPPY_RUNTIME_CONFIG presence
    Auto,
    /// Force daemon mode - requires PEPPY_RUNTIME_CONFIG
    Daemon,
    /// Standalone mode with optional configuration
    Standalone(StandaloneConfig),
}

impl Default for ExecutionMode {
    fn default() -> Self {
        Self::Auto
    }
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

    /// Set runtime parameters
    pub fn with_parameters(mut self, params: serde_json::Value) -> Self {
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

    /// Set messaging host (defaults to DEFAULT_ZENOH_HOST)
    pub fn with_messaging_host(mut self, host: impl Into<String>) -> Self {
        self.messaging_host = Some(host.into());
        self
    }

    /// Set messaging port (defaults to DEFAULT_ZENOH_PORT)
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
            .unwrap_or_else(|| DEFAULT_ZENOH_HOST.to_string())
    }

    pub(crate) fn messaging_port_or_default(&self) -> u16 {
        self.messaging_port.unwrap_or(DEFAULT_ZENOH_PORT)
    }
}

/// Builder for configuring and running a Peppy node.
///
/// # Examples
///
/// ## Auto-detect mode (recommended for most cases)
/// ```ignore
/// NodeBuilder::<MyParams>::new()
///     .auto_detect()
///     .run(|params, node_runner| async move {
///         // your code
///         Ok(())
///     })
/// ```
///
/// ## Standalone with custom messaging
/// ```ignore
/// NodeBuilder::<MyParams>::new()
///     .standalone(StandaloneConfig::new()
///         .with_messaging("192.168.1.100", 7448))
///     .run(|params, node_runner| async move {
///         Ok(())
///     })
/// ```
///
/// ## Direct async for debugging
/// ```ignore
/// #[tokio::main]
/// async fn main() -> Result<()> {
///     let ctx = NodeBuilder::<MyParams>::new().auto_detect().init()?;
///     let node_runner = ctx.create_node_runner().await?;
///     let params = ctx.parameters()?;
///
///     // breakpoints work here
///     my_function(node_runner, params).await
/// }
/// ```
pub struct NodeBuilder<Params> {
    mode: ExecutionMode,
    peppy_config_path: PathBuf,
    _params: PhantomData<Params>,
}

impl<Params> NodeBuilder<Params>
where
    Params: serde::de::DeserializeOwned + schemars::JsonSchema,
{
    /// Create a new NodeBuilder with auto-detect mode
    pub fn new() -> Self {
        Self {
            mode: ExecutionMode::Auto,
            peppy_config_path: PathBuf::from(NODE_CONFIG_FILE),
            _params: PhantomData,
        }
    }

    /// Auto-detect execution mode based on environment.
    /// - PEPPY_RUNTIME_CONFIG set → Daemon mode
    /// - Otherwise → Standalone mode with default messaging
    pub fn auto_detect(mut self) -> Self {
        self.mode = ExecutionMode::Auto;
        self
    }

    /// Force daemon mode (requires PEPPY_RUNTIME_CONFIG)
    pub fn daemon(mut self) -> Self {
        self.mode = ExecutionMode::Daemon;
        self
    }

    /// Force standalone mode with configuration
    pub fn standalone(mut self, config: StandaloneConfig) -> Self {
        self.mode = ExecutionMode::Standalone(config);
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
            ExecutionMode::Auto => unreachable!("Auto is resolved before this point"),
        };

        Ok(NodeContext {
            processor,
            mode: resolved_mode,
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
        match &self.mode {
            ExecutionMode::Auto => {
                if std::env::var(config::consts::RUNTIME_CONFIG_VAR_NAME).is_ok() {
                    ExecutionMode::Daemon
                } else {
                    ExecutionMode::Standalone(StandaloneConfig::new())
                }
            }
            other => other.clone(),
        }
    }
}

impl<Params> Default for NodeBuilder<Params>
where
    Params: serde::de::DeserializeOwned + schemars::JsonSchema,
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
    _params: PhantomData<Params>,
}

impl<Params> NodeContext<Params>
where
    Params: serde::de::DeserializeOwned + schemars::JsonSchema,
{
    /// Create the NodeRunner, connecting to the messaging system.
    pub async fn create_node_runner(&self) -> Result<Arc<NodeRunner>> {
        let node_runner = NodeRunner::new(self.processor.clone()).await?;
        Ok(Arc::new(node_runner))
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

    fn run_with_closure<F, Fut>(self, setup_fn: F) -> Result<()>
    where
        F: FnOnce(Params, Arc<NodeRunner>) -> Fut,
        Fut: std::future::Future<Output = Result<()>>,
    {
        let rt = tokio::runtime::Runtime::new().map_err(|source| Error::RuntimeInitialization {
            context: "node runner".to_string(),
            source,
        })?;

        rt.block_on(async move {
            let node_runner = self.create_node_runner().await?;
            let parameters: Params = self.parameters()?;

            if self.is_standalone() {
                info!(
                    "Running in standalone mode [{}:{}] as '{}/{}'",
                    self.messaging_host(),
                    self.messaging_port(),
                    self.node_name(),
                    self.instance_id(),
                );
                return setup_fn(parameters, node_runner).await;
            }

            // Daemon mode: full service lifecycle
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
                    return Ok(());
                }
            }

            run_post_setup_services(
                node_runner,
                pre_setup.ready_handle,
                pre_setup.shutdown_handle,
                shutdown_rx,
            )
            .await
        })
    }
}

struct PreSetupHandles {
    ready_handle: JoinHandle<Result<()>>,
    shutdown_handle: JoinHandle<Result<()>>,
    shutdown_rx: oneshot::Receiver<()>,
}

async fn start_pre_setup_services(node_runner: Arc<NodeRunner>) -> Result<PreSetupHandles> {
    let processor = node_runner.processor();

    let ready_handle = listen_for_node_ready(
        node_runner.messenger(),
        processor.bound_master_node(),
        processor.bound_instance_id(),
        processor.node_name(),
    )
    .await?;

    let (shutdown_handle, shutdown_rx) = listen_for_shutdown(
        node_runner.messenger(),
        processor.bound_master_node(),
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
    ready_handle: JoinHandle<Result<()>>,
    shutdown_handle: JoinHandle<Result<()>>,
    mut shutdown_rx: oneshot::Receiver<()>,
) -> Result<()> {
    let processor = node_runner.processor();

    let health_handle = listen_for_node_health(
        node_runner.messenger(),
        processor.bound_master_node(),
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
        }
    }

    info!("Node shutting down");
    Ok(())
}

async fn wait_for_handles(handles: Vec<JoinHandle<Result<()>>>) -> Result<()> {
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
