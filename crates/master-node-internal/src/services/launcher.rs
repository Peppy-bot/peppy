use std::process::Child;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use config::consts::{DEFAULT_ZENOH_HOST, DEFAULT_ZENOH_PORT};
use config::peppy_config::{BuildSystem, PeppyLauncherParser};
use config::runtime::RuntimeConfig;
use node_stack::{LaunchPlan, NodeEntity, NodeStack};
use peppylib::messaging::ServiceRequestContext;
use peppylib::{MessengerHandle, PeppyError, PeppyResult, ServiceMessenger};
use tokio::task::JoinHandle;
use tracing::debug;

use crate::Result;
use crate::encoding::{LauncherRequest, LauncherResponse};

use super::names;
use super::node::{perform_health_check, run_node};

pub async fn listen_for_launch_configuration(
    messenger: &MessengerHandle,
    master_node_node: &str,
    instance_id: &str,
    node_name: &str,
    node_stack: Arc<NodeStack>,
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
        names::LAUNCHER,
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
                    handle_launcher_request(
                        context,
                        node_stack,
                        messenger,
                        master_node_name,
                        master_instance_id,
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

async fn handle_launcher_request(
    context: ServiceRequestContext,
    node_stack: Arc<NodeStack>,
    messenger: MessengerHandle,
    master_node_name: String,
    master_instance_id: String,
    node_start_health_timeout: Duration,
) -> PeppyResult<Bytes> {
    let sender_instance_id = context.message().instance_id().to_string();

    let response = match handle_launcher_request_inner(
        &context,
        &node_stack,
        &messenger,
        master_node_name.as_str(),
        master_instance_id.as_str(),
        node_start_health_timeout,
    )
    .await
    {
        Ok(()) => LauncherResponse::new(),
        Err(error) => LauncherResponse::error(error),
    };

    response
        .encode()
        .map_err(|e| PeppyError::InvalidServiceRequest {
            identifier: sender_instance_id,
            reason: e.to_string(),
        })
}

async fn handle_launcher_request_inner(
    context: &ServiceRequestContext,
    node_stack: &NodeStack,
    messenger: &MessengerHandle,
    master_node_name: &str,
    master_instance_id: &str,
    node_start_health_timeout: Duration,
) -> std::result::Result<(), String> {
    let sender_instance_id = context.message().instance_id();
    let payload = context.message().payload();
    debug!("Received launcher request from {sender_instance_id}");

    let request = LauncherRequest::decode(&payload.as_bytes()).map_err(|e| e.to_string())?;

    let peppy_launcher = PeppyLauncherParser::from_content(&request.peppy_launcher_json5)
        .map_err(|e| format!("invalid peppy_launcher_json5: {e}"))?;

    if !request.nodes_directory.is_dir() {
        return Err(format!(
            "nodes_directory is not a directory: {}",
            request.nodes_directory.display()
        ));
    }

    let master_node_config = node_stack.root().config().clone();
    let nodes_directory = request.nodes_directory.clone();

    let plan = tokio::task::spawn_blocking(move || -> std::result::Result<LaunchPlan, String> {
        LaunchPlan::from_config(master_node_config, peppy_launcher, &nodes_directory, None)
            .map_err(|e| format!("failed to build launch plan: {e}"))
    })
    .await
    .map_err(|e| format!("launch plan task failed: {e}"))??;

    validate_launch_plan(&plan)?;
    start_launch_plan_instances(
        &plan,
        messenger,
        master_node_name,
        master_instance_id,
        node_start_health_timeout,
    )
    .await?;
    apply_launch_plan(node_stack, plan.node_stack())?;

    Ok(())
}

async fn start_launch_plan_instances(
    plan: &LaunchPlan,
    messenger: &MessengerHandle,
    master_node_name: &str,
    master_instance_id: &str,
    node_start_health_timeout: Duration,
) -> std::result::Result<(), String> {
    let (messaging_host, messaging_port) = messenger
        .messaging_endpoint()
        .await
        .unwrap_or_else(|| (DEFAULT_ZENOH_HOST.to_string(), DEFAULT_ZENOH_PORT));

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

        for deployment_instance in &deployment.instances {
            // Run `generate` on the node to generate the peppygen library
            let node_root_path = entity.root_path().to_path_buf();
            tokio::task::spawn_blocking(move || {
                generator::generate_lib_for_build_system(BuildSystem::Cargo, &node_root_path)
            })
            .await
            .map_err(|e| format!("generate task failed for {node_name}:{tag}: {e}"))?
            .map_err(|e| format!("failed to generate peppygen for {node_name}:{tag}: {e}"))?;

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
                node_start_health_timeout,
            )
            .await
            {
                Ok(child) => started_children.push(child),
                Err(error) => {
                    for mut child in started_children {
                        let _ = child.kill();
                        let _ = tokio::task::spawn_blocking(move || child.wait());
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
    node_start_health_timeout: Duration,
) -> std::result::Result<Child, String> {
    let manifest = entity.config().manifest.clone();

    let mut child = run_node(entity, runtime_config_json5).map_err(|e| {
        format!(
            "failed to start node {}:{} instance {}: {e}",
            manifest.name.as_str(),
            manifest.tag,
            instance_id
        )
    })?;

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
            if let Err(kill_err) = child.kill() {
                debug!(
                    "Failed to kill process for node {}:{} instance {}: {}",
                    manifest.name.as_str(),
                    manifest.tag,
                    instance_id,
                    kill_err
                );
            }

            let stderr_output = tokio::task::spawn_blocking(move || child.wait_with_output())
                .await
                .ok()
                .and_then(|result| result.ok())
                .map(|output| String::from_utf8_lossy(&output.stderr).to_string())
                .unwrap_or_default();

            let error_msg = if stderr_output.is_empty() {
                format!(
                    "failed to start node {}:{} instance {}: {error}",
                    manifest.name.as_str(),
                    manifest.tag,
                    instance_id
                )
            } else {
                format!(
                    "failed to start node {}:{} instance {}: {error}. Node stderr: {stderr_output}",
                    manifest.name.as_str(),
                    manifest.tag,
                    instance_id
                )
            };
            Err(error_msg)
        }
    }
}

fn validate_launch_plan(plan: &LaunchPlan) -> std::result::Result<(), String> {
    let report = plan.report();

    let mut errors = Vec::new();

    for deployment in report
        .deployments()
        .iter()
        .filter(|deployment| !deployment.is_resolved() && !deployment.deployment().optional)
    {
        let deployment_name = deployment.deployment().name.as_str();
        let deployment_tag = deployment.deployment().tag.as_str();
        let reason = deployment
            .error()
            .map(ToString::to_string)
            .unwrap_or_else(|| "unknown error".to_string());
        errors.push(format!(
            "deployment {deployment_name}:{deployment_tag} failed: {reason}"
        ));
    }

    for dependency_error in report.dependency_errors() {
        errors.push(dependency_error.to_string());
    }

    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("\n"))
    }
}

fn apply_launch_plan(
    target_stack: &NodeStack,
    planned_stack: &NodeStack,
) -> std::result::Result<(), String> {
    let target_root = target_stack.root();
    let target_root_name = target_root.config().manifest.name.as_str().to_owned();
    let target_root_tag = target_root.config().manifest.tag.clone();

    target_stack.reset();

    for entity in planned_stack.snapshot() {
        let config = entity.config();

        if config.manifest.name.as_str() == target_root_name.as_str()
            && config.manifest.tag == target_root_tag
        {
            continue;
        }

        // First, push the config
        target_stack
            .push_config(config.clone(), true, entity.root_path())
            .map_err(|e| {
                format!(
                    "failed to add config {}:{} to node stack: {e}",
                    config.manifest.name.as_str(),
                    config.manifest.tag,
                )
            })?;

        // Then spawn each instance
        for instance in entity.instances() {
            target_stack
                .add_instance(
                    config.manifest.name.as_str(),
                    &config.manifest.tag,
                    Some(instance.instance_id()),
                )
                .map_err(|e| {
                    format!(
                        "failed to spawn instance {} for {}:{} in node stack: {e}",
                        instance.instance_id().as_str(),
                        config.manifest.name.as_str(),
                        config.manifest.tag,
                    )
                })?;
        }
    }

    Ok(())
}
