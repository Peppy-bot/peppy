use std::sync::Arc;

use bytes::Bytes;
use config::peppy_config::PeppyLauncherParser;
use node_stack::{LaunchPlan, NodeStack};
use peppylib::messaging::ServiceRequestContext;
use peppylib::{MessengerHandle, PeppyError, PeppyResult, ServiceMessenger};
use tokio::task::JoinHandle;
use tracing::debug;

use crate::Result;
use crate::encoding::{LauncherRequest, LauncherResponse};

use super::names;

pub async fn listen_for_launch_configuration(
    messenger: &MessengerHandle,
    master_node_node: &str,
    instance_id: &str,
    node_name: &str,
    node_stack: Arc<NodeStack>,
) -> Result<JoinHandle<Result<()>>> {
    let mut endpoint = ServiceMessenger::listen(
        messenger,
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
                async move { handle_launcher_request(context, node_stack).await }
            })
            .await
            .map_err(Into::into)
    });

    Ok(handle)
}

async fn handle_launcher_request(
    context: ServiceRequestContext,
    node_stack: Arc<NodeStack>,
) -> PeppyResult<Bytes> {
    let sender_instance_id = context.message().instance_id().to_string();

    let response = match handle_launcher_request_inner(&context, &node_stack).await {
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
    apply_launch_plan(node_stack, plan.node_stack())?;

    Ok(())
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
                .spawn_instance(
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
