#[cfg(all(test, feature = "zenoh"))]
mod tests;

mod actions;
mod services;
mod topics;

pub use actions::{ActionCreation, ActionGoalHandle, ActionMessenger};
pub use services::{ServiceEndpoint, ServiceMessenger, ServiceRequestContext, ServiceResponder};
pub use topics::{TopicMessenger, TopicPublisher};

use crate::error::{Error, Result};
use bytes::Bytes;
use config::node::QoSProfile;
use pmi::{
    Message, Messenger, MessengerAdapter, MessengerBackend, PeppyMessagingInterfaceError,
    PublisherQoS, SubscriberQoS, Subscription, TopicMessage, ZenohAdapter, ZenohNetProtocol,
};
use sha2::{Digest, Sha256};
use std::{
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};
use tokio::{
    sync::Mutex,
    time::{Duration, Instant, timeout},
};

// services
pub const NODE_HEALTH_SERVICE: &str = "node_health";
pub const NODE_READY_SERVICE: &str = "node_ready";
pub const SHUTDOWN_SERVICE: &str = "shutdown";

const INSTANCE_ID_WILDCARD: &str = "**";
/// Marker used in key expressions for broadcast requests (targeting any instance)
const BROADCAST_MARKER: &str = "_any_";

/// Prefix used for encoding service-side handler errors into response payloads.
///
/// This allows callers to get a useful error response instead of timing out, and prevents a
/// single bad request from killing the entire service listener loop.
const SERVICE_ERROR_PREFIX: &[u8] = b"\0peppy_service_error\0";

/// Sentinel payload sent by the service immediately upon receiving a request, before the handler
/// runs. The caller uses this to distinguish `ServiceTimeout` (ack received but no response)
/// from `ServiceUnreachable` (no ack at all within the timeout).
const SERVICE_ACK_PAYLOAD: &[u8] = b"\0peppy_service_ack\0";

/// Sentinel payload used by `is_reachable` to probe whether a service is listening without
/// invoking the user handler. The service auto-responds to probes inside `next_request()`.
const SERVICE_PROBE_PAYLOAD: &[u8] = b"\0peppy_service_probe\0";

/// Timeout for reachability probes sent by `is_reachable`.
const PROBE_TIMEOUT: Duration = Duration::from_millis(500);

fn is_service_ack_payload(payload: &[u8]) -> bool {
    payload == SERVICE_ACK_PAYLOAD
}

fn is_service_probe_payload(payload: &[u8]) -> bool {
    payload == SERVICE_PROBE_PAYLOAD
}

fn encode_service_error_payload(reason: &str) -> Bytes {
    let mut payload = Vec::with_capacity(SERVICE_ERROR_PREFIX.len() + reason.len());
    payload.extend_from_slice(SERVICE_ERROR_PREFIX);
    payload.extend_from_slice(reason.as_bytes());
    Bytes::from(payload)
}

fn decode_service_error_payload(payload: &[u8]) -> Option<String> {
    if !payload.starts_with(SERVICE_ERROR_PREFIX) {
        return None;
    }

    let reason_bytes = &payload[SERVICE_ERROR_PREFIX.len()..];
    match std::str::from_utf8(reason_bytes) {
        Ok(reason) => Some(reason.to_owned()),
        Err(_) => Some("service returned a non-UTF8 error payload".to_string()),
    }
}

#[derive(Clone)]
pub struct MessengerHandle {
    messenger: Arc<Mutex<Messenger>>,
}

/// Generates a unique request ID using SHA256 hash of timestamp + thread ID
/// This ensures each service call has a unique correlation ID
fn generate_request_id() -> String {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let thread_id = std::thread::current().id();

    let mut hasher = Sha256::new();
    hasher.update(timestamp.to_le_bytes());
    hasher.update(format!("{:?}", thread_id).as_bytes());

    let result = hasher.finalize();
    format!("{:x}", result)[..16].to_string() // Use first 16 hex chars for compactness
}

/// Formats an instance ID as a bound instance segment (appears right after MASTER_NODE in key expressions)
fn format_bound_instance_segment(instance_id: &str) -> Option<String> {
    (instance_id != INSTANCE_ID_WILDCARD).then(|| instance_id.to_string())
}

