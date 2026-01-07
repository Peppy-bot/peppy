use crate::Result;
use crate::encoding::{NodeGenerateRequest, NodeGenerateResponse};
use crate::names;
use bytes::Bytes;
use peppylib::messaging::ServiceRequestContext;
use peppylib::{MessengerHandle, PeppyError, PeppyResult, ServiceMessenger};
use tokio::task::JoinHandle;
use tracing::debug;

pub async fn listen_for_node_generate(
    messenger: &MessengerHandle,
    master_node_node: &str,
    instance_id: &str,
    node_name: &str,
) -> Result<JoinHandle<Result<()>>> {
    let mut endpoint = ServiceMessenger::listen(
        messenger,
        master_node_node,
        instance_id,
        node_name,
        names::NODE_GENERATE,
    )
    .await?;

    let handle = tokio::spawn(async move {
        endpoint
            .handle_requests(handle_node_generate_request)
            .await
            .map_err(Into::into)
    });

    Ok(handle)
}

async fn handle_node_generate_request(context: ServiceRequestContext) -> PeppyResult<Bytes> {
    let sender_instance_id = context.message().instance_id();
    handle_node_generate_request_inner(&context)
        .await
        .map_err(|e| PeppyError::InvalidServiceRequest {
            identifier: sender_instance_id.to_string(),
            reason: e.to_string(),
        })
}

async fn handle_node_generate_request_inner(context: &ServiceRequestContext) -> Result<Bytes> {
    let sender_instance_id = context.message().instance_id();
    let payload = context.message().payload();

    let request = NodeGenerateRequest::decode(&payload.as_bytes())?;

    debug!("Received `node_generate` request from {sender_instance_id}");

    if request.node_root_dir.as_os_str().is_empty() {
        return NodeGenerateResponse::failure("Missing `node_root_dir` in node_generate request")
            .encode();
    }

    if !request.node_root_dir.exists() {
        return NodeGenerateResponse::failure(format!(
            "`node_root_dir` does not exist: {}",
            request.node_root_dir.display()
        ))
        .encode();
    }

    if !request.node_root_dir.is_dir() {
        return NodeGenerateResponse::failure(format!(
            "`node_root_dir` is not a directory: {}",
            request.node_root_dir.display()
        ))
        .encode();
    }

    let build_system = request.build_system;
    let node_root_dir = request.node_root_dir;
    match tokio::task::spawn_blocking(move || {
        generator::generate_lib_for_build_system(build_system, &node_root_dir)
    })
    .await
    {
        Ok(Ok(())) => {}
        Ok(Err(e)) => {
            return NodeGenerateResponse::failure(format!("Failed to generate peppygen: {}", e))
                .encode();
        }
        Err(e) => {
            return NodeGenerateResponse::failure(format!(
                "Failed to generate peppygen (generate task failed): {}",
                e
            ))
            .encode();
        }
    };

    NodeGenerateResponse::success().encode()
}
