#[cfg(all(test, feature = "zenoh"))]
mod tests;

mod actions;
mod services;
mod topics;

pub use actions::{
    ActionCreation, ActionFeedbackPublisher, ActionFeedbackPublisherFactory, ActionGoalHandle,
    ActionMessenger, DeclaredFeedback, EmptyPayloadError, NonEmptyPayload, generate_goal_id,
    unwrap_goal_payload, wrap_goal_payload,
};
pub use services::{ServiceEndpoint, ServiceMessenger, ServiceRequestContext, ServiceResponder};
pub use topics::{Subscription, TopicMessenger, TopicPublisher};

use crate::error::{Error, Result};
use crate::types::{Message, Payload};
use config::node::QoSProfile;
use pmi::{
    Message as PmiMessage, Messenger, MessengerAdapter, MessengerBackend, MessengerPublisher,
    PeppyMessagingInterfaceError, PublisherQoS, SubscriberQoS, Subscription as PmiSubscription,
    ZenohAdapter, ZenohNetProtocol,
};
use sha2::{Digest, Sha256};
use std::{
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
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

/// Encodes a service handler failure as a protocol-level error payload.
///
/// External wrappers (for example Python bindings) can use this to ensure
/// handler exceptions are reported to callers as `ServiceError` instead of
/// surfacing as request timeouts.
pub fn encode_service_handler_error(reason: &str) -> Payload {
    let mut payload = Vec::with_capacity(SERVICE_ERROR_PREFIX.len() + reason.len());
    payload.extend_from_slice(SERVICE_ERROR_PREFIX);
    payload.extend_from_slice(reason.as_bytes());
    Payload::from(payload)
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

/// 16 hex chars (64 bits) of correlation entropy, salted with `domain` so
/// IDs from different namespaces (request, goal, ...) cannot collide on a
/// timestamp + thread_id tie. A process-wide counter is folded in to keep
/// IDs unique even when two calls land on the same thread within a single
/// clock tick.
pub(crate) fn generate_short_id(domain: &str) -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);

    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    let thread_id = std::thread::current().id();
    let counter = COUNTER.fetch_add(1, Ordering::Relaxed);

    let mut hasher = Sha256::new();
    hasher.update(domain.as_bytes());
    hasher.update(timestamp.to_le_bytes());
    hasher.update(format!("{thread_id:?}").as_bytes());
    hasher.update(counter.to_le_bytes());
    let result = hasher.finalize();

    use std::fmt::Write;
    let mut hex = String::with_capacity(16);
    for b in result.iter().take(8) {
        let _ = write!(hex, "{b:02x}");
    }
    hex
}

fn generate_request_id() -> String {
    generate_short_id("request")
}

/// Formats an instance ID as a key-expression segment, returning `None` for wildcards.
fn format_instance_segment(instance_id: &str) -> Option<String> {
    (instance_id != INSTANCE_ID_WILDCARD).then(|| instance_id.to_string())
}

/// Formats a variant label as a key-expression segment. `None` resolves to
/// `*` (wildcard, i.e. "any variant of the target node"); a concrete label
/// (including [`config::runtime::DEFAULT_VARIANT`]) is used verbatim. The
/// target side of subscribe/expose key expressions always uses a concrete
/// label so subscriber pattern counts stay constant; the caller side
/// passes `None` to fan out across every variant.
fn format_variant_segment(variant: Option<&str>) -> String {
    variant.map(str::to_owned).unwrap_or_else(|| "*".to_owned())
}

impl MessengerHandle {
    pub fn from_shared(messenger: Arc<Mutex<Messenger>>) -> Self {
        Self { messenger }
    }

    /// Pre-bind a per-topic publisher. Locks the messenger once at
    /// declaration to extract the per-adapter handle, then never again — the
    /// returned [`pmi::MessengerPublisher`] holds its own state (an
    /// `Arc<zenoh::Session>` clone or `Arc<Mutex<HashMap>>` clones for the
    /// mock) and `publish` skips the central messenger mutex.
    pub(crate) async fn declare_publisher(
        &self,
        topic: String,
        qos: PublisherQoS,
    ) -> Result<MessengerPublisher> {
        let messenger = self.messenger.lock().await;
        messenger
            .declare_publisher(topic, qos)
            .map_err(Error::PeppyMessagingInterface)
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
        as_core_node: &str,
        as_instance_id: &str,
        to_node_name: &str,
        to_topic: &str,
        to_core_node: Option<&str>,
        to_instance_id: Option<&str>,
        to_variant: Option<&str>,
        qos: QoSProfile,
    ) -> Result<PmiSubscription> {
        let to_core_node = to_core_node.unwrap_or("*");
        let to_instance_id = to_instance_id.unwrap_or("*");
        let to_variant = format_variant_segment(to_variant);
        let key_expr = format!(
            "{as_core_node}/{to_core_node}/{as_instance_id}/{to_instance_id}/{to_variant}/topic/{to_node_name}/{to_topic}"
        );
        let subscription = {
            let messenger = self.messenger.lock().await;
            messenger.subscribe(&key_expr, qos.into()).await
        }
        .map_err(Error::PeppyMessagingInterface)?;

        Ok(subscription)
    }

