use crate::Result;
use crate::encoding::{NodeStartRequest, NodeStartResponse};
use bytes::Bytes;
use config::consts::RUNTIME_CONFIG_VAR_NAME;
use config::node::Name;
use config::runtime::RuntimeConfig;
use node_stack::{NodeEntity, NodeStack};
use peppylib::encoding::health::NodeHealthRequest;
use peppylib::encoding::ready::NodeReadyRequest;
use peppylib::messaging::{NODE_HEALTH_SERVICE, NODE_READY_SERVICE, ServiceRequestContext};
use peppylib::{MessengerHandle, PeppyError, PeppyResult, ServiceMessenger};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::task::JoinHandle;
use tracing::debug;

use crate::services::names;

pub async fn listen_for_node_start(
    messenger: &MessengerHandle,
    master_node_node: &str,
    instance_id: &str,
    node_name: &str,
    node_stack: Arc<NodeStack>,
    node_startup_timeout: Duration,
    node_start_health_timeout: Duration,
) -> Result<JoinHandle<Result<()>>> {
    let mut endpoint = ServiceMessenger::listen(
        messenger,
        master_node_node,
        instance_id,
        node_name,
        names::NODE_START,
    )
    .await?;

    let messenger = messenger.clone();
    let master_node_name = master_node_node.to_string();
    let caller_instance_id = instance_id.to_string();

    let handle = tokio::spawn(async move {
        endpoint
            .handle_requests(|context| {
                handle_node_start_request(
                    context,
                    node_stack.clone(),
                    messenger.clone(),
                    master_node_name.clone(),
                    caller_instance_id.clone(),
                    node_startup_timeout,
                    node_start_health_timeout,
                )
            })
            .await
            .map_err(Into::into)
    });

    Ok(handle)
}

async fn handle_node_start_request(
    context: ServiceRequestContext,
    node_stack: Arc<NodeStack>,
    messenger: MessengerHandle,
    master_node_name: String,
    caller_instance_id: String,
    node_startup_timeout: Duration,
    node_start_health_timeout: Duration,
) -> PeppyResult<Bytes> {
    let sender_instance_id = context.message().instance_id();
    handle_node_start_request_inner(
        &context,
        node_stack,
        &messenger,
        &master_node_name,
        &caller_instance_id,
        node_startup_timeout,
        node_start_health_timeout,
    )
    .await
    .map_err(|e| PeppyError::InvalidServiceRequest {
        identifier: sender_instance_id.to_string(),
        reason: e.to_string(),
    })
}

