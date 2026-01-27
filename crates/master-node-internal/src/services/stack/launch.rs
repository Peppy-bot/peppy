use crate::Result;
use crate::encoding::{LaunchRequest, LaunchResponse};
use crate::names;
use bytes::Bytes;
use node_stack::NodeStack;
use peppylib::MessengerHandle;
use peppylib::messaging::ServiceRequestContext;
use peppylib::{PeppyError, PeppyResult, ServiceMessenger};
use std::sync::Arc;
use std::time::Duration;
use tokio::task::JoinHandle;
use tracing::debug;

pub async fn listen_for_stack_launch(
    messenger: &MessengerHandle,
    master_node_node: &str,
    instance_id: &str,
    node_name: &str,
    _node_stack: Arc<NodeStack>,
    _node_startup_timeout: Duration,
    _node_start_health_timeout: Duration,
) -> Result<JoinHandle<Result<()>>> {
    let mut endpoint = ServiceMessenger::listen(
        messenger,
        master_node_node,
        instance_id,
        node_name,
        names::STACK_LAUNCH,
    )
    .await?;

    let handle = tokio::spawn(async move {
        endpoint
            .handle_requests(handle_stack_launch_request)
            .await
            .map_err(Into::into)
    });

    Ok(handle)
}

async fn handle_stack_launch_request(context: ServiceRequestContext) -> PeppyResult<Bytes> {
    let sender_instance_id = context.message().instance_id();
    handle_stack_launch_request_inner(&context).map_err(|e| PeppyError::InvalidServiceRequest {
        identifier: sender_instance_id.to_string(),
        reason: e.to_string(),
    })
}

fn handle_stack_launch_request_inner(context: &ServiceRequestContext) -> Result<Bytes> {
    let sender_instance_id = context.message().instance_id();
    let payload = context.message().payload();

    let request = LaunchRequest::decode(&payload.as_bytes())?;

    debug!("Received `stack_launch` request from {sender_instance_id}");

    // Step 1: request.launch_runtime_config_json5 should turn into a PeppyLauncher object
    // Step 2: Clear up the node stack, all nodes and instances in the node stack should be removed
    // Step 3: Call the code in `crates/master-node-internal/src/services/node/info.rs` to retrieve the info of every node in the `deployments` (reuse the code, don't duplicate)
    // Step 4: Solve the dependencies between the nodes, if they match, continue, if not, raise an error
    // Step 5: Add every node to the node stack using functions from `crates/master-node-internal/src/services/node/add.rs` (reuse the code, don't duplicate)
    // Step 6: Start the instance of all the nodes using functions from `crates/master-node-internal/src/services/node/start.rs` (reuse the code, don't duplicate). The list of instances and their instance-id can be obtained from the PeppyLauncher::deployments::instances
    // Step 7: Done, return a success to the user
}
