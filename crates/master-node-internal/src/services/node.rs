use std::sync::Arc;

use bytes::Bytes;
use node_stack::NodeStack;
use peppylib::messaging::ServiceRequestContext;
use peppylib::{MessengerHandle, PeppyError, PeppyResult, ServiceMessenger};
use tokio::task::JoinHandle;
use tracing::debug;

use crate::Result;
use crate::encoding::{NodeCmd, NodeRequest, NodeResponse};

pub async fn listen_for_node_cmds(
    messenger: &MessengerHandle,
    master_node_node: &str,
    instance_id: &str,
    node_name: &str,
    node_stack: Arc<NodeStack>,
) -> Result<JoinHandle<Result<()>>> {
    let service_name = "node";
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
            .handle_requests(|context| handle_listen_for_node_cmd(context, node_stack.clone()))
            .await
            .map_err(Into::into)
    });

    Ok(handle)
}

async fn handle_listen_for_node_cmd(
    context: ServiceRequestContext,
    node_stack: Arc<NodeStack>,
) -> PeppyResult<Bytes> {
    let sender_instance_id = context.message().instance_id();
    handle_listen_for_node_cmd_inner(&context, node_stack).map_err(|e| {
        PeppyError::InvalidServiceRequest {
            identifier: sender_instance_id.to_string(),
            reason: e.to_string(),
        }
    })
}

fn handle_listen_for_node_cmd_inner(
    context: &ServiceRequestContext,
    node_stack: Arc<NodeStack>,
) -> Result<Bytes> {
    let sender_instance_id = context.message().instance_id();
    let payload = context.message().payload();

    let request = NodeRequest::decode(&payload.as_bytes())?;

    debug!(
        "Received `node` request from {sender_instance_id}, node_cmd={:?}",
        request.cmd
    );

    let value = match request.cmd {
        NodeCmd::Add => String::new(),
        NodeCmd::List => node_stack.to_dot(),
        NodeCmd::Synchronize => String::new(),
    };

    NodeResponse::new(request.cmd, value).encode()
}
