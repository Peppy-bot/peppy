use crate::Result;
use crate::encoding::{LaunchRequest, LaunchResponse, NodeGenerateRequest};
use crate::names;
use crate::services::node::{perform_health_check, start_node, wait_for_ready_signal};
use bytes::Bytes;
use config::peppy_config::{BuildSystem, PeppyLauncherParser};
use config::runtime::{LauncherRuntimeConfig, RuntimeConfig};
use node_stack::{LaunchPlan, NodeEntity, NodeStack};
use peppylib::messaging::ServiceRequestContext;
use peppylib::{MessengerHandle, PeppyError, PeppyResult, ServiceMessenger};
use std::sync::Arc;
use std::time::Duration;
use tokio::process::Child;
use tokio::task::JoinHandle;
use tracing::debug;

const NODE_GENERATE_TIMEOUT: Duration = Duration::from_secs(30);

pub async fn listen_for_stack_launch(
    messenger: &MessengerHandle,
    master_node_node: &str,
    instance_id: &str,
    node_name: &str,
    node_stack: Arc<NodeStack>,
    node_startup_timeout: Duration,
    node_start_health_timeout: Duration,
) -> Result<JoinHandle<Result<()>>> {
    let messenger = messenger.clone();
    let master_node_name = master_node_node.to_string();
    let master_instance_id = instance_id.to_string();

    let mut endpoint = ServiceMessenger::listen(
        &messenger,
        master_node_node,
        instance_id,
        node_name,
        names::STACK_LAUNCH,
    )
    .await?;

    let handle = tokio::spawn(async move {
        endpoint
            .handle_requests(move |context| {
                let node_stack = Arc::clone(&node_stack);
                let messenger = messenger.clone();
                let master_node_name = master_node_name.clone();
                let master_instance_id = master_instance_id.clone();
                async move {
                    handle_stack_launch_request(
                        context,
                        node_stack,
                        messenger,
                        master_node_name,
                        master_instance_id,
                        node_startup_timeout,
                        node_start_health_timeout,
                    )
                    .await
                }
            })
            .await
            .map_err(Into::into)
    });

    Ok(handle)
}

async fn handle_stack_launch_request(
    context: ServiceRequestContext,
    node_stack: Arc<NodeStack>,
    messenger: MessengerHandle,
    master_node_name: String,
    master_instance_id: String,
    node_startup_timeout: Duration,
    node_start_health_timeout: Duration,
) -> PeppyResult<Bytes> {
    let sender_instance_id = context.message().instance_id().to_string();

    let response = match handle_stack_launch_request_inner(
        &context,
        &node_stack,
        &messenger,
        master_node_name.as_str(),
        master_instance_id.as_str(),
        node_startup_timeout,
        node_start_health_timeout,
    )
    .await
    {
        Ok(()) => LaunchResponse::new(),
        Err(error) => LaunchResponse::error(error),
    };

    response
        .encode()
        .map_err(|e| PeppyError::InvalidServiceRequest {
            identifier: sender_instance_id,
            reason: e.to_string(),
        })
}

async fn handle_stack_launch_request_inner(
    context: &ServiceRequestContext,
    node_stack: &NodeStack,
    messenger: &MessengerHandle,
    master_node_name: &str,
    master_instance_id: &str,
    node_startup_timeout: Duration,
    node_start_health_timeout: Duration,
) -> std::result::Result<(), String> {
    let sender_instance_id = context.message().instance_id();
    let payload = context.message().payload();
    let master_node_config = node_stack.root().config().clone();

    debug!("Received launcher request from {sender_instance_id}");
    let request = LaunchRequest::decode(&payload.as_bytes()).map_err(|e| e.to_string())?;

    let launcher_content = &request.peppy_launch_json5;
    let nodes_directory = request.nodes_directory.clone();
    let peppy_launcher = PeppyLauncherParser::from_content(launcher_content)
        .map_err(|e| format!("invalid peppy_launcher_json5: {e}"))?;

    let launcher_runtime_config: LauncherRuntimeConfig =
        serde_json5::from_str(&request.launch_runtime_config_json5)
            .map_err(|e| format!("invalid launcher_runtime_config_json5: {e}"))?;

    if !request.nodes_directory.is_dir() {
        return Err(format!(
            "nodes_directory is not a directory: {}",
            request.nodes_directory.display()
        ));
    }

    let plan = tokio::task::spawn_blocking(move || -> std::result::Result<LaunchPlan, String> {
        LaunchPlan::from_config(master_node_config, peppy_launcher, &nodes_directory)
            .map_err(|e| format!("failed to build launch plan: {e}"))
    })
    .await
    .map_err(|e| format!("launch plan task failed: {e}"))??;

    plan.report().validate()?;
    start_launch_plan_instances(
        &plan,
        &launcher_runtime_config,
        messenger,
        master_node_name,
        master_instance_id,
        node_startup_timeout,
        node_start_health_timeout,
    )
    .await?;
    node_stack.apply_from(plan.node_stack())?;

    Ok(())
}

