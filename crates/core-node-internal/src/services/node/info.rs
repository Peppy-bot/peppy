use crate::Result;
use core_node_api::ServiceId;
use core_node_api::encoding::{NodeInfo, NodeInfoRequest, NodeInfoResponse, NodeInstanceInfo};
use core_node_api::names;
use daemon_config::consts::PeppyDirs;
use node_stack::NodeStack;
use peppylib::messaging::SenderTarget;
use peppylib::messaging::ServiceRequestContext;
use peppylib::types::Payload;
use peppylib::{MessengerHandle, PeppyError, PeppyResult, ServiceMessenger};
use std::path::PathBuf;
use std::{sync::Arc, time::Duration};
use tokio::task::JoinHandle;
use tracing::debug;

pub async fn listen_for_node_info(
    messenger: &MessengerHandle,
    core_node_name: &str,
    instance_id: &str,
    node_name: &str,
    node_stack: Arc<NodeStack>,
    peppy_dirs: PeppyDirs,
    timeout: Duration,
) -> Result<JoinHandle<Result<()>>> {
    let mut endpoint = ServiceMessenger::listen(
        messenger,
        core_node_name,
        instance_id,
        SenderTarget::node(node_name, names::CORE_NODE_TAG)?,
        ServiceId::NodeInfo.name(),
    )
    .await?;

    let handle = tokio::spawn(async move {
        endpoint
            .handle_requests(|context| {
                handle_node_info_request(
                    context,
                    Arc::clone(&node_stack),
                    peppy_dirs.clone(),
                    timeout,
                )
            })
            .await
            .map_err(Into::into)
    });

    Ok(handle)
}

/// Failure mode of `handle_node_info_request_inner`. Routed to a different
/// `PeppyError` variant by the outer wrapper so that internal faults
/// (serializer/encoder errors) are not classified as caller-fault
/// `InvalidServiceRequest`.
enum InfoError {
    Invalid(String),
    Internal(String),
}

// Only `String` is convertible into `InfoError` via `?`, and only as the
// `Invalid` (caller-fault) variant. The previous blanket
// `From<E: Display>` swept *every* error type into `Invalid`, which
// silently routed things like serializer faults to `InvalidServiceRequest`
// instead of `ServiceError`. With this restricted impl, internal-fault
// sites must call `InfoError::Internal(...)` explicitly.
impl From<String> for InfoError {
    fn from(reason: String) -> Self {
        InfoError::Invalid(reason)
    }
}

async fn handle_node_info_request(
    context: ServiceRequestContext,
    node_stack: Arc<NodeStack>,
    peppy_dirs: PeppyDirs,
    timeout: Duration,
) -> PeppyResult<Payload> {
    let sender_instance_id = context.message().instance_id().to_string();

    match tokio::time::timeout(
        timeout,
        handle_node_info_request_inner(&context, node_stack, peppy_dirs),
    )
    .await
    {
        Ok(Ok(bytes)) => Ok(bytes),
        Ok(Err(InfoError::Invalid(reason))) => Err(PeppyError::InvalidServiceRequest {
            identifier: sender_instance_id,
            reason,
        }),
        Ok(Err(InfoError::Internal(reason))) => Err(PeppyError::ServiceError {
            instance_id: Some(sender_instance_id),
            service_name: ServiceId::NodeInfo.name().to_string(),
            reason,
        }),
        Err(_) => Err(PeppyError::ServiceTimeout {
            instance_id: None,
            service_name: ServiceId::NodeInfo.name().to_string(),
        }),
    }
}

async fn handle_node_info_request_inner(
    context: &ServiceRequestContext,
    node_stack: Arc<NodeStack>,
    peppy_dirs: PeppyDirs,
) -> std::result::Result<Payload, InfoError> {
    let sender_instance_id = context.message().instance_id();
    let payload = context.message().payload_bytes();

    let request = NodeInfoRequest::decode(payload.as_ref()).map_err(|e| format!("{}", e))?;

    debug!(
        "Received `node_info` request from {sender_instance_id} for {}:{}",
        request.node_name, request.node_tag
    );

    // A missing `(name, tag)` is a *successful* negative lookup, not a
    // malformed request. Encode it as `NodeInfoResponse::NotInStack` so
    // callers (e.g. the `peppy node add` preflight) can handle it without
    // provoking the generic service-handler error log. Malformed requests
    // (decode failures above) still route to `InfoError::Invalid` and are
    // the only legitimate caller-fault path through this handler.
    let Some(entity) = node_stack.find(&request.node_name, &request.node_tag) else {
        return NodeInfoResponse::NotInStack
            .encode()
            .map_err(|e| InfoError::Internal(format!("failed to encode NodeInfoResponse: {}", e)));
    };

    // Live pairs, read before the entity lock (`live_pairs` filters dead
    // endpoints without taking the stack write lock).
    let live_pairs = node_stack.live_pairs();
    let core_node = node_stack
        .root()
        .read()
        .config()
        .manifest
        .name
        .as_str()
        .to_owned();

    let (node_config, stage, instances, run_log_paths) = {
        let guard = entity.read();
        let stage = guard.stage().to_serialized();
        let node_config = guard.config().clone();
        let tracked = guard.instances();
        let run_log_dir = peppy_dirs.logs_dir_run();
        let pairing_deps = node_config
            .manifest
            .depends_on
            .as_ref()
            .map(|d| d.pairings.as_slice())
            .unwrap_or(&[]);
        let mut instances: Vec<NodeInstanceInfo> = Vec::with_capacity(tracked.len());
        let mut run_log_paths: Vec<PathBuf> = Vec::with_capacity(tracked.len());
        for instance in tracked.iter() {
            let id = instance.instance_id().as_str();
            instances.push(NodeInstanceInfo {
                instance_id: id.to_owned(),
                state: instance.state(),
                healthy: instance.healthy(),
                slot_bindings: instance.slot_bindings().clone(),
                pairing_slots: node_stack::pairing_slot_view(
                    &core_node,
                    id,
                    pairing_deps,
                    &live_pairs,
                ),
            });
            run_log_paths.push(run_log_dir.join(format!("{}.log", id)));
        }
        (node_config, stage, instances, run_log_paths)
    };

    let add_log_path = node_stack.add_log_path(&request.node_name, &request.node_tag);

    let config_integrity =
        super::manifest_fingerprint(&node_config).map_err(InfoError::Internal)?;

    NodeInfoResponse::Found(Box::new(NodeInfo {
        config: node_config,
        config_integrity,
        stage,
        instances,
        add_log_path,
        run_log_paths,
    }))
    .encode()
    .map_err(|e| InfoError::Internal(format!("failed to encode NodeInfoResponse: {}", e)))
}
