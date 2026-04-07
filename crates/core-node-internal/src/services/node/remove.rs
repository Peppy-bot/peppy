use crate::Result;
use crate::encoding::{NodeRemoveRequest, NodeRemoveResponse};
use crate::names;
use config::node::Name;
use node_stack::NodeStack;
use peppylib::messaging::{SHUTDOWN_SERVICE, ServiceMessenger, ServiceRequestContext};
use peppylib::types::Payload;
use peppylib::{MessengerHandle, PeppyError, PeppyResult};
use std::sync::Arc;
use std::time::Duration;
use tokio::task::JoinHandle;
use tracing::debug;

const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);

pub async fn listen_for_node_remove(
    messenger: &MessengerHandle,
    core_node_node: &str,
    instance_id: &str,
    node_name: &str,
    node_stack: Arc<NodeStack>,
) -> Result<JoinHandle<Result<()>>> {
    let core_node_node = core_node_node.to_string();
    let core_instance_id = instance_id.to_string();
    let messenger = messenger.clone();

    let mut endpoint = ServiceMessenger::listen(
        &messenger,
        &core_node_node,
        &core_instance_id,
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
                    core_node_node.clone(),
                    core_instance_id.clone(),
                    Arc::clone(&node_stack),
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
    core_node_node: String,
    core_instance_id: String,
    node_stack: Arc<NodeStack>,
) -> PeppyResult<Payload> {
    let sender_instance_id = context.message().instance_id();
    handle_node_remove_request_inner(
        &context,
        &messenger,
        &core_node_node,
        &core_instance_id,
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
    core_node_node: &str,
    core_instance_id: &str,
    node_stack: Arc<NodeStack>,
) -> Result<Payload> {
    let sender_instance_id = context.message().instance_id();
    let payload = context.message().payload();

    let request = NodeRemoveRequest::decode(payload.as_ref())?;

    debug!(
        "Received `node_remove` request from {sender_instance_id}, node_name={}, tag={}, stop_instances={}",
        request.node_name, request.tag, request.stop_instances
    );

    let root_handle = node_stack.root();
    let (root_node_name, root_node_tag) = {
        let guard = root_handle.read().expect("entity poisoned");
        (
            guard.config().manifest.name.as_str().to_owned(),
            guard.config().manifest.tag.clone(),
        )
    };
    if request.node_name == root_node_name && request.tag == root_node_tag {
        return NodeRemoveResponse::failure("Cannot remove the core node from the node stack")
            .encode();
    }

    let matching_entity = node_stack.snapshot().into_iter().find(|handle| {
        let guard = handle.read().expect("entity poisoned");
        guard.config().manifest.name.as_str() == request.node_name
            && guard.config().manifest.tag == request.tag
    });

    let Some(matching_entity) = matching_entity else {
        return NodeRemoveResponse::failure(format!(
            "Node '{}:{}' not found in node stack",
            request.node_name, request.tag
        ))
        .encode();
    };

    let matching_entities = vec![matching_entity];

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
    for handle in matching_entities {
        let guard = handle.read().expect("entity poisoned");
        let node_tag = guard.config().manifest.tag.clone();
        let node_name = guard.config().manifest.name.as_str().to_owned();
        config_targets.push(ConfigRemovalTarget {
            node_name: node_name.clone(),
            node_tag: node_tag.clone(),
        });
        for instance in guard.instances() {
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
            core_node_node,
            core_instance_id,
            &target.node_name,
            SHUTDOWN_SERVICE,
            Some(core_node_node),
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
                core_node_node,
                core_instance_id,
                &target.node_name,
                SHUTDOWN_SERVICE,
                Some(core_node_node),
                Some(target.instance_id.as_str()),
                Payload::from_static(b"shutdown"),
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
        let Some(handle) = node_stack.find(&target.node_name, &target.node_tag) else {
            // Entity was concurrently removed; nothing to stop. Treat as
            // success rather than failing the whole removal request.
            debug!(
                "Node '{}:{}' already absent from node stack; skipping instance stop",
                target.node_name, target.node_tag
            );
            continue;
        };
        let removed = handle
            .write()
            .expect("entity poisoned")
            .stop_instance(&target.instance_id);
        if !removed {
            // Instance was concurrently removed; treat as success.
            debug!(
                "Node instance '{}' already absent; skipping",
                target.instance_id.as_str()
            );
        }
    }

    for target in &config_targets {
        match node_stack.remove_config(&target.node_name, &target.node_tag) {
            Ok(true) => {}
            Ok(false) => {
                // Concurrently removed — treat as success.
                debug!(
                    "Node '{}:{}' already absent from node stack during remove_config",
                    target.node_name, target.node_tag
                );
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