async fn start_launch_plan_instances(
    plan: &LaunchPlan,
    launcher_runtime_config: &LauncherRuntimeConfig,
    messenger: &MessengerHandle,
    master_node_name: &str,
    master_instance_id: &str,
    node_startup_timeout: Duration,
    node_start_health_timeout: Duration,
) -> std::result::Result<(), String> {
    let messaging_host = &launcher_runtime_config.messaging_host;
    let messaging_port = launcher_runtime_config.messaging_port;

    let mut started_children: Vec<Child> = Vec::new();

    for planned in plan
        .report()
        .deployments()
        .iter()
        .filter(|d| d.is_resolved())
    {
        let deployment = planned.deployment();
        let node_name = deployment.name.as_str();
        let tag = deployment.tag.as_str();

        let entity = plan
            .node_stack()
            .find(node_name, tag)
            .ok_or_else(|| format!("deployment {node_name}:{tag} missing from planned stack"))?;

        // Call `node_generate` to generate the peppygen library (once per node, before starting instances)
        let response = NodeGenerateRequest::new(entity.root_path().to_path_buf())
            .with_build_system(BuildSystem::Cargo)
            .poll(
                messenger,
                master_node_name,
                master_instance_id,
                master_node_name,
                NODE_GENERATE_TIMEOUT,
            )
            .await
            .map_err(|e| format!("node_generate request failed for {node_name}:{tag}: {e}"))?;

        if !response.success {
            let msg = if response.error_message.trim().is_empty() {
                "node_generate failed with no error message".to_string()
            } else {
                response.error_message
            };
            return Err(format!(
                "failed to generate peppygen for {node_name}:{tag}: {msg}"
            ));
        }

        for deployment_instance in &deployment.instances {
            let runtime_config = RuntimeConfig::new(
                messaging_host.as_str(),
                messaging_port,
                deployment_instance.clone(),
                node_name,
                master_node_name,
            )
            .map_err(|e| format!("failed to build runtime config for {node_name}:{tag}: {e}"))?;

            let runtime_config_json5 = serde_json5::to_string(&runtime_config).map_err(|e| {
                format!("failed to serialize runtime config for {node_name}:{tag}: {e}")
            })?;

            match start_node_instance(
                messenger,
                master_node_name,
                master_instance_id,
                &entity,
                deployment_instance.instance_id.as_str(),
                &runtime_config_json5,
                node_startup_timeout,
                node_start_health_timeout,
            )
            .await
            {
                Ok(child) => started_children.push(child),
                Err(error) => {
                    for mut child in started_children {
                        let _ = child.kill().await;
                        drop(tokio::spawn(async move {
                            let _ = child.wait().await;
                        }));
                    }
                    return Err(error);
                }
            }
        }
    }

    Ok(())
}

async fn start_node_instance(
    messenger: &MessengerHandle,
    master_node_name: &str,
    master_instance_id: &str,
    entity: &NodeEntity,
    instance_id: &str,
    runtime_config_json5: &str,
    node_startup_timeout: Duration,
    node_start_health_timeout: Duration,
) -> std::result::Result<Child, String> {
    let manifest = entity.config().manifest.clone();

    let mut child = start_node(entity, runtime_config_json5).map_err(|e| {
        format!(
            "failed to start node {}:{} instance {}: {e}",
            manifest.name.as_str(),
            manifest.tag,
            instance_id
        )
    })?;

    // Phase 1: Wait for the node to signal it's ready
    if let Err(error) = wait_for_ready_signal(
        messenger,
        master_node_name,
        master_instance_id,
        manifest.name.as_str(),
        master_node_name,
        instance_id,
        node_startup_timeout,
        &mut child,
    )
    .await
    {
        return kill_and_report_launch_error(
            child,
            &manifest.name.as_str(),
            &manifest.tag,
            instance_id,
            &error,
        )
        .await;
    }

    // Phase 2: Perform health check (node should respond quickly now)
    match perform_health_check(
        messenger,
        master_node_name,
        master_instance_id,
        manifest.name.as_str(),
        master_node_name,
        instance_id,
        node_start_health_timeout,
        &mut child,
    )
    .await
    {
        Ok(()) => Ok(child),
        Err(error) => {
            kill_and_report_launch_error(
                child,
                &manifest.name.as_str(),
                &manifest.tag,
                instance_id,
                &error,
            )
            .await
        }
    }
}

/// Helper function to kill a child process during launch and report an error.
async fn kill_and_report_launch_error(
    mut child: Child,
    node_name: &str,
    tag: &str,
    instance_id: &str,
    error: &str,
) -> std::result::Result<Child, String> {
    if let Err(kill_err) = child.kill().await {
        debug!(
            "Failed to kill process for node {}:{} instance {}: {}",
            node_name, tag, instance_id, kill_err
        );
    }

    let stderr_output = child
        .wait_with_output()
        .await
        .ok()
        .map(|output| String::from_utf8_lossy(&output.stderr).to_string())
        .unwrap_or_default();

    let error_msg = if stderr_output.is_empty() {
        format!(
            "failed to start node {}:{} instance {}: {error}",
            node_name, tag, instance_id
        )
    } else {
        format!(
            "failed to start node {}:{} instance {}: {error}. Node stderr: {stderr_output}",
            node_name, tag, instance_id
        )
    };
    Err(error_msg)
}