async fn handle_node_start_request_inner(
    context: &ServiceRequestContext,
    node_stack: Arc<NodeStack>,
    messenger: &MessengerHandle,
    master_node_name: &str,
    caller_instance_id: &str,
    node_startup_timeout: Duration,
    node_start_health_timeout: Duration,
) -> Result<Bytes> {
    let sender_instance_id = context.message().instance_id();
    let payload = context.message().payload();

    let NodeStartRequest {
        runtime_config_json5,
        node_name,
        tag,
    } = NodeStartRequest::decode(&payload.as_bytes())?;

    // Parse the PEPPY_RUNTIME_CONFIG from json5
    let runtime_config: RuntimeConfig = match serde_json5::from_str(&runtime_config_json5) {
        Ok(config) => config,
        Err(e) => {
            return NodeStartResponse::failure(format!(
                "Failed to parse PEPPY_RUNTIME_CONFIG: {}",
                e
            ))
            .encode();
        }
    };

    // Convert instance_id from config::peppy_config::Name to config::node::Name
    let instance_id_str = runtime_config.deployment_instance.instance_id.as_str();
    let instance_id = match Name::new(instance_id_str) {
        Ok(name) => name,
        Err(e) => {
            return NodeStartResponse::failure(format!("Invalid instance_id: {}", e)).encode();
        }
    };

    debug!(
        "Received `node_start` request from {sender_instance_id}, node={}:{}, instance_id={}",
        node_name, tag, instance_id_str
    );

    // Find the entity in the node stack by name and tag
    let entity = match node_stack.find(&node_name, &tag) {
        Some(entity) => entity,
        None => {
            return NodeStartResponse::failure(format!(
                "Node '{}:{}' not found in node stack",
                node_name, tag
            ))
            .encode();
        }
    };

    // Run the node with the runtime config
    let mut child = match run_node(&entity, &runtime_config_json5) {
        Ok(child) => child,
        Err(e) => {
            debug!("Failed to start node instance '{}': {}", instance_id_str, e);
            return NodeStartResponse::failure(format!("Failed to start node: {}", e)).encode();
        }
    };

    debug!(
        "Successfully spawned node instance '{}', waiting for ready signal...",
        instance_id_str
    );

    // Phase 1: Wait for the node to signal it's ready (covers compilation time)
    let ready_result = wait_for_ready_signal(
        messenger,
        master_node_name,
        caller_instance_id,
        runtime_config.node_name.as_str(),
        runtime_config.bound_master_node.as_str(),
        instance_id_str,
        node_startup_timeout,
        &mut child,
    )
    .await;

    if let Err(e) = ready_result {
        debug!(
            "Ready signal failed for node instance '{}': {}, killing process",
            instance_id_str, e
        );
        return kill_and_report_error(child, instance_id_str, &e).await;
    }

    debug!(
        "Node instance '{}' is ready, performing health check...",
        instance_id_str
    );

    // Phase 2: Perform health check (node should respond quickly now that it's ready)
    let health_result = perform_health_check(
        messenger,
        master_node_name,
        caller_instance_id,
        runtime_config.node_name.as_str(),
        runtime_config.bound_master_node.as_str(),
        instance_id_str,
        node_start_health_timeout,
        &mut child,
    )
    .await;

    match health_result {
        Ok(()) => {
            debug!(
                "Health check passed for node instance '{}'",
                instance_id_str
            );
            // Add the instance to the node stack now that it has successfully started
            if let Err(e) = node_stack.add_instance(&node_name, &tag, Some(&instance_id)) {
                // Kill the process since we couldn't register the instance
                if let Err(kill_err) = child.kill() {
                    debug!(
                        "Failed to kill process for node instance '{}': {}",
                        instance_id_str, kill_err
                    );
                }
                return NodeStartResponse::failure(format!("Failed to register instance: {}", e))
                    .encode();
            }
            NodeStartResponse::success().encode()
        }
        Err(e) => {
            debug!(
                "Health check failed for node instance '{}': {}, killing process",
                instance_id_str, e
            );
            kill_and_report_error(child, instance_id_str, &e).await
        }
    }
}

/// Helper function to kill a child process and report an error with stderr capture.
async fn kill_and_report_error(
    mut child: Child,
    instance_id_str: &str,
    error: &str,
) -> Result<Bytes> {
    if let Err(kill_err) = child.kill() {
        debug!(
            "Failed to kill process for node instance '{}': {}",
            instance_id_str, kill_err
        );
    }
    // Capture stderr after killing the process (best-effort) so we don't block forever
    // trying to read until EOF from a still-running process.
    let stderr_output = tokio::task::spawn_blocking(move || child.wait_with_output())
        .await
        .ok()
        .and_then(|result| result.ok())
        .map(|output| String::from_utf8_lossy(&output.stderr).to_string())
        .unwrap_or_default();

    if !stderr_output.is_empty() {
        debug!(
            "Node instance '{}' stderr: {}",
            instance_id_str, stderr_output
        );
    }
    let error_msg = if stderr_output.is_empty() {
        error.to_string()
    } else {
        format!("{}. Node stderr: {}", error, stderr_output)
    };
    NodeStartResponse::failure(error_msg).encode()
}

/// Runs a node using its manifest's launch_cmd and passes the PEPPY_RUNTIME_CONFIG as an env var.
/// Returns the spawned child process handle on success.
pub fn run_node(entity: &NodeEntity, runtime_config_json5: &str) -> std::io::Result<Child> {
    let manifest = &entity.config().manifest;

    let Some((program, args)) = manifest.launch_cmd.split_first() else {
        return Err(std::io::Error::other("launch_cmd is empty"));
    };

    debug!(
        "Running node '{}:{}' with command: {} {:?} in dir {:?}",
        manifest.name.as_str(),
        manifest.tag,
        program,
        args,
        entity.root_path()
    );

    // Write the runtime config to a file in the node's .peppy directory
    // PEPPY_RUNTIME_CONFIG expects a file path, not JSON content
    let runtime_config_path = entity
        .root_path()
        .join(".peppy")
        .join("runtime")
        .join("runtime_config.json");

    if let Some(parent) = runtime_config_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&runtime_config_path, runtime_config_json5)?;

    let mut command = Command::new(program);
    command.current_dir(entity.root_path());
    command
        .args(args)
        .env(RUNTIME_CONFIG_VAR_NAME, &runtime_config_path)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    command.spawn()
}

