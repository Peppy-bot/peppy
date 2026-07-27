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
        handle_node_reset_request_inner(
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

async fn handle_node_reset_request_inner(
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

    debug!("Received `node_reset` request from {sender_instance_id}");
    // Stop the running instances (cooperative shutdown, then force-kill the
    // process group of any straggler) before dropping them from the stack, so a
    // reset never orphans the previous stack's processes.
    teardown_all_instances(messenger, core_node_node, instance_id, &node_stack).await;
    // Clear both cross-instance registries the same way. `node_stack.reset()`
    // drops the pairing registry; the observation registry is a separate
    // authority, so a reset must clear it too, or a re-run of the same instance
    // ids would inherit the previous stack's observer records. The mass
    // teardown above mark_stopping's every instance, so their exit watchers bail
    // before the per-instance teardown seam runs; clearing here is the seam for
    // the whole-stack case.
    node_stack.reset();
    observation.clear();
    // An emptied stack belongs to no launch. Clearing this also releases any
    // reservation held over this machine, so a reset is a complete escape
    // hatch rather than one that leaves the daemon refusing future launches.
    ownership.clear();
    StackResetResponse::success().encode().map_err(Into::into)
}