    #[allow(clippy::too_many_arguments)]
    async fn emit_topic_message(
        &self,
        as_core_node: &str,
        as_instance_id: &str,
        as_variant: &str,
        as_node_name: &str,
        as_topic_name: &str,
        qos: QoSProfile,
        payload: Payload,
    ) -> Result<()> {
        let key_expr = format!(
            "*/{}/*/{}/{}/topic/{}/{}",
            as_core_node, as_instance_id, as_variant, as_node_name, as_topic_name
        );
        let msg = PmiMessage::new(&key_expr, payload.into_inner());

        let mut messenger = self.messenger.lock().await;
        messenger
            .publish(msg, qos.into())
            .await
            .map_err(Error::PeppyMessagingInterface)
    }

    async fn expose_service(
        &self,
        bound_core_node: &str,
        as_instance_id: &str,
        as_variant: &str,
        as_node_name: &str,
        as_service_name: &str,
    ) -> Result<ServiceEndpoint> {
        let service_root = format!("service/{as_node_name}/{as_service_name}");
        self.create_service_endpoint(bound_core_node, service_root, as_instance_id, as_variant)
            .await
    }

    async fn create_service_endpoint(
        &self,
        bound_core_node: &str,
        service_root: String,
        as_instance_id: &str,
        as_variant: &str,
    ) -> Result<ServiceEndpoint> {
        // Format: target_core_node/caller_core_node/target_instance/caller_instance/target_variant/service_root/request/id
        // The receiver always knows its own variant, so the variant slot is
        // a literal in every pattern: pattern count stays at 4 (broadcast vs.
        // specific only applies to the caller-identity slots).
        let patterns = [
            // 1. Specific core node, specific instance
            format!(
                "{bound_core_node}/*/{as_instance_id}/*/{as_variant}/{service_root}/request/**"
            ),
            // 2. Specific core node, broadcast instance
            format!(
                "{bound_core_node}/*/{BROADCAST_MARKER}/*/{as_variant}/{service_root}/request/**"
            ),
            // 3. Broadcast core node, specific instance
            format!(
                "{BROADCAST_MARKER}/*/{as_instance_id}/*/{as_variant}/{service_root}/request/**"
            ),
            // 4. Broadcast core node, broadcast instance
            format!(
                "{BROADCAST_MARKER}/*/{BROADCAST_MARKER}/*/{as_variant}/{service_root}/request/**"
            ),
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
            bound_core_node.to_string(),
            service_root,
            as_instance_id.to_string(),
        ))
    }

    #[allow(clippy::too_many_arguments)]
    async fn poll_service(
        &self,
        message_type: &str,
        bound_core_node: &str,
        as_instance_id: &str,
        target_node_name: &str,
        target_service_name: &str,
        target_core_node: Option<&str>,
        target_instance_id: Option<&str>,
        target_variant: Option<&str>,
        request_payload: Payload,
        response_timeout: impl Into<Option<Duration>>,
    ) -> Result<Message> {
        let response_timeout: Option<Duration> = response_timeout.into();
        let service_root = format!(
            "{}/{}/{}",
            message_type, target_node_name, target_service_name
        );
        // Caller's instance as TARGET (identifies who is calling in request)
        let caller_target_instance_segment =
            format_instance_segment(as_instance_id).unwrap_or_else(|| BROADCAST_MARKER.to_string());
        // Caller's instance as BOUND (caller receives response)
        let caller_bound_instance_segment =
            format_instance_segment(as_instance_id).unwrap_or_else(|| BROADCAST_MARKER.to_string());

        let target_instance_id = target_instance_id.map(str::to_string);

        // If no target specified, use BROADCAST_MARKER for broadcast requests
        // This allows Zenoh subscription patterns to filter at the key expression level
        let (effective_target_core_node, effective_target_instance) =
            match (target_core_node, target_instance_id.as_deref()) {
                (Some(core_node), Some(instance)) => (core_node.to_string(), instance.to_string()),
                (Some(core_node), None) => (core_node.to_string(), BROADCAST_MARKER.to_string()),
                (None, Some(instance)) => (BROADCAST_MARKER.to_string(), instance.to_string()),
                (None, None) => (BROADCAST_MARKER.to_string(), BROADCAST_MARKER.to_string()),
            };

        // Target's instance as BOUND (service is bound to receive requests)
        let target_bound_instance_segment = format_instance_segment(&effective_target_instance);
        let request_id = generate_request_id();

        // Format: target_core_node/caller_core_node/target_instance/caller_instance/target_variant/service_root/request/id
        let target_core_node = target_bound_instance_segment
            .as_ref()
            .map(|_| effective_target_core_node.as_str())
            .unwrap_or(BROADCAST_MARKER);
        let target_instance = target_bound_instance_segment
            .as_deref()
            .unwrap_or(BROADCAST_MARKER);
        let target_variant_segment = format_variant_segment(target_variant);
        let request_topic = format!(
            "{}/{}/{}/{}/{}/{}/request/{request_id}",
            target_core_node,
            bound_core_node,
            target_instance,
            caller_target_instance_segment,
            target_variant_segment,
            service_root
        );

        // Response topic format: caller_core_node/responder_core_node/caller_instance/responder_instance/service_root/response/request_id
        // Always subscribe with wildcards for responder core node and instance.
        // The request_id (UUID) uniquely identifies our response, so wildcards
        // are safe and — crucially — keep the subscriber pattern consistent
        // across different targeting modes, avoiding Zenoh routing-table
        // interference when the same session is reused for successive polls
        // with varying target specificity.
        let response_topic = format!(
            "{}/*/{}/*/{}/response/{request_id}",
            bound_core_node, caller_bound_instance_segment, service_root
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
                    pmi::Message::new(&request_topic, request_payload.into_inner()),
                    PublisherQoS::Standard,
                )
                .await
                .map_err(Error::PeppyMessagingInterface)?;
        }

        // Wait for the response, filtering out service acks.
        // The service sends an ack immediately upon receiving the request (before the
        // handler runs). With a timeout, if we receive the ack but no response, the
        // service is alive but slow (ServiceTimeout). If we receive nothing, nobody is
        // listening (ServiceUnreachable). With no timeout (None), we wait indefinitely
        // for the response signal — used in tests to avoid wall-clock dependencies.
        let channel_closed_err = || {
            Error::PeppyMessagingInterface(PeppyMessagingInterfaceError::BackendError(
                "service response channel closed".to_string(),
            ))
        };

        let response = match response_timeout {
            Some(response_timeout) => {
                let deadline = Instant::now() + response_timeout;
                let mut received_ack = false;

                loop {
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
                        Ok(None) => return Err(channel_closed_err()),
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
                }
            }
            None => loop {
                match response_subscription.rx.recv().await {
                    Some(message) => {
                        if is_service_ack_payload(&message.payload().as_bytes()) {
                            continue;
                        }
                        break message;
                    }
                    None => return Err(channel_closed_err()),
                }
            },
        };

        let response = Message::from(response);
        let response_payload = response.payload();
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
        bound_core_node: &str,
        as_node_name: &str,
        as_action_name: &str,
        as_instance_id: &str,
        as_variant: &str,
    ) -> Result<ActionCreation> {
        let action_root = format!("action/{}/{}", as_node_name, as_action_name);

        let goal_service_root = format!("{action_root}/goal");
        let cancel_service_root = format!("{action_root}/cancel");
        let result_service_root = format!("{action_root}/result");

        let bound_instance_segment =
            format_instance_segment(as_instance_id).unwrap_or_else(|| BROADCAST_MARKER.to_string());
        // Feedback shape mirrors the action request shape: `as_variant`
        // is the literal variant of THIS server, slotted before the
        // action_root. The repeated `as_instance_id` after `feedback/`
        // identifies the target server-side instance for per-goal scoping
        // (mirroring the request-side `target_instance_id`).
        let feedback_topic_suffix = format!(
            "*/{bound_core_node}/*/{bound_instance_segment}/{as_variant}/{action_root}/feedback/{as_instance_id}"
        );

        let goal_service = self
            .create_service_endpoint(
                bound_core_node,
                goal_service_root,
                as_instance_id,
                as_variant,
            )
            .await?;
        let cancel_service = self
            .create_service_endpoint(
                bound_core_node,
                cancel_service_root,
                as_instance_id,
                as_variant,
            )
            .await?;
        let result_service = self
            .create_service_endpoint(
                bound_core_node,
                result_service_root,
                as_instance_id,
                as_variant,
            )
            .await?;

        // Per-goal feedback uses `Important` (Block on congestion, DataHigh
        // priority) rather than `Standard`. The publisher is declared inside
        // the goal handler — the moment a fast server's first `emit_feedback`
        // fires, the local routing tables may not yet have the client's
        // subscription propagated through the router. Empirically, `Standard`
        // (Drop, Data) loses the first publish in tight in-process tests;
        // `Important` is delivered reliably. The block-on-congestion semantic
        // is also the right call for action feedback: it's preferable to
        // backpressure a fast emitter than to silently drop progress updates.
        let feedback_publisher_factory = actions::ActionFeedbackPublisherFactory::new(
            self.clone(),
            feedback_topic_suffix,
            PublisherQoS::Important,
        );

        Ok(ActionCreation {
            goal_service,
            cancel_service,
            feedback_publisher_factory,
            result_service,
        })
    }
}
