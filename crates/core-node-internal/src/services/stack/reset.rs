use crate::Result;
use crate::services::node::{ObservationCoordinator, teardown_all_instances};
use crate::services::response::into_service_response;
use core_node_api::ServiceId;
use core_node_api::encoding::{StackResetRequest, StackResetResponse};
use core_node_api::names;
use node_stack::NodeStack;
use peppylib::messaging::SenderTarget;
use peppylib::messaging::ServiceRequestContext;
use peppylib::types::Payload;
use peppylib::{MessengerHandle, PeppyResult, ServiceMessenger};
use std::sync::Arc;
use tokio::task::JoinHandle;
use tracing::debug;

/// Empties this daemon's stack slice: stops every running instance, drops them,
/// and clears both cross-instance registries.
///
/// Shared by the three paths that replace a slice (`stack reset`, the launch's
/// own teardown, and a federated `participant_slice_begin`) because they must
/// agree exactly. `node_stack.reset()` drops the pairing registry, but the
/// observation registry is a separate authority: a path that forgot it would
/// let a re-run of the same instance ids inherit the previous stack's observer
/// records.
///
/// Deliberately says nothing about launch ownership. Emptying a slice and
/// deciding which launch the next one belongs to are different questions, and
/// the three callers answer the second one differently.
pub(crate) async fn clear_stack_slice(
    messenger: &MessengerHandle,
    core_node_name: &str,
    instance_id: &str,
    node_stack: &Arc<NodeStack>,
    observation: &ObservationCoordinator,
) {
    // Stop the running instances (cooperative shutdown, then force-kill the
    // process group of any straggler) before dropping them from the stack, so a
    // reset never orphans the previous stack's processes. The mass teardown
    // mark_stopping's every instance, so their exit watchers bail before the
    // per-instance teardown seam runs; clearing below is the seam for the
    // whole-stack case.
    teardown_all_instances(messenger, core_node_name, instance_id, node_stack).await;
    node_stack.reset();
    observation.clear();
}

pub async fn listen_for_stack_reset(
    messenger: &MessengerHandle,
    core_node_node: &str,
    instance_id: &str,
    node_name: &str,
    node_stack: Arc<NodeStack>,
    observation: Arc<ObservationCoordinator>,
    ownership: Arc<crate::services::federation::SliceOwnership>,
) -> Result<JoinHandle<Result<()>>> {
    let mut endpoint = ServiceMessenger::listen(
        messenger,
        core_node_node,
        instance_id,
        SenderTarget::node(node_name, names::CORE_NODE_TAG)?,
        ServiceId::StackReset.name(),
    )
    .await?;

    // Owned copies moved into the spawned task so the reset handler can stop the
    // running instances before clearing them. The reset request itself carries
    // no routing identity, so the daemon's own (messenger, core node, instance)
    // are what the teardown sends cooperative shutdowns from.
    let messenger = messenger.clone();
    let core_node_node = core_node_node.to_owned();
    let instance_id = instance_id.to_owned();

    let handle = tokio::spawn(async move {
        endpoint
            .handle_requests(|context| {
                handle_stack_reset_request(
                    context,
                    messenger.clone(),
                    core_node_node.clone(),
                    instance_id.clone(),
                    Arc::clone(&node_stack),
                    Arc::clone(&observation),
                    Arc::clone(&ownership),
                )
            })
            .await
            .map_err(Into::into)
    });

    Ok(handle)
}

async fn handle_stack_reset_request(
    context: ServiceRequestContext,
    messenger: MessengerHandle,
    core_node_node: String,
    instance_id: String,
    node_stack: Arc<NodeStack>,
    observation: Arc<ObservationCoordinator>,
    ownership: Arc<crate::services::federation::SliceOwnership>,
) -> PeppyResult<Payload> {
    into_service_response(
        &context,
        handle_stack_reset_request_inner(
            &context,
            &messenger,
            &core_node_node,
            &instance_id,
            node_stack,
            &observation,
            &ownership,
        )
        .await,
    )
}

async fn handle_stack_reset_request_inner(
    context: &ServiceRequestContext,
    messenger: &MessengerHandle,
    core_node_node: &str,
    instance_id: &str,
    node_stack: Arc<NodeStack>,
    observation: &ObservationCoordinator,
    ownership: &crate::services::federation::SliceOwnership,
) -> Result<Payload> {
    let sender_instance_id = context.message().instance_id();
    let payload = context.message().payload();

    let _request = StackResetRequest::decode(payload.as_ref())?;

    debug!("Received `stack_reset` request from {sender_instance_id}");
    clear_stack_slice(
        messenger,
        core_node_node,
        instance_id,
        &node_stack,
        observation,
    )
    .await;
    // An emptied stack belongs to no launch. Clearing this also releases any
    // reservation held over this machine, so a reset is a complete escape
    // hatch rather than one that leaves the daemon refusing future launches.
    ownership.clear();
    StackResetResponse::success().encode().map_err(Into::into)
}
