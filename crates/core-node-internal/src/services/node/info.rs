use crate::Result;
use crate::services::response::failure;
use config::failure_kind::PeppyFailureKind;
use core_node_api::ServiceId;
use core_node_api::encoding::{NodeInfo, NodeInfoRequest, NodeInfoResponse, NodeInstanceInfo};
use core_node_api::names;
use node_stack::NodeStack;
use peppylib::messaging::SenderTarget;
use peppylib::messaging::ServiceAnswer;
use peppylib::messaging::ServiceRequestContext;
use peppylib::{MessengerHandle, ServiceMessenger};
use std::path::{Path, PathBuf};
use std::{sync::Arc, time::Duration};
use tokio::task::JoinHandle;
use tracing::debug;

pub async fn listen_for_node_info(
    messenger: &MessengerHandle,
    core_node_name: &str,
    instance_id: &str,
    node_name: &str,
    node_stack: Arc<NodeStack>,
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
                handle_node_info_request(context, Arc::clone(&node_stack), timeout)
            })
            .await
            .map_err(Into::into)
    });

    Ok(handle)
}

/// Answers one `node_info` request within `timeout`: a request that does not
/// decode is `rejected`, an internal fault (a serializer or encoder error)
/// is `attempt_failed`, and a lookup that outlasts `timeout` is `timeout`.
async fn handle_node_info_request(
    context: ServiceRequestContext,
    node_stack: Arc<NodeStack>,
    timeout: Duration,
) -> ServiceAnswer {
    tokio::time::timeout(
        timeout,
        handle_node_info_request_inner(&context, node_stack),
    )
    .await
    .unwrap_or_else(|_| {
        Err(failure(
            PeppyFailureKind::Timeout,
            format!(
                "the node lookup took longer than {}s",
                timeout.as_secs_f64()
            ),
        ))
    })
}

async fn handle_node_info_request_inner(
    context: &ServiceRequestContext,
    node_stack: Arc<NodeStack>,
) -> ServiceAnswer {
    let sender_instance_id = context.message().instance_id();
    let payload = context.message().payload_bytes();

    let request = NodeInfoRequest::decode(payload.as_ref())
        .map_err(|e| failure(PeppyFailureKind::Rejected, e))?;

    debug!(
        "Received `node_info` request from {sender_instance_id} for {}:{}",
        request.node_name, request.node_tag
    );

    // A missing `(name, tag)` is a *successful* negative lookup, not a
    // malformed request. Encode it as `NodeInfoResponse::NotInStack` so
    // callers (e.g. the `peppy node add` preflight) can handle it without
    // provoking the generic service-handler error log. Malformed requests
    // (decode failures above) are the only caller-fault path through this
    // handler.
    let Some(entity) = node_stack.find(&request.node_name, &request.node_tag) else {
        return NodeInfoResponse::NotInStack
            .encode()
            .map_err(encode_failure);
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
                clock: instance.clock().clone(),
                slot_bindings: instance.slot_bindings().clone(),
                pairing_slots: node_stack::pairing_slot_view(
                    &core_node,
                    id,
                    pairing_deps,
                    &live_pairs,
                ),
                endpoints: instance.endpoints().to_vec(),
                restart_status: instance.restart_status(),
            });
            run_log_paths.push(
                instance
                    .run_log_path()
                    .map_or_else(PathBuf::new, Path::to_path_buf),
            );
        }
        (node_config, stage, instances, run_log_paths)
    };

    let add_log_path = node_stack.add_log_path(&request.node_name, &request.node_tag);

    let config_integrity = super::manifest_fingerprint(&node_config)
        .map_err(|reason| failure(PeppyFailureKind::AttemptFailed, reason))?;

    NodeInfoResponse::Found(Box::new(NodeInfo {
        config: node_config,
        config_integrity,
        stage,
        instances,
        add_log_path,
        run_log_paths,
        reported_at_ms: core_node_api::unix_ms(std::time::SystemTime::now()),
    }))
    .encode()
    .map_err(encode_failure)
}

fn encode_failure(error: core_node_api::Error) -> peppylib::messaging::Failure {
    failure(
        PeppyFailureKind::AttemptFailed,
        format!("failed to encode NodeInfoResponse: {error}"),
    )
}