/// Performs a health check on a newly started node instance.
/// Polls the node's health service with a timeout and returns Ok if the node responds.
/// Also monitors the child process to detect early exits.
#[allow(clippy::too_many_arguments)]
pub async fn perform_health_check(
    messenger: &MessengerHandle,
    master_node_name: &str,
    caller_instance_id: &str,
    target_node_name: &str,
    target_master_node: &str,
    target_instance_id: &str,
    timeout: Duration,
    child: &mut Child,
) -> std::result::Result<(), String> {
    let request_payload = NodeHealthRequest::new()
        .encode()
        .map_err(|e| format!("failed to encode node health request: {e}"))?;
    let deadline = Instant::now() + timeout;
    let mut last_err: Option<PeppyError> = None;

    // Poll in short intervals to avoid a startup race where the node subscribes to
    // `node_health` after the first request has already been published.
    loop {
        // Check if the child process has exited
        match child.try_wait() {
            Ok(Some(status)) => {
                return Err(format!(
                    "node process exited before becoming healthy (status={status})"
                ));
            }
            Ok(None) => {}
            Err(err) => return Err(format!("failed to query node process status: {err}")),
        }

        let now = Instant::now();
        if now >= deadline {
            let err = last_err.unwrap_or_else(|| PeppyError::ServiceTimeout {
                instance_id: Some(target_instance_id.to_string()),
                service_name: NODE_HEALTH_SERVICE.to_string(),
            });
            return Err(format!("health check timed out: {err}"));
        }

        let remaining = deadline - now;
        let attempt_timeout = remaining.min(Duration::from_millis(500));

        match ServiceMessenger::poll(
            messenger,
            master_node_name,
            caller_instance_id,
            target_node_name,
            NODE_HEALTH_SERVICE,
            Some(target_master_node),
            Some(target_instance_id),
            request_payload.clone(),
            attempt_timeout,
        )
        .await
        {
            Ok(_) => return Ok(()),
            Err(err) => {
                last_err = Some(err);
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        }
    }
}

/// Waits for a node to signal it's ready (runner::run() has started).
/// Polls the node's ready service with a timeout and returns Ok when the node responds.
/// Also monitors the child process to detect early exits (e.g., compilation failures).
///
/// This is used during startup to wait for compilation to complete before
/// starting the health check timer.
#[allow(clippy::too_many_arguments)]
pub async fn wait_for_ready_signal(
    messenger: &MessengerHandle,
    master_node_name: &str,
    caller_instance_id: &str,
    target_node_name: &str,
    target_master_node: &str,
    target_instance_id: &str,
    timeout: Duration,
    child: &mut Child,
) -> std::result::Result<(), String> {
    let request_payload = NodeReadyRequest::new()
        .encode()
        .map_err(|e| format!("failed to encode node ready request: {e}"))?;
    let deadline = Instant::now() + timeout;
    let mut last_err: Option<PeppyError> = None;

    // Poll in short intervals to detect when the node becomes ready
    loop {
        // Check if the child process has exited (e.g., compilation failed)
        match child.try_wait() {
            Ok(Some(status)) => {
                return Err(format!(
                    "node process exited during startup (status={status})"
                ));
            }
            Ok(None) => {}
            Err(err) => return Err(format!("failed to query node process status: {err}")),
        }

        let now = Instant::now();
        if now >= deadline {
            let err = last_err.unwrap_or_else(|| PeppyError::ServiceTimeout {
                instance_id: Some(target_instance_id.to_string()),
                service_name: NODE_READY_SERVICE.to_string(),
            });
            return Err(format!(
                "startup timed out waiting for node to be ready (node may still be compiling): {err}"
            ));
        }

        let remaining = deadline - now;
        let attempt_timeout = remaining.min(Duration::from_millis(500));

        match ServiceMessenger::poll(
            messenger,
            master_node_name,
            caller_instance_id,
            target_node_name,
            NODE_READY_SERVICE,
            Some(target_master_node),
            Some(target_instance_id),
            request_payload.clone(),
            attempt_timeout,
        )
        .await
        {
            Ok(_) => return Ok(()),
            Err(err) => {
                last_err = Some(err);
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        }
    }
}
