use crate::Result;
use crate::services::response::into_service_response;
use config::runtime::Name;
use core_node_api::ServiceId;
use core_node_api::encoding::{NodeRemoveRequest, NodeRemoveResponse};
use core_node_api::names;
use node_stack::NodeStack;
use peppylib::messaging::SenderTarget;
use peppylib::messaging::{
    SHUTDOWN_SERVICE, ServiceMessenger, ServiceRequestContext, ServiceTarget,
};
use peppylib::types::Payload;
use peppylib::{MessengerHandle, PeppyError, PeppyResult};
use std::sync::Arc;
use std::time::Duration;
use tokio::task::JoinHandle;
use tracing::{debug, warn};

use super::RelationshipCoordinators;

const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);

pub async fn listen_for_node_remove(
    messenger: &MessengerHandle,
    core_node_node: &str,
    instance_id: &str,
    node_name: &str,
    node_stack: Arc<NodeStack>,
    relationships: RelationshipCoordinators,
) -> Result<JoinHandle<Result<()>>> {
    let core_node_node = core_node_node.to_string();
    let core_instance_id = instance_id.to_string();
    let messenger = messenger.clone();

    let mut endpoint = ServiceMessenger::listen(
        &messenger,
        &core_node_node,
        &core_instance_id,
        SenderTarget::node(node_name, names::CORE_NODE_TAG)?,
        ServiceId::NodeRemove.name(),
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
                    relationships.clone(),
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
    relationships: RelationshipCoordinators,
) -> PeppyResult<Payload> {
    into_service_response(
        &context,
        handle_node_remove_request_inner(
            &context,
            &messenger,
            &core_node_node,
            &core_instance_id,
            node_stack,
            &relationships,
        )
        .await,
    )
}

async fn handle_node_remove_request_inner(
    context: &ServiceRequestContext,
    messenger: &MessengerHandle,
    core_node_node: &str,
    core_instance_id: &str,
    node_stack: Arc<NodeStack>,
    relationships: &RelationshipCoordinators,
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
        let guard = root_handle.read();
        (
            guard.config().manifest.name.as_str().to_owned(),
            guard.config().manifest.tag.clone(),
        )
    };
    if request.node_name == root_node_name && request.tag == root_node_tag {
        return NodeRemoveResponse::failure("Cannot remove the core node from the node stack")
            .encode()
            .map_err(Into::into);
    }

    let matching_entity = node_stack.snapshot().into_iter().find(|handle| {
        let guard = handle.read();
        guard.config().manifest.name.as_str() == request.node_name
            && guard.config().manifest.tag == request.tag
    });
    let Some(matching_entity) = matching_entity else {
        return NodeRemoveResponse::failure(format!(
            "Node '{}:{}' not found in node stack",
            request.node_name, request.tag
        ))
        .encode()
        .map_err(Into::into);
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

    // `targets` are live (`Running`) instances that get a reachability probe and
    // a cooperative shutdown before removal. `terminal_targets` are instances
    // that already exited on their own (`Finished`/`Failed`): there is no process
    // to probe or signal, but they are still tracked, so they must be counted by
    // the safety gate and cleared from the entity before `remove_config`.
    let mut targets: Vec<RemovalTarget> = Vec::new();
    let mut terminal_targets: Vec<RemovalTarget> = Vec::new();
    let mut config_targets: Vec<ConfigRemovalTarget> = Vec::new();
    for handle in matching_entities {
        let guard = handle.read();
        let node_tag = guard.config().manifest.tag.clone();
        let node_name = guard.config().manifest.name.as_str().to_owned();
        config_targets.push(ConfigRemovalTarget {
            node_name: node_name.clone(),
            node_tag: node_tag.clone(),
        });
        for instance in guard.instances() {
            // Skip Starting instances: they will resolve via the
            // prepare_and_spawn → abort_started path; calling stop_instance on
            // them is a no-op at best and racy at worst.
            if instance.state() == node_stack::InstanceState::Starting {
                continue;
            }
            let target = RemovalTarget {
                node_name: node_name.clone(),
                node_tag: node_tag.clone(),
                instance_id: instance.instance_id().clone(),
            };
            if instance.state().is_terminal() {
                terminal_targets.push(target);
            } else {
                targets.push(target);
            }
        }
    }

    // Classify each tracked instance as reachable, unreachable, or probe-failed.
    // `running_targets` drives the actual shutdown loop below (only reachable
    // instances can accept the shutdown RPC). `unreachable_targets` is tracked
    // separately so the safety gate can decide what to do about them without
    // silently dropping their existence.
    // Probe all instances in parallel so overall latency is bounded by the
    // slowest probe rather than the sum.
    let reachability: Vec<std::result::Result<bool, PeppyError>> =
        futures::future::join_all(targets.iter().map(|target| {
            let producer =
                peppylib::messaging::ProducerRef::new(core_node_node, target.instance_id.as_str());
            async move {
                ServiceMessenger::is_reachable(
                    messenger,
                    core_node_node,
                    core_instance_id,
                    SenderTarget::node_from_validated(&target.node_name, &target.node_tag),
                    SHUTDOWN_SERVICE,
                    ServiceTarget::Producer(&producer),
                )
                .await
            }
        }))
        .await;

    let mut running_targets: Vec<RemovalTarget> = Vec::new();
    let mut unreachable_targets: Vec<RemovalTarget> = Vec::new();
    for (target, reachable) in targets.iter().zip(reachability) {
        let reachable = match reachable {
            Ok(r) => r,
            Err(e) => {
                return NodeRemoveResponse::failure(format!(
                    "Failed to check shutdown service for instance '{}': {}",
                    target.instance_id.as_str(),
                    e
                ))
                .encode()
                .map_err(Into::into);
            }
        };
        if reachable {
            running_targets.push(target.clone());
        } else {
            unreachable_targets.push(target.clone());
        }
    }

    // Safety gate: without `stop_instances`, any tracked instance (reachable,
    // unreachable, or terminal) blocks the remove. Reachable/unreachable could
    // still be backed by a live process; terminal instances have exited but are
    // still tracked, and `remove_config` rejects a node that still has any
    // instance, so they must be cleared first too.
    if !request.stop_instances
        && (!running_targets.is_empty()
            || !unreachable_targets.is_empty()
            || !terminal_targets.is_empty())
    {
        let example = running_targets
            .first()
            .or_else(|| unreachable_targets.first())
            .or_else(|| terminal_targets.first())
            .expect("one of the lists is non-empty");
        return NodeRemoveResponse::failure(format!(
            "Node '{}' has tracked instances (e.g. '{}'); set stop_instances=true to clear them before removing",
            request.node_name,
            example.instance_id.as_str(),
        ))
        .encode().map_err(Into::into);
    }

    // With `stop_instances=true`, we proceed despite unreachable instances
    // (typically they correspond to a peer that already exited), but log a
    // warning per skipped instance so the divergence is never silent.
    if request.stop_instances {
        for target in &unreachable_targets {
            warn!(
                "Instance '{}' of node '{}:{}' is tracked but its shutdown service is unreachable; \
                 removing without sending shutdown",
                target.instance_id.as_str(),
                target.node_name,
                target.node_tag,
            );
        }
    }

    if request.stop_instances {
        for target in &running_targets {
            debug!(
                "Stopping node instance '{}' before removal",
                target.instance_id.as_str()
            );
        }

        // Shut down instances concurrently. Overall wall-clock latency is
        // bounded by the slowest shutdown (up to SHUTDOWN_TIMEOUT) rather
        // than the sum.
        let shutdown_results = futures::future::join_all(running_targets.iter().map(|target| {
            let producer =
                peppylib::messaging::ProducerRef::new(core_node_node, target.instance_id.as_str());
            async move {
                ServiceMessenger::poll(
                    messenger,
                    core_node_node,
                    core_instance_id,
                    SenderTarget::node_from_validated(&target.node_name, &target.node_tag),
                    SHUTDOWN_SERVICE,
                    ServiceTarget::Producer(&producer),
                    Payload::from_static(b"shutdown"),
                    SHUTDOWN_TIMEOUT,
                )
                .await
            }
        }))
        .await;
        for (target, res) in running_targets.iter().zip(shutdown_results) {
            if let Err(e) = res {
                return NodeRemoveResponse::failure(format!(
                    "Failed to stop node instance '{}': {}",
                    target.instance_id.as_str(),
                    e
                ))
                .encode()
                .map_err(Into::into);
            }
        }
    }

    // Clear every tracked instance from its entity before `remove_config`:
    // the live ones we just cooperatively stopped, plus any terminal ones that
    // had already exited. `stop_instance` removes a `Running` or terminal
    // instance (only `Starting` is excluded), so both kinds are handled here.
    for target in targets.iter().chain(terminal_targets.iter()) {
        let Some(handle) = node_stack.find(&target.node_name, &target.node_tag) else {
            // Entity was concurrently removed; nothing to stop. Treat as
            // success rather than failing the whole removal request.
            debug!(
                "Node '{}:{}' already absent from node stack; skipping instance stop",
                target.node_name, target.node_tag
            );
            continue;
        };
        let removed = handle.write().stop_instance(&target.instance_id);
        if !removed {
            // Instance was concurrently removed; treat as success.
            debug!(
                "Node instance '{}' already absent; skipping",
                target.instance_id.as_str()
            );
        }
    }

    // Run the single teardown seam for every removed instance, exactly as the
    // stop path does: dissolve its pairs and live-notify each surviving peer
    // its slot is now Unpaired, and drop its observations while telling any
    // live observer of a removed source that the source went down. Both halves
    // are idempotent, so re-tearing-down a terminal instance the exit watcher
    // already cleared is a harmless no-op. Keyed on instance_id and independent
    // of entity presence, so it runs even when the entity was concurrently
    // removed above.
    for target in targets.iter().chain(terminal_targets.iter()) {
        relationships
            .tear_down_instance(target.instance_id.as_str())
            .await;
    }

    for target in &config_targets {
        match node_stack.remove_config(&target.node_name, &target.node_tag) {
            Ok(true) => {}
            Ok(false) => {
                // Concurrently removed; treat as success.
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
                .encode()
                .map_err(Into::into);
            }
        }
    }

    NodeRemoveResponse::success().encode().map_err(Into::into)
}
