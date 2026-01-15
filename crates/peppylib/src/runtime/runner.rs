use config::consts::NODE_CONFIG_FILE;
use config::NodeArguments;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tracing::info;

use crate::MessengerHandle;
use crate::config::deserialize_parameters;
use crate::error::{Error, Result};
use crate::runtime::Processor;
use crate::runtime::standalone::StandaloneConfig;
use crate::services::health::listen_for_node_health;
use crate::services::ready::listen_for_node_ready;
use crate::services::shutdown::listen_for_shutdown;
use std::sync::Arc;

struct PreSetupHandles {
    ready_handle: JoinHandle<Result<()>>,
    shutdown_handle: JoinHandle<Result<()>>,
    shutdown_rx: oneshot::Receiver<()>,
}

pub struct NodeRunner {
    messenger: MessengerHandle,
    runtime_processor: Processor,
}

impl NodeRunner {
    pub async fn new(runtime_processor: Processor) -> Result<Self> {
        let messenger: MessengerHandle = MessengerHandle::from_host_port(
            runtime_processor.messaging_host(),
            runtime_processor.messaging_port(),
        )
        .await?;

        Ok(Self {
            messenger,
            runtime_processor,
        })
    }

    pub fn runtime(&self) -> &Processor {
        &self.runtime_processor
    }

    pub fn messenger(&self) -> &MessengerHandle {
        &self.messenger
    }
}

pub fn run<F, Fut, Params>(setup_fn: F) -> Result<()>
where
    F: FnOnce(Params, Arc<NodeRunner>) -> Fut,
    Fut: std::future::Future<Output = Result<()>>,
    Params: serde::de::DeserializeOwned,
{
    let rt = tokio::runtime::Runtime::new().map_err(|source| Error::RuntimeInitialization {
        context: "node runner".to_string(),
        source,
    })?;
    let peppy_config = std::path::PathBuf::from(NODE_CONFIG_FILE);
    let runtime_processor = Processor::new_with_peppy_config(peppy_config)?;

    rt.block_on(async move {
        let node_runner = Arc::new(NodeRunner::new(runtime_processor).await?);
        let parameters: Params = deserialize_parameters(node_runner.runtime().input_arguments())?;

        // Start ready and shutdown listeners BEFORE setup_fn - this allows the master to:
        // 1. Detect if user code hangs during initialization (node responds to ready but not health)
        // 2. Request shutdown even if setup_fn is blocking
        let pre_setup = mandatory_pre_setup_services(Arc::clone(&node_runner)).await?;
        let mut shutdown_rx = pre_setup.shutdown_rx;

        tokio::select! {
            result = setup_fn(parameters, Arc::clone(&node_runner)) => {
                result?;
            }
            _ = &mut shutdown_rx => {
                info!("Shutdown requested during setup, aborting...");
                return Ok(());
            }
        }

        mandatory_post_setup_services(
            node_runner,
            pre_setup.ready_handle,
            pre_setup.shutdown_handle,
            shutdown_rx,
        )
        .await?;

        Ok(())
    })
    // Runtime drops here → all spawned tasks are cancelled
}