/// Formats an instance ID as a target instance segment (identifies a specific target/source instance)
fn format_target_instance_segment(instance_id: &str) -> Option<String> {
    (instance_id != INSTANCE_ID_WILDCARD).then(|| instance_id.to_string())
}

impl MessengerHandle {
    pub fn from_shared(messenger: Arc<Mutex<Messenger>>) -> Self {
        Self { messenger }
    }

    pub async fn messaging_port(&self) -> u16 {
        let messenger = self.messenger.lock().await;
        messenger.get_host().port()
    }

    pub async fn messaging_endpoint(&self) -> Option<(String, u16)> {
        let messenger = self.messenger.lock().await;
        match &messenger.adapter {
            #[cfg(feature = "zenoh")]
            MessengerAdapter::Zenoh(adapter) => {
                let (host, port) = adapter.client_endpoint();
                (!host.is_empty() && port != 0).then(|| (host.to_string(), port))
            }
            _ => None,
        }
    }

    pub async fn from_host_port(host: &str, port: u16) -> Result<Self> {
        let adapter = ZenohAdapter::connect_to(ZenohNetProtocol::Tcp, host, port)?;
        let messenger = Self::new_session(adapter).await?;
        Ok(Self {
            messenger: Arc::new(Mutex::new(messenger)),
        })
    }

    async fn new_session(adapter: ZenohAdapter) -> Result<Messenger> {
        let mut messenger = Messenger::new(MessengerAdapter::Zenoh(adapter));
        messenger
            .start_session()
            .await
            .map_err(Error::PeppyMessagingInterface)?;

        Ok(messenger)
    }

    #[allow(clippy::too_many_arguments)]
    async fn subscribe_to_topic(
        &self,
        as_master_node: &str,
        as_instance_id: &str,
        to_node_name: &str,
        to_topic: &str,
        to_master_node: Option<&str>,
        to_instance_id: Option<&str>,
        qos: QoSProfile,
    ) -> Result<Subscription> {
        let to_master_node = to_master_node.unwrap_or("*");
        let to_instance_id = to_instance_id.unwrap_or("*");
        let key_expr = format!(
            "{as_master_node}/{to_master_node}/{as_instance_id}/{to_instance_id}/topic/{to_node_name}/{to_topic}"
        );
        let subscription = {
            let messenger = self.messenger.lock().await;
            messenger.subscribe(&key_expr, qos.into()).await
        }
        .map_err(Error::PeppyMessagingInterface)?;

        Ok(subscription)
    }

    async fn emit_topic_message(
        &self,
        as_master_node: &str,
        as_instance_id: &str,
        as_node_name: &str,
        as_topic_name: &str,
        qos: QoSProfile,
        payload: Bytes,
    ) -> Result<()> {
        let key_expr = format!(
            "*/{}/*/{}/topic/{}/{}",
            as_master_node, as_instance_id, as_node_name, as_topic_name
        );
        let msg = Message::new(&key_expr, payload);

        let mut messenger = self.messenger.lock().await;
        messenger
            .publish(msg, qos.into())
            .await
            .map_err(Error::PeppyMessagingInterface)
    }

    async fn expose_service(
        &self,
        bound_master_node: &str,
        as_instance_id: &str,
        as_node_name: &str,
        as_service_name: &str,
    ) -> Result<ServiceEndpoint> {
        let service_root = format!("service/{as_node_name}/{as_service_name}");
        self.create_service_endpoint(bound_master_node, service_root, as_instance_id)
            .await
    }

