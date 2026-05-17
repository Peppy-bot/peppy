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

// Public re-exports. `Iface` / `IfaceError` / `ServiceKind` describe the
// shape of messaging calls and surface in user-facing peppylib APIs.
// `ActionWireSender` is exposed because peppylib-py caches one to drive
// subsequent cancel / result calls without locking. The other wire structs
// (TopicWire*, ServiceWire*, ActionWireReceiver) are internal to peppylib's
// own messaging implementation — each submodule imports them directly from
// `pmi::`.
pub use pmi::{ActionWireSender, Iface, IfaceError, ServiceKind};

use crate::error::{Error, Result};
use crate::types::{Message, Payload};
use config::node::QoSProfile;
use pmi::{
    ActionWireReceiver, Messenger, MessengerAdapter, MessengerBackend, MessengerPublisher,
    PeppyMessagingInterfaceError, PublisherQoS, ServiceWireReceiver, ServiceWireSender,
    SubscriberQoS, Subscription as PmiSubscription, TopicWireReceiver, TopicWireSender,
    ZenohAdapter, ZenohNetProtocol,
};
use sha2::{Digest, Sha256};
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::{
    sync::Mutex,
    time::{Duration, Instant, timeout},
};

// services
pub const NODE_HEALTH_SERVICE: &str = "node_health";
pub const NODE_READY_SERVICE: &str = "node_ready";
pub const SHUTDOWN_SERVICE: &str = "shutdown";

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

impl MessengerHandle {
    pub fn from_shared(messenger: Arc<Mutex<Messenger>>) -> Self {
        Self { messenger }
    }

    /// Pre-bind a per-topic publisher. Locks the messenger once at
    /// declaration to extract the per-adapter handle, then never again — the
    /// returned [`pmi::MessengerPublisher`] holds its own state (an
    /// `Arc<zenoh::Session>` clone or `Arc<Mutex<HashMap>>` clones for the
    /// mock) and `publish` skips the central messenger mutex.
    pub(crate) async fn declare_topic_publisher(
        &self,
        sender: &TopicWireSender,
        qos: PublisherQoS,
    ) -> Result<MessengerPublisher> {
        let messenger = self.messenger.lock().await;
        messenger
            .declare_topic_publisher(sender, qos)
            .map_err(Error::PeppyMessagingInterface)
    }

    /// Pre-bind a per-goal action-feedback publisher.
    pub(crate) async fn declare_action_feedback_publisher(
        &self,
        recv: &ActionWireReceiver,
        goal_id: &str,
        qos: PublisherQoS,
    ) -> Result<MessengerPublisher> {
        let messenger = self.messenger.lock().await;
        messenger
            .declare_action_feedback_publisher(recv, goal_id, qos)
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

    async fn subscribe_to_topic(
        &self,
        recv: &TopicWireReceiver,
        qos: QoSProfile,
    ) -> Result<PmiSubscription> {
        let messenger = self.messenger.lock().await;
        messenger
            .subscribe_topic(recv, qos.into())
            .await
            .map_err(Error::PeppyMessagingInterface)
    }

    async fn emit_topic_message(
        &self,
        sender: &TopicWireSender,
        qos: QoSProfile,
        payload: Payload,
    ) -> Result<()> {
        let mut messenger = self.messenger.lock().await;
        messenger
            .publish_topic(sender, payload.into_inner().into(), qos.into())
            .await
            .map_err(Error::PeppyMessagingInterface)
    }

    pub(crate) async fn expose_service(
        &self,
        recv: &ServiceWireReceiver,
    ) -> Result<ServiceEndpoint> {
        let subscriptions = {
            let messenger = self.messenger.lock().await;
            messenger
                .listen_service(recv)
                .await
                .map_err(Error::PeppyMessagingInterface)?
        };
        Ok(ServiceEndpoint::new(
            Arc::clone(&self.messenger),
            subscriptions,
            recv.clone(),
        ))
    }

    pub(crate) async fn poll_service(
        &self,
        sender: &ServiceWireSender,
        request_payload: Payload,
        response_timeout: impl Into<Option<Duration>>,
    ) -> Result<Message> {
        let response_timeout: Option<Duration> = response_timeout.into();
        let request_id = generate_request_id();

        let mut response_subscription = {
            let mut messenger = self.messenger.lock().await;
            messenger
                .open_service_call(sender, &request_id, request_payload.into_inner().into())
                .await
                .map_err(Error::PeppyMessagingInterface)?
        };

        // Wait for the response, filtering out service acks. The service sends an
        // ack immediately upon receiving the request (before the handler runs).
        // With a timeout, ack-without-response → ServiceTimeout, no ack at all →
        // ServiceUnreachable. With no timeout (None), we wait indefinitely — used
        // in tests to avoid wall-clock dependencies.
        let channel_closed_err = || {
            Error::PeppyMessagingInterface(PeppyMessagingInterfaceError::BackendError(
                "service response channel closed".to_string(),
            ))
        };
        let target_service_name = sender.to_service_name().to_string();
        let target_instance_id = sender.to_instance_id().map(str::to_string);

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
                                service_name: target_service_name,
                            });
                        } else {
                            return Err(Error::ServiceUnreachable {
                                instance_id: target_instance_id,
                                service_name: target_service_name,
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
                                    service_name: target_service_name,
                                });
                            } else {
                                return Err(Error::ServiceUnreachable {
                                    instance_id: target_instance_id,
                                    service_name: target_service_name,
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
                service_name: target_service_name,
                reason,
            });
        }

        Ok(response)
    }

    pub(crate) async fn expose_action(&self, recv: &ActionWireReceiver) -> Result<ActionCreation> {
        let goal_service = self.expose_service(&recv.goal_service()).await?;
        let cancel_service = self.expose_service(&recv.cancel_service()).await?;
        let result_service = self.expose_service(&recv.result_service()).await?;

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
            recv.clone(),
            PublisherQoS::Important,
        );

        Ok(ActionCreation {
            goal_service,
            cancel_service,
            feedback_publisher_factory,
            result_service,
        })
    }

    pub(crate) async fn subscribe_action_feedback(
        &self,
        sender: &ActionWireSender,
        goal_id: &str,
        qos: SubscriberQoS,
    ) -> Result<PmiSubscription> {
        let messenger = self.messenger.lock().await;
        messenger
            .subscribe_action_feedback(sender, goal_id, qos)
            .await
            .map_err(Error::PeppyMessagingInterface)
    }
}