/// Runs a node in standalone mode without requiring the peppy daemon.
///
/// This allows running nodes directly with `cargo run` while still having
/// full messaging capabilities with a local Zenoh router.
///
/// Unlike `run()`, this function:
/// - Does not read from `PEPPY_RUNTIME_CONFIG` environment variable
/// - Does not validate the peppy.json5 fingerprint
/// - Does not start ready/health/shutdown services (no daemon to coordinate with)
/// - Runs until the setup_fn completes or ctrl-c is received
///
/// # Example
/// ```ignore
/// let config = StandaloneConfig::new("127.0.0.1", 7448, "my_node", "instance_1", my_params);
///
/// run_standalone(config, |params, node_runner| async move {
///     // Use node_runner.messenger() for topics/services
///     Ok(())
/// })
/// ```
pub fn run_standalone<F, Fut, Params>(config: StandaloneConfig<Params>, setup_fn: F) -> Result<()>
where
    F: FnOnce(Params, Arc<NodeRunner>) -> Fut,
    Fut: std::future::Future<Output = Result<()>>,
    Params: serde::Serialize,
{
    let rt = tokio::runtime::Runtime::new().map_err(|source| Error::RuntimeInitialization {
        context: "standalone node runner".to_string(),
        source,
    })?;

    // Convert the Params to NodeArguments for the Processor
    let arguments = params_to_node_arguments(&config.parameters)?;

    let runtime_processor = Processor::new_standalone(
        &config.messaging_host,
        config.messaging_port,
        &config.node_name,
        &config.instance_id,
        config.effective_master_node(),
        arguments,
    )?;

    rt.block_on(async move {
        let node_runner = Arc::new(NodeRunner::new(runtime_processor).await?);

        info!(
            "Starting standalone node with name {} and instance_id {}...",
            node_runner.runtime().node_name(),
            node_runner.runtime().bound_instance_id(),
        );

        // In standalone mode, we run until setup_fn completes or ctrl-c
        tokio::select! {
            result = setup_fn(config.parameters, Arc::clone(&node_runner)) => {
                result?;
            }
            _ = tokio::signal::ctrl_c() => {
                info!("Received ctrl-c, shutting down standalone node...");
            }
        }

        info!("Shutting down standalone node...");
        Ok(())
    })
}

/// Converts a serializable Params struct to NodeArguments.
fn params_to_node_arguments<Params: serde::Serialize>(params: &Params) -> Result<NodeArguments> {
    let json_value = serde_json::to_value(params).map_err(|e| Error::StandaloneConfigCreation {
        reason: format!("failed to serialize parameters: {}", e),
    })?;

    serde_json::from_value(json_value).map_err(|e| Error::StandaloneConfigCreation {
        reason: format!("failed to convert parameters to NodeArguments: {}", e),
    })
}

/// Services that must start BEFORE setup_fn runs.
/// This allows the master to query the node and request shutdown even if user code hangs.
async fn mandatory_pre_setup_services(node_runner: Arc<NodeRunner>) -> Result<PreSetupHandles> {
    let runtime = node_runner.runtime();
    info!(
        "Starting node with name {} and instance_id {}...",
        runtime.node_name(),
        runtime.bound_instance_id(),
    );

    let ready_handle = listen_for_node_ready(
        node_runner.messenger(),
        runtime.bound_master_node(),
        runtime.bound_instance_id(),
        runtime.node_name(),
    )
    .await?;

    let (shutdown_handle, shutdown_rx) = listen_for_shutdown(
        node_runner.messenger(),
        runtime.bound_master_node(),
        runtime.bound_instance_id(),
        runtime.node_name(),
    )
    .await?;

    Ok(PreSetupHandles {
        ready_handle,
        shutdown_handle,
        shutdown_rx,
    })
}

/// Services that start AFTER setup_fn completes.
/// Health indicates the node is fully initialized and operational.
async fn mandatory_post_setup_services(
    node_runner: Arc<NodeRunner>,
    ready_handle: JoinHandle<Result<()>>,
    shutdown_handle: JoinHandle<Result<()>>,
    mut shutdown_rx: oneshot::Receiver<()>,
) -> Result<()> {
    let runtime = node_runner.runtime();

    let health_handle = listen_for_node_health(
        node_runner.messenger(),
        runtime.bound_master_node(),
        runtime.bound_instance_id(),
        runtime.node_name(),
    )
    .await?;

    let handles = vec![ready_handle, health_handle, shutdown_handle];

    tokio::select! {
        result = wait_for_handles(handles) => {
            result?;
        }
        _ = &mut shutdown_rx => {
            info!("Received shutdown request, stopping services...");
        }
    }

    info!("Shutting down node...");
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
