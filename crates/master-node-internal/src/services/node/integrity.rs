use crate::encoding::{InterfaceIntegrity, NodeIntegrityRequest, NodeIntegrityResponse};
use crate::{Result, names};
use bytes::Bytes;
use config::fingerprint::fingerprint_for_bytes;
use config::node::{InterfaceKind, NodeConfig};
use node_stack::NodeStack;
use peppylib::messaging::ServiceRequestContext;
use peppylib::{MessengerHandle, PeppyError, PeppyResult, ServiceMessenger};
use std::{sync::Arc, time::Duration};
use tokio::task::JoinHandle;
use tracing::debug;

/// This calls return the sha256 of each exposed interface,
/// allowing subscribers to validate the integrity of the interfaces they subscribe to
pub async fn listen_for_node_integrity(
    messenger: &MessengerHandle,
    master_node_name: &str,
    instance_id: &str,
    node_name: &str,
    node_stack: Arc<NodeStack>,
    timeout: Duration,
) -> Result<JoinHandle<Result<()>>> {
    let mut endpoint = ServiceMessenger::listen(
        messenger,
        master_node_name,
        instance_id,
        node_name,
        names::NODE_INTEGRITY,
    )
    .await?;

    let handle = tokio::spawn(async move {
        endpoint
            .handle_requests(|context| {
                handle_node_integrity_request(context, Arc::clone(&node_stack), timeout)
            })
            .await
            .map_err(Into::into)
    });

    Ok(handle)
}

async fn handle_node_integrity_request(
    context: ServiceRequestContext,
    node_stack: Arc<NodeStack>,
    timeout: Duration,
) -> PeppyResult<Bytes> {
    let sender_instance_id = context.message().instance_id().to_string();

    match tokio::time::timeout(
        timeout,
        handle_node_integrity_request_inner(&context, node_stack),
    )
    .await
    {
        Ok(Ok(bytes)) => Ok(bytes),
        Ok(Err(reason)) => Err(PeppyError::InvalidServiceRequest {
            identifier: sender_instance_id,
            reason,
        }),
        Err(_) => Err(PeppyError::ServiceTimeout {
            instance_id: None,
            service_name: names::NODE_INTEGRITY.to_string(),
        }),
    }
}

async fn handle_node_integrity_request_inner(
    context: &ServiceRequestContext,
    _node_stack: Arc<NodeStack>,
) -> std::result::Result<Bytes, String> {
    let sender_instance_id = context.message().instance_id();
    let payload = context.message().payload();

    let request =
        NodeIntegrityRequest::decode(&payload.as_bytes()).map_err(|e| format!("{}", e))?;

    debug!("Received `node_integrity` request from {sender_instance_id}");

    let node_config = super::resolve_node_config(request.source).await?;

    let interfaces_integrity = compute_interfaces_integrity(&node_config)?;

    let config_json = serde_json5::to_string(&node_config).map_err(|e| format!("{}", e))?;
    let config_integrity = fingerprint_for_bytes(config_json.as_bytes());

    NodeIntegrityResponse::new(interfaces_integrity, config_integrity)
        .encode()
        .map_err(|e| format!("{}", e))
}

/// Computes SHA256 hashes for each exposed interface in a node config.
///
/// Each interface (topic, service, action) is serialized to JSON and hashed
/// individually, producing a list of [`InterfaceIntegrity`] entries that
/// subscribers can use to verify interface compatibility.
pub fn compute_interfaces_integrity(
    config: &NodeConfig,
) -> std::result::Result<Vec<InterfaceIntegrity>, String> {
    let mut result = Vec::new();

    let Some(exposes) = &config.interfaces.exposes else {
        return Ok(result);
    };

    for topic in exposes.topics.iter().flatten() {
        let json = serde_json::to_string(topic).map_err(|e| format!("{}", e))?;
        result.push(InterfaceIntegrity {
            name: topic.name.clone(),
            sha256: fingerprint_for_bytes(json.as_bytes()),
            interface_kind: InterfaceKind::Topic,
        });
    }

    for service in exposes.services.iter().flatten() {
        let json = serde_json::to_string(service).map_err(|e| format!("{}", e))?;
        result.push(InterfaceIntegrity {
            name: service.name.clone(),
            sha256: fingerprint_for_bytes(json.as_bytes()),
            interface_kind: InterfaceKind::Service,
        });
    }

    for action in exposes.actions.iter().flatten() {
        let json = serde_json::to_string(action).map_err(|e| format!("{}", e))?;
        result.push(InterfaceIntegrity {
            name: action.name.clone(),
            sha256: fingerprint_for_bytes(json.as_bytes()),
            interface_kind: InterfaceKind::Action,
        });
    }

    Ok(result)
}
