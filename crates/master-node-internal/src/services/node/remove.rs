use crate::Result;
use crate::encoding::{NodeRemoveRequest, NodeRemoveResponse};
use bytes::Bytes;
use config::node::Name;
use node_stack::NodeStack;
use peppylib::messaging::{SHUTDOWN_SERVICE, ServiceMessenger, ServiceRequestContext};
use peppylib::{MessengerHandle, PeppyError, PeppyResult};
use std::sync::Arc;
use std::time::Duration;
use tokio::task::JoinHandle;
use tracing::debug;

use crate::services::names;

const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);

pub async fn listen_for_node_remove(
    messenger: &MessengerHandle,
    master_node_node: &str,
    instance_id: &str,
    node_name: &str,
    node_stack: Arc<NodeStack>,
) -> Result<JoinHandle<Result<()>>> {
    let master_node_node = master_node_node.to_string();
    let master_instance_id = instance_id.to_string();
    let messenger = messenger.clone();

    let mut endpoint = ServiceMessenger::listen(
        &messenger,
        &master_node_node,
        &master_instance_id,
        node_name,
        names::NODE_REMOVE,
    )
    .await?;

    let handle = tokio::spawn(async move {
        endpoint
            .handle_requests(|context| {
                handle_node_remove_request(
                    context,
                    messenger.clone(),
                    master_node_node.clone(),
                    master_instance_id.clone(),
                    node_stack.clone(),
                )
            })
            .await
            .map_err(Into::into)
    });

    Ok(handle)
}

async fn handle_node_remove_request(
    context: ServiceRequestContext,
    messenger: MessengerHandle,
    master_node_node: String,
    master_instance_id: String,
    node_stack: Arc<NodeStack>,
) -> PeppyResult<Bytes> {
    let sender_instance_id = context.message().instance_id();
    handle_node_remove_request_inner(
        &context,
        &messenger,
        &master_node_node,
        &master_instance_id,
        node_stack,
    )
    .await
    .map_err(|e| PeppyError::InvalidServiceRequest {
        identifier: sender_instance_id.to_string(),
        reason: e.to_string(),
    })
}

async fn handle_node_remove_request_inner(
    context: &ServiceRequestContext,
    messenger: &MessengerHandle,
    master_node_node: &str,
    master_instance_id: &str,
    node_stack: Arc<NodeStack>,
) -> Result<Bytes> {
    let sender_instance_id = context.message().instance_id();
    let payload = context.message().payload();

    let request = NodeRemoveRequest::decode(&payload.as_bytes())?;

    debug!(
        "Received `node_remove` request from {sender_instance_id}, node_name={}, stop_instances={}",
        request.node_name, request.stop_instances
    );

    let root_node_name = node_stack.root().config().manifest.name.as_str().to_owned();
    if request.node_name == root_node_name {
        return NodeRemoveResponse::failure("Cannot remove the master node from the node stack")
            .encode();
    }

    let matching_entities: Vec<_> = node_stack
        .snapshot()
        .into_iter()
        .filter(|entity| entity.config().manifest.name.as_str() == request.node_name)
        .collect();

    if matching_entities.is_empty() {
        return NodeRemoveResponse::failure(format!(
            "Node '{}' not found in node stack",
            request.node_name
        ))
        .encode();
    }

    #[derive(Debug, Clone)]
    struct RemovalTarget {
        node_name: String,
        node_tag: String,
        instance_id: Name,
    }

    #[derive(Debug, Clone)]
    struct ConfigRemovalTarget {
        node_name: String,
        node_tag: String,
    }

    let mut targets: Vec<RemovalTarget> = Vec::new();
    let mut config_targets: Vec<ConfigRemovalTarget> = Vec::new();
    for entity in matching_entities {
        let node_tag = entity.config().manifest.tag.clone();
        let node_name = entity.config().manifest.name.as_str().to_owned();
        if entity.instances().is_empty() {
            config_targets.push(ConfigRemovalTarget {
                node_name,
                node_tag,
            });
            continue;
        }
        for instance in entity.instances() {
            targets.push(RemovalTarget {
                node_name: node_name.clone(),
                node_tag: node_tag.clone(),
                instance_id: instance.instance_id().clone(),
            });
        }
    }

    let mut running_targets: Vec<RemovalTarget> = Vec::new();
    for target in &targets {
        let reachable = match ServiceMessenger::is_reachable(
            messenger,
            master_node_node,
            master_instance_id,
            &target.node_name,
            SHUTDOWN_SERVICE,
            Some(master_node_node),
            Some(target.instance_id.as_str()),
        )
        .await
        {
            Ok(reachable) => reachable,
            Err(e) => {
                return NodeRemoveResponse::failure(format!(
                    "Failed to check shutdown service for instance '{}': {}",
                    target.instance_id.as_str(),
                    e
                ))
                .encode();
            }
        };

        if reachable {
            running_targets.push(target.clone());
        }
    }

    if !request.stop_instances && !running_targets.is_empty() {
        return NodeRemoveResponse::failure(format!(
            "Node '{}' has running instances (e.g. '{}'); set stop_instances=true to stop them before removing",
            request.node_name,
            running_targets[0].instance_id.as_str(),
        ))
        .encode();
    }

    if request.stop_instances {
        for target in &running_targets {
            debug!(
                "Stopping node instance '{}' before removal",
                target.instance_id.as_str()
            );

            let shutdown_result = ServiceMessenger::poll(
                messenger,
                master_node_node,
                master_instance_id,
                &target.node_name,
                SHUTDOWN_SERVICE,
                Some(master_node_node),
                Some(target.instance_id.as_str()),
                Bytes::from_static(b"shutdown"),
                SHUTDOWN_TIMEOUT,
            )
            .await;

            if let Err(e) = shutdown_result {
                return NodeRemoveResponse::failure(format!(
                    "Failed to stop node instance '{}': {}",
                    target.instance_id.as_str(),
                    e
                ))
                .encode();
            }
        }
    }

    for target in &targets {
        match node_stack.remove_instance(&target.node_name, &target.node_tag, &target.instance_id) {
            Ok(true) => {}
            Ok(false) => {
                return NodeRemoveResponse::failure(format!(
                    "Node instance '{}' not found in node stack",
                    target.instance_id.as_str()
                ))
                .encode();
            }
            Err(e) => {
                return NodeRemoveResponse::failure(format!(
                    "Failed to remove node instance '{}': {}",
                    target.instance_id.as_str(),
                    e
                ))
                .encode();
            }
        }
    }

    for target in &config_targets {
        match node_stack.remove_config(&target.node_name, &target.node_tag) {
            Ok(true) => {}
            Ok(false) => {
                return NodeRemoveResponse::failure(format!(
                    "Node '{}:{}' not found in node stack",
                    target.node_name, target.node_tag
                ))
                .encode();
            }
            Err(e) => {
                return NodeRemoveResponse::failure(format!(
                    "Failed to remove node config '{}:{}': {}",
                    target.node_name, target.node_tag, e
                ))
                .encode();
            }
        }
    }

    NodeRemoveResponse::success().encode()
}
