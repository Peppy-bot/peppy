use std::sync::Arc;

use bytes::Bytes;
use node_stack::NodeStack;
use peppylib::messaging::ServiceRequestContext;
use peppylib::{MessengerHandle, PeppyError, PeppyResult, ServiceMessenger};
use tokio::task::JoinHandle;
use tracing::debug;

use crate::Result;
use crate::encoding::{LauncherRequest, LauncherResponse};

pub async fn listen_for_launch_configuration(
    messenger: &MessengerHandle,
    master_node_node: &str,
    instance_id: &str,
    node_name: &str,
    _node_stack: Arc<NodeStack>,
) -> Result<JoinHandle<Result<()>>> {
    let service_name = "launch_configuration";
    let mut endpoint = ServiceMessenger::listen(
        messenger,
        master_node_node,
        instance_id,
        node_name,
        &service_name,
    )
    .await?;

    let handle = tokio::spawn(async move {
        endpoint
            .handle_requests(handle_launcher_request)
            .await
            .map_err(Into::into)
    });

    Ok(handle)
}

async fn handle_launcher_request(context: ServiceRequestContext) -> PeppyResult<Bytes> {
    let instance_id = context.message().instance_id();
    handle_launcher_request_inner(&context).map_err(|e| PeppyError::InvalidServiceRequest {
        identifier: instance_id.to_string(),
        reason: e.to_string(),
    })
}

fn handle_launcher_request_inner(context: &ServiceRequestContext) -> Result<Bytes> {
    let instance_id = context.message().instance_id();
    let payload = context.message().payload();
    debug!("Received launcher request from {instance_id}");

    let request = LauncherRequest::decode(&payload.as_bytes())?;

    // TODO: build a config::PeppyLauncher based on request.peppy_launcher_json5
    // TODO the command is supposed to find all the `peppy.json5` recursively in the `request.from_directory` folder. There is a feature like that available in the project
    // TODO: Use `LocalNodeStackBuilder::from_launch_file` to create a new node stack and use it in place of `app_context.set_node_stack`

    LauncherResponse::new().encode()
}
