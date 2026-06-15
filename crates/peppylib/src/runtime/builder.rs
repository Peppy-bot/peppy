use std::path::PathBuf;
use std::sync::Arc;

use super::CancellationToken;
use tokio::sync::oneshot;
use tracing::info;

use crate::error::{Error, Result};
use crate::runtime::TaskHandle;
use crate::runtime::node_runner::NodeRunner;
use crate::runtime::processor::Processor;
use crate::services::clock_offset::listen_for_clock_offset;
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
    _params: std::marker::PhantomData<Params>,
}

impl<Params> NodeBuilder<Params>
where
    Params: serde::de::DeserializeOwned + schemars::JsonSchema,
{
    /// Create a new NodeBuilder
    pub fn new() -> Self {
        Self {
            standalone_config: None,
            peppy_config_path: PathBuf::from(NODE_CONFIG_FILE),
            _params: std::marker::PhantomData,
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
    /// Parses and validates parameters eagerly — any type mismatch or missing
    /// field is reported immediately rather than being deferred to
    /// [`NodeContext::parameters`].
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

        let params: Params = crate::config::deserialize_parameters(processor.input_arguments())?;

        Ok(NodeContext {
            processor,
            mode: resolved_mode,
            cancellation_token: None,
            params: Some(params),
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
        // Daemon mode takes precedence: the CLI sets PEPPY_RUNTIME_CONFIG when it
        // launches a node. This lets a node specify .standalone(config) as a
        // fallback while still running in daemon mode when launched by the CLI.
        if std::env::var(config::consts::RUNTIME_CONFIG_VAR_NAME).is_ok() {
            return ExecutionMode::Daemon;
        }

        ExecutionMode::Standalone(self.standalone_config.clone().unwrap_or_default())
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
/// Returned by `NodeBuilder::init()`. Parameters are parsed eagerly
/// during construction — call [`take_parameters`](Self::take_parameters) to
/// take the already-validated, typed parameters.
pub struct NodeContext<Params> {
    processor: Processor,
    mode: ExecutionMode,
    cancellation_token: Option<CancellationToken>,
    params: Option<Params>,
}

impl<Params> NodeContext<Params>
where
    Params: serde::de::DeserializeOwned + schemars::JsonSchema,
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

    /// Take the parsed parameters.
    ///
    /// Returns the parameters that were parsed and validated during
    /// [`NodeBuilder::init`]. Can only be called once — subsequent calls
    /// return [`Error::ParametersAlreadyTaken`].
    pub fn take_parameters(&mut self) -> Result<Params> {
        self.params.take().ok_or(Error::ParametersAlreadyTaken)
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

    /// Get the node tag
    pub fn node_tag(&self) -> &str {
        self.processor.node_tag()
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
            let parameters: Params = self.take_parameters()?;

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

            // Daemon-liveness watchdog: self-terminate if the daemon dies
            // uncatchably and stays gone past the configured grace period. Held
            // for the node's lifetime; the runtime aborts it on shutdown.
            let _daemon_watchdog = crate::services::daemon_watchdog::spawn_daemon_watchdog(
                Arc::clone(&node_runner),
                node_runner.processor().daemon_grace(),
                cancellation_token.clone(),
            )
            .await?;

            // Bridge SIGINT/SIGTERM into the cancellation token so process
            // signals converge on the same shutdown path as `peppy node stop`
            // and daemon-liveness loss: cancel, run hooks, exit.
            let _signal_bridge = spawn_signal_to_cancel_bridge(cancellation_token.clone());

            let run_result = async {
                tokio::select! {
                    result = setup_fn(parameters, Arc::clone(&node_runner)) => {
                        result?;
                    }
                    _ = &mut shutdown_rx => {
                        info!("Shutdown requested during setup");
                        return Ok(());
                    }
                    // Fired during setup by a process signal or by the watchdog
                    // when the daemon has been gone past the grace period.
                    _ = cancellation_token.cancelled() => {
                        info!("Shutdown signal during setup");
                        return Ok(());
                    }
                }
                run_post_setup_services(
                    Arc::clone(&node_runner),
                    pre_setup.ready_handle,
                    pre_setup.shutdown_handle,
                    shutdown_rx,
                    cancellation_token.clone(),
                )
                .await
            }
            .await;

            // Every exit path converges here: graceful stop, signal, daemon
            // loss, setup error, service failure. Cancel the token (idempotent)
            // so user tasks observe shutdown, then await registered cleanup
            // bounded by the daemon's grace window: the window the daemon waits
            // before SIGKILL on stop paths, and the only bound at all on the
            // daemon-death path, where nothing is left to force-kill us.
            cancellation_token.cancel();
            node_runner
                .run_shutdown_hooks(node_runner.processor().shutdown_grace())
                .await;
            run_result
        })
    }

    /// Run in standalone mode with signal handling.
    ///
    /// Sets up:
    /// - A cancellation token that is cancelled on SIGINT/SIGTERM
    /// - Graceful shutdown awaiting registered shutdown hooks
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

        let _signal_bridge = spawn_signal_to_cancel_bridge(cancellation_token.clone());

        // Run the user's setup function; an error falls through to the
        // convergence below so hooks registered before the failure still run.
        let run_result = async {
            setup_fn(parameters, Arc::clone(&node_runner)).await?;

            // Wait for a shutdown signal (or programmatic cancel) before exiting
            info!("Node running. Press Ctrl+C to shutdown.");
            cancellation_token.cancelled().await;
            Ok(())
        }
        .await;

        // Same convergence as daemon mode: cancel (idempotent), then await
        // registered cleanup bounded by the grace window (built-in default in
        // standalone mode, since there is no daemon to resolve it from).
        info!("Shutting down...");
        cancellation_token.cancel();
        node_runner
            .run_shutdown_hooks(node_runner.processor().shutdown_grace())
            .await;
        run_result
    }
}

/// Bridges process signals into the runtime's cancellation token so every stop
/// path (SIGINT/SIGTERM, `peppy node stop`, daemon-liveness loss) converges
/// on the token and the registered shutdown hooks. A second signal while the
/// shutdown is in flight force-exits immediately with the conventional
/// `128 + signo` code, so stuck cleanup can always be overridden from the
/// terminal.
#[cfg(unix)]
fn spawn_signal_to_cancel_bridge(token: CancellationToken) -> TaskHandle<()> {
    crate::runtime::spawn(async move {
        use tokio::signal::unix::{SignalKind, signal};
        let (mut sigint, mut sigterm) = match (
            signal(SignalKind::interrupt()),
            signal(SignalKind::terminate()),
        ) {
            (Ok(sigint), Ok(sigterm)) => (sigint, sigterm),
            (int_result, term_result) => {
                tracing::error!(
                    "Failed to install signal handlers (SIGINT: {:?}, SIGTERM: {:?}); \
                     signals will use their default disposition",
                    int_result.err(),
                    term_result.err(),
                );
                return;
            }
        };
        let name = tokio::select! {
            _ = sigint.recv() => "SIGINT",
            _ = sigterm.recv() => "SIGTERM",
        };
        info!("Received {name}, initiating graceful shutdown...");
        token.cancel();
        // If no second signal arrives this select never resolves; the bridge
        // task is simply dropped when the runtime tears down on exit.
        let code = tokio::select! {
            _ = sigint.recv() => 130,
            _ = sigterm.recv() => 143,
        };
        info!("Received a second signal; exiting immediately");
        std::process::exit(code);
    })
}

#[cfg(not(unix))]
fn spawn_signal_to_cancel_bridge(token: CancellationToken) -> TaskHandle<()> {
    crate::runtime::spawn(async move {
        match tokio::signal::ctrl_c().await {
            Ok(()) => {
                info!("Received Ctrl+C, initiating graceful shutdown...");
                token.cancel();
            }
            Err(e) => {
                tracing::error!("Failed to listen for Ctrl+C signal: {}", e);
            }
        }
    })
}

struct PreSetupHandles {
    ready_handle: TaskHandle<Result<()>>,
    shutdown_handle: TaskHandle<Result<()>>,
    shutdown_rx: oneshot::Receiver<()>,
}

async fn start_pre_setup_services(node_runner: Arc<NodeRunner>) -> Result<PreSetupHandles> {
    let processor = node_runner.processor();
    let as_identity =
        crate::messaging::SenderTarget::node(processor.node_name(), processor.node_tag())?;

    let ready_handle = listen_for_node_ready(
        node_runner.messenger(),
        processor.bound_core_node(),
        processor.bound_instance_id(),
        as_identity.clone(),
    )
    .await?;

    let (shutdown_handle, shutdown_rx) = listen_for_shutdown(
        node_runner.messenger(),
        processor.bound_core_node(),
        processor.bound_instance_id(),
        as_identity,
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

    let as_identity =
        crate::messaging::SenderTarget::node(processor.node_name(), processor.node_tag())?;

    // `node_health` stays in post-setup on purpose: many tests use
    // `wait_for_health` as a "setup completed" signal — they spawn a
    // consumer, wait for its health endpoint, and only then send shutdown,
    // relying on health reachability to imply that the consumer's
    // `setup_fn` already produced its observable output (e.g. the printed
    // response from a `poll` call). Registering health pre-setup
    // collapses that signal: probes succeed immediately after the process
    // connects to messaging, before `setup_fn` runs, and the test's
    // subsequent `send_shutdown` cancels `setup_fn` mid-flight. Keeping
    // health post-setup preserves the "I've finished setup" contract.
    // The corollary: a `setup_fn` that intentionally blocks forever
    // (e.g. `handle_next_request` on a discover-then-pin loser) never
    // exposes `node_health`. Tests covering that case must skip the
    // health probe and either rely on pre-setup signals (`shutdown` /
    // `node_ready`) or wait on a domain-specific signal instead.
    let health_handle = listen_for_node_health(
        node_runner.messenger(),
        processor.bound_core_node(),
        processor.bound_instance_id(),
        as_identity.clone(),
    )
    .await?;

    // `clock_offset` lets `peppy stack benchmark` read this node's measured
    // offset to the core node to normalize cross-host topic timestamps. It runs
    // an on-demand clock exchange; no user code is involved.
    let clock_offset_handle =
        listen_for_clock_offset(Arc::clone(&node_runner), as_identity).await?;

    let handles = vec![
        ready_handle,
        health_handle,
        clock_offset_handle,
        shutdown_handle,
    ];

    tokio::select! {
        result = wait_for_handles(handles) => {
            result?;
        }
        _ = &mut shutdown_rx => {
            info!("Received shutdown request");
        }
        // Fired by a process signal (via the signal bridge) or by the
        // daemon-liveness watchdog when the daemon has been gone past the grace
        // period. Converges on the same clean-shutdown path as an explicit
        // `SHUTDOWN_SERVICE`: the caller cancels the token (idempotent), runs
        // the registered shutdown hooks, and exits, so the node tears down
        // (and reaps its own children) instead of lingering as an orphan.
        _ = cancellation_token.cancelled() => {
            info!("Shutdown signal received; shutting down");
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