    async fn create_service_endpoint(
        &self,
        bound_master_node: &str,
        service_root: String,
        as_instance_id: &str,
    ) -> Result<ServiceEndpoint> {
        // Format: target_master/caller_master/target_instance/caller_instance/service_root/request/id
        // We need 4 subscription patterns to match all valid request combinations:
        let patterns = [
            // 1. Specific master, specific instance
            format!("{bound_master_node}/*/{as_instance_id}/*/{service_root}/request/**"),
            // 2. Specific master, broadcast instance
            format!("{bound_master_node}/*/{BROADCAST_MARKER}/*/{service_root}/request/**"),
            // 3. Broadcast master, specific instance
            format!("{BROADCAST_MARKER}/*/{as_instance_id}/*/{service_root}/request/**"),
            // 4. Broadcast master, broadcast instance
            format!("{BROADCAST_MARKER}/*/{BROADCAST_MARKER}/*/{service_root}/request/**"),
        ];

        let messenger = self.messenger.lock().await;

        // Create all 4 subscriptions
        let sub0 = messenger
            .subscribe(&patterns[0], SubscriberQoS::Standard)
            .await
            .map_err(Error::PeppyMessagingInterface)?;
        let sub1 = messenger
            .subscribe(&patterns[1], SubscriberQoS::Standard)
            .await
            .map_err(Error::PeppyMessagingInterface)?;
        let sub2 = messenger
            .subscribe(&patterns[2], SubscriberQoS::Standard)
            .await
            .map_err(Error::PeppyMessagingInterface)?;
        let sub3 = messenger
            .subscribe(&patterns[3], SubscriberQoS::Standard)
            .await
            .map_err(Error::PeppyMessagingInterface)?;

        drop(messenger);

        Ok(ServiceEndpoint::new(
            Arc::clone(&self.messenger),
            [sub0, sub1, sub2, sub3],
            bound_master_node.to_string(),
            service_root,
            as_instance_id.to_string(),
        ))
    }

    #[allow(clippy::too_many_arguments)]
    async fn poll_service(
        &self,
        message_type: &str,
        bound_master_node: &str,
        as_instance_id: &str,
        target_node_name: &str,
        target_service_name: &str,
        target_master_node: Option<&str>,
        target_instance_id: Option<&str>,
        request_payload: Bytes,
        response_timeout: Duration,
    ) -> Result<TopicMessage> {
        let service_root = format!(
            "{}/{}/{}",
            message_type, target_node_name, target_service_name
        );
        // Caller's instance as TARGET (identifies who is calling in request)
        let caller_target_instance_segment = format_target_instance_segment(as_instance_id)
            .unwrap_or_else(|| INSTANCE_ID_WILDCARD.to_string());
        // Caller's instance as BOUND (caller receives response)
        let caller_bound_instance_segment = format_bound_instance_segment(as_instance_id)
            .unwrap_or_else(|| INSTANCE_ID_WILDCARD.to_string());

        let target_instance_id = target_instance_id.map(str::to_string);

        // If no target specified, use BROADCAST_MARKER for broadcast requests
        // This allows Zenoh subscription patterns to filter at the key expression level
        let (effective_target_master, effective_target_instance) =
            match (target_master_node, target_instance_id.as_deref()) {
                (Some(master), Some(instance)) => (master.to_string(), instance.to_string()),
                (Some(master), None) => (master.to_string(), BROADCAST_MARKER.to_string()),
                (None, Some(instance)) => (BROADCAST_MARKER.to_string(), instance.to_string()),
                (None, None) => (BROADCAST_MARKER.to_string(), BROADCAST_MARKER.to_string()),
            };

        // Target's instance as BOUND (service is bound to receive requests)
        let target_bound_instance_segment =
            format_bound_instance_segment(&effective_target_instance);
        let request_id = generate_request_id();

        // Format: target_master/caller_master/target_instance/caller_instance/service_root/request/id
        let target_master = target_bound_instance_segment
            .as_ref()
            .map(|_| effective_target_master.as_str())
            .unwrap_or(BROADCAST_MARKER);
        let target_instance = target_bound_instance_segment
            .as_deref()
            .unwrap_or(BROADCAST_MARKER);
        let request_topic = format!(
            "{}/{}/{}/{}/{}/request/{request_id}",
            target_master,
            bound_master_node,
            target_instance,
            caller_target_instance_segment,
            service_root
        );

        // Response topic format: caller_master/responder_master/caller_instance/responder_instance/service_root/response/request_id
        // Always subscribe with wildcards for responder master and instance.
        // The request_id (UUID) uniquely identifies our response, so wildcards
        // are safe and — crucially — keep the subscriber pattern consistent
        // across different targeting modes, avoiding Zenoh routing-table
        // interference when the same session is reused for successive polls
        // with varying target specificity.
        let response_topic = format!(
            "{}/*/{}/*/{}/response/{request_id}",
            bound_master_node, caller_bound_instance_segment, service_root
        );

        let mut response_subscription = {
            let messenger = self.messenger.lock().await;
            messenger
                .subscribe(&response_topic, SubscriberQoS::Standard)
                .await
        }
        .map_err(Error::PeppyMessagingInterface)?;

        {
            let mut messenger = self.messenger.lock().await;
            messenger
                .publish(
                    Message::new(&request_topic, request_payload),
                    PublisherQoS::Standard,
                )
                .await
                .map_err(Error::PeppyMessagingInterface)?;
        }

        // Wait for the response using a deadline-based loop that tracks service acks.
        // The service sends an ack immediately upon receiving the request (before the
        // handler runs). If we receive the ack but no response, the service is alive
        // but slow (ServiceTimeout). If we receive nothing, nobody is listening
        // (ServiceUnreachable).
        let deadline = Instant::now() + response_timeout;
        let mut received_ack = false;

        let response = loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                if received_ack {
                    return Err(Error::ServiceTimeout {
                        instance_id: target_instance_id,
                        service_name: target_service_name.to_string(),
                    });
                } else {
                    return Err(Error::ServiceUnreachable {
                        instance_id: target_instance_id,
                        service_name: target_service_name.to_string(),
                    });
                }
            }

            match timeout(remaining, response_subscription.rx.recv()).await {
                Ok(Some(message)) => {
                    if is_service_ack_payload(&message.payload().as_bytes()) {
                        received_ack = true;
                        continue;
                    }
                    break message;
                }
                Ok(None) => {
                    return Err(Error::PeppyMessagingInterface(
                        PeppyMessagingInterfaceError::BackendError(
                            "service response channel closed".to_string(),
                        ),
                    ));
                }
                Err(_) => {
                    if received_ack {
                        return Err(Error::ServiceTimeout {
                            instance_id: target_instance_id,
                            service_name: target_service_name.to_string(),
                        });
                    } else {
                        return Err(Error::ServiceUnreachable {
                            instance_id: target_instance_id,
                            service_name: target_service_name.to_string(),
                        });
                    }
                }
            }
        };

        let response_payload = response.payload().to_bytes();
        if let Some(reason) = decode_service_error_payload(response_payload.as_ref()) {
            return Err(Error::ServiceError {
                instance_id: target_instance_id,
                service_name: target_service_name.to_string(),
                reason,
            });
        }

        Ok(response)
    }

    async fn expose_action(
        &self,
        bound_master_node: &str,
        as_node_name: &str,
        as_action_name: &str,
        as_instance_id: &str,
    ) -> Result<ActionCreation> {
        let action_root = format!("action/{}/{}", as_node_name, as_action_name);

        let goal_service_root = format!("{action_root}/goal");
        let cancel_service_root = format!("{action_root}/cancel");
        let result_service_root = format!("{action_root}/result");

        let bound_instance_segment = format_bound_instance_segment(as_instance_id)
            .unwrap_or_else(|| as_instance_id.to_string());
        let feedback_topic_suffix = format!(
            "*/{bound_master_node}/*/{bound_instance_segment}/{action_root}/feedback/{as_instance_id}"
        );

        let goal_service = self
            .create_service_endpoint(bound_master_node, goal_service_root, as_instance_id)
            .await?;
        let cancel_service = self
            .create_service_endpoint(bound_master_node, cancel_service_root, as_instance_id)
            .await?;
        let result_service = self
            .create_service_endpoint(bound_master_node, result_service_root, as_instance_id)
            .await?;

        let feedback_publisher = TopicPublisher::new(
            Arc::clone(&self.messenger),
            feedback_topic_suffix,
            PublisherQoS::Standard,
        );

        Ok(ActionCreation {
            goal_service,
            cancel_service,
            feedback_publisher,
            result_service,
        })
    }
}
