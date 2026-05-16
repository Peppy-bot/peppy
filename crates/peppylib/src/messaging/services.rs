use super::{
    BROADCAST_MARKER, MessengerHandle, PROBE_TIMEOUT, SERVICE_ACK_PAYLOAD, SERVICE_PROBE_PAYLOAD,
    encode_service_handler_error, format_instance_segment, is_service_probe_payload,
};
use crate::error::{Error, Result};
use crate::runtime::{TaskHandle, spawn};
use crate::types::{Message, Payload};
use pmi::{
    Message as PmiMessage, Messenger, MessengerBackend, PublisherQoS, Subscription, TopicMessage,
};
use std::{fmt, sync::Arc};
use tokio::{sync::Mutex, time::Duration};
use tracing::error;

/// Runs a service handler and converts any error into a protocol-level error payload.
async fn run_handler<F, Fut>(handler: F, context: ServiceRequestContext) -> Payload
where
    F: FnOnce(ServiceRequestContext) -> Fut,
    Fut: std::future::Future<Output = Result<Payload>>,
{
    match handler(context).await {
        Ok(payload) => payload,
        Err(err) => {
            let reason = err.to_string();
            error!(%reason, "service handler returned error");
            encode_service_handler_error(&reason)
        }
    }
}

pub struct ServiceMessenger;

pub struct ServiceEndpoint {
    messenger: Arc<Mutex<Messenger>>,
    /// Subscriptions to service requests. Four patterns are needed to match:
    /// - [0] Requests targeting this specific core node and instance
    /// - [1] Requests targeting this specific core node with broadcast instance
    /// - [2] Broadcast requests (any core node) targeting this specific instance
    /// - [3] Full broadcast requests (any core node, any instance)
    subscriptions: [Subscription; 4],
    bound_core_node: String,
    service_root: String,
    instance_id: String,
}

impl ServiceEndpoint {
    pub(crate) fn new(
        messenger: Arc<Mutex<Messenger>>,
        subscriptions: [Subscription; 4],
        bound_core_node: String,
        service_root: String,
        instance_id: String,
    ) -> Self {
        Self {
            messenger,
            subscriptions,
            bound_core_node,
            service_root,
            instance_id,
        }
    }
}

/// Handle returned by [`ServiceEndpoint::recv_next_request`] that must be used to send the
/// response back to the caller.
pub struct ServiceResponder {
    messenger: Arc<Mutex<Messenger>>,
    response_topic: String,
}

impl ServiceResponder {
    /// Send the response payload for this request.
    pub async fn respond(self, payload: Payload) -> Result<()> {
        let message = PmiMessage::new(&self.response_topic, payload.into_inner());
        let mut messenger = self.messenger.lock().await;
        messenger
            .publish(message, PublisherQoS::Standard)
            .await
            .map_err(Error::PeppyMessagingInterface)
    }
}

impl ServiceEndpoint {
    /// Waits for the next service request, auto-handles probes, sends ACK, and returns the
    /// request context together with a [`ServiceResponder`] that must be used to send the reply.
    ///
    /// Returns `Ok(None)` when the subscription stream has closed.
    pub async fn recv_next_request(
        &mut self,
    ) -> Result<Option<(ServiceRequestContext, ServiceResponder)>> {
        match self.next_request().await {
            Ok((context, response_topic)) => {
                self.publish_ack(&response_topic).await?;
                Ok(Some((
                    context,
                    ServiceResponder {
                        messenger: Arc::clone(&self.messenger),
                        response_topic,
                    },
                )))
            }
            Err(Error::ServiceRequestStreamClosed) => Ok(None),
            Err(err) => Err(err),
        }
    }

    /// Handles a single incoming request using the provided callback.
    ///
    /// Returns `Ok(true)` after successfully processing a request, or `Ok(false)` when the
    /// subscription stream has closed.
    pub async fn handle_next_request<F, Fut>(&mut self, handler: F) -> Result<bool>
    where
        F: FnOnce(ServiceRequestContext) -> Fut,
        Fut: std::future::Future<Output = Result<Payload>>,
    {
        let Some((context, responder)) = self.recv_next_request().await? else {
            return Ok(false);
        };
        responder
            .respond(run_handler(handler, context).await)
            .await?;
        Ok(true)
    }

    /// Handles requests until the subscription stream ends.
    pub async fn handle_requests<F, Fut>(&mut self, mut handler: F) -> Result<()>
    where
        F: FnMut(ServiceRequestContext) -> Fut,
        Fut: std::future::Future<Output = Result<Payload>>,
    {
        while let Some((context, responder)) = self.recv_next_request().await? {
            responder
                .respond(run_handler(&mut handler, context).await)
                .await?;
        }
        Ok(())
    }

    /// Spawns the handler on its own task so multiple requests can progress concurrently.
    /// Returns `Ok(None)` when the subscription closes before yielding a request.
    pub async fn spawn_next_request_handler<F, Fut>(
        &mut self,
        handler: F,
    ) -> Result<Option<TaskHandle<Result<()>>>>
    where
        F: FnOnce(ServiceRequestContext) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = Result<Payload>> + Send + 'static,
    {
        let Some((context, responder)) = self.recv_next_request().await? else {
            return Ok(None);
        };
        let task =
            spawn(async move { responder.respond(run_handler(handler, context).await).await });
        Ok(Some(task))
    }

    async fn next_request(&mut self) -> Result<(ServiceRequestContext, String)> {
        loop {
            // Destructure to get separate mutable references for tokio::select!
            let [sub0, sub1, sub2, sub3] = &mut self.subscriptions;

            // Use tokio::select! to receive from any of the 4 subscription patterns
            let request = tokio::select! {
                msg = sub0.rx.recv() => msg,
                msg = sub1.rx.recv() => msg,
                msg = sub2.rx.recv() => msg,
                msg = sub3.rx.recv() => msg,
            };

            match request {
                Some(request) => {
                    match self.build_request_context(request) {
                        Ok((context, response_topic)) => {
                            // Auto-handle probes: respond immediately without invoking
                            // the user handler, so is_reachable() checks are transparent.
                            if is_service_probe_payload(context.message().payload().as_ref()) {
                                let _ = self.publish_response(response_topic, Payload::new()).await;
                                continue;
                            }
                            return Ok((context, response_topic));
                        }
                        Err(Error::InvalidServiceRequest { .. }) => {
                            // Skip messages that do not match this service endpoint.
                            continue;
                        }
                        Err(err) => return Err(err),
                    }
                }
                None => return Err(Error::ServiceRequestStreamClosed),
            }
        }
    }

    fn build_request_context(
        &self,
        request: TopicMessage,
    ) -> Result<(ServiceRequestContext, String)> {
        // Format: target_core_node/caller_core_node/target_instance/caller_instance/service_root/request/id
        let identifier = request.key_expr().to_string();
        let mut parts = identifier.split('/').filter(|segment| !segment.is_empty());

        // Parse target_core_node (first segment)
        let target_core_node_segment =
            parts.next().ok_or_else(|| Error::InvalidServiceRequest {
                identifier: identifier.clone(),
                reason: "missing target core node segment in request".to_string(),
            })?;

        // Parse caller_core_node (second segment)
        let caller_core_node_segment =
            parts.next().ok_or_else(|| Error::InvalidServiceRequest {
                identifier: identifier.clone(),
                reason: "missing caller core node segment in request".to_string(),
            })?;

        // Parse target_instance (third segment)
        let target_instance_segment = parts.next().ok_or_else(|| Error::InvalidServiceRequest {
            identifier: identifier.clone(),
            reason: "missing target instance segment in request".to_string(),
        })?;

        // Parse caller_instance (fourth segment)
        let caller_instance_segment = parts.next().ok_or_else(|| Error::InvalidServiceRequest {
            identifier: identifier.clone(),
            reason: "missing caller instance segment in request".to_string(),
        })?;
        let response_target_instance_segment = format_instance_segment(self.instance_id.as_str())
            .unwrap_or_else(|| BROADCAST_MARKER.to_string());

        // Parse and validate service root segments
        let expected_root_segments: Vec<_> = self
            .service_root
            .split('/')
            .filter(|segment| !segment.is_empty())
            .collect();

        for expected_segment in expected_root_segments {
            let Some(segment) = parts.next() else {
                let reason = "request is missing expected service path segments".to_string();
                error!(%identifier, %reason, "service received invalid request");
                return Err(Error::InvalidServiceRequest {
                    identifier: identifier.clone(),
                    reason,
                });
            };

            if segment != expected_segment {
                let reason = format!(
                    "request path does not match service root; expected segment `{expected_segment}`, got `{segment}`"
                );
                error!(%identifier, %reason, "service received invalid request");
                return Err(Error::InvalidServiceRequest {
                    identifier: identifier.clone(),
                    reason,
                });
            }
        }

        // Parse request marker
        let request_marker = parts.next().unwrap_or_default();
        if request_marker != "request" {
            let reason = "missing request marker segment".to_string();
            error!(%identifier, %reason, "service received invalid request");
            return Err(Error::InvalidServiceRequest {
                identifier: identifier.clone(),
                reason,
            });
        }

        // Parse request_id
        let request_id = match parts.next().filter(|segment| !segment.is_empty()) {
            Some(id) => id.to_string(),
            None => {
                error!(%identifier, "service received request without request id segment");
                return Err(Error::InvalidServiceRequest {
                    identifier,
                    reason: "missing request id segment".to_string(),
                });
            }
        };

        if parts.next().is_some() {
            let reason = "request contains unexpected trailing segments".to_string();
            error!(%identifier, %reason, "service received invalid request");
            return Err(Error::InvalidServiceRequest { identifier, reason });
        }

        // Construct message_identifier preserving the original target values from the request
        // This ensures all listeners receiving a broadcast see the same key_expr
        let message_identifier = format!(
            "{}/{}/{}/{}/{}/request/{request_id}",
            target_core_node_segment,
            caller_core_node_segment,
            target_instance_segment,
            caller_instance_segment,
            self.service_root
        );

        // Response topic format: caller_core_node/responder_core_node/caller_instance/responder_instance/service_root/response/request_id
        // This ensures core_node() returns responder's core node (position 1) and instance_id() returns responder's instance (position 3)
        let response_topic = format!(
            "{}/{}/{}/{}/{}/response/{request_id}",
            caller_core_node_segment,
            self.bound_core_node,
            caller_instance_segment,
            response_target_instance_segment,
            self.service_root
        );

        let message = TopicMessage::new(&message_identifier, request.into_payload())?;
        let context = ServiceRequestContext::new(message, request_id);

        Ok((context, response_topic))
    }

    async fn publish_ack(&self, response_topic: &str) -> Result<()> {
        Self::publish_response_with_messenger(
            Arc::clone(&self.messenger),
            response_topic.to_string(),
            Payload::from_static(SERVICE_ACK_PAYLOAD),
        )
        .await
    }

    async fn publish_response(&self, topic: String, payload: Payload) -> Result<()> {
        Self::publish_response_with_messenger(Arc::clone(&self.messenger), topic, payload).await
    }

    async fn publish_response_with_messenger(
        messenger: Arc<Mutex<Messenger>>,
        topic: String,
        payload: Payload,
    ) -> Result<()> {
        let response = PmiMessage::new(&topic, payload.into_inner());
        let mut messenger = messenger.lock().await;
        messenger
            .publish(response, PublisherQoS::Standard)
            .await
            .map_err(Error::PeppyMessagingInterface)
    }
}

pub struct ServiceRequestContext {
    message: Message,
    request_id: String,
}

impl ServiceRequestContext {
    pub fn new(message: TopicMessage, request_id: String) -> Self {
        Self {
            message: Message(message),
            request_id,
        }
    }

    pub fn message(&self) -> &Message {
        &self.message
    }

    pub fn request_id(&self) -> &str {
        &self.request_id
    }
}

impl fmt::Debug for ServiceRequestContext {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ServiceRequestContext")
            .field("message_key", &self.message.key_expr())
            .field("instance_id", &self.message.instance_id())
            .field("request_id", &self.request_id)
            .finish()
    }
}

impl ServiceMessenger {
    /// Listening as a service is a 2 way stream, so the process that exposes the service needs to provide its instance_id
    ///
    /// `iface_name`/`iface_tag` scope the wire path to a `conforms_to` interface; pass
    /// [`NATIVE_IFACE_SEGMENT_NAME`](super::NATIVE_IFACE_SEGMENT_NAME)/
    /// [`NATIVE_IFACE_SEGMENT_TAG`](super::NATIVE_IFACE_SEGMENT_TAG) for native (non-conformed) services.
    #[allow(clippy::too_many_arguments)]
    pub async fn listen(
        messenger: &MessengerHandle,
        as_core_node: &str,
        as_instance_id: &str,
        as_node_name: &str,
        iface_name: &str,
        iface_tag: &str,
        as_service_name: &str,
    ) -> Result<ServiceEndpoint> {
        messenger
            .expose_service(
                as_core_node,
                as_instance_id,
                as_node_name,
                iface_name,
                iface_tag,
                as_service_name,
            )
            .await
    }

    /// If `target_instance_id` is `None`, this call returns with the first service instance that it hits.
    ///
    /// `iface_name`/`iface_tag` must match the segments the responder used in [`Self::listen`].
    #[allow(clippy::too_many_arguments)]
    pub async fn poll(
        messenger: &MessengerHandle,
        bound_core_node: &str,
        as_instance_id: &str,
        target_node_name: &str,
        iface_name: &str,
        iface_tag: &str,
        target_service_name: &str,
        target_core_node: Option<&str>,
        target_instance_id: Option<&str>,
        request_payload: Payload,
        response_timeout: impl Into<Option<Duration>>,
    ) -> Result<Message> {
        messenger
            .poll_service(
                "service",
                bound_core_node,
                as_instance_id,
                target_node_name,
                iface_name,
                iface_tag,
                target_service_name,
                target_core_node,
                target_instance_id,
                request_payload,
                response_timeout,
            )
            .await
    }

    /// Sends a lightweight probe to check whether a service is listening.
    ///
    /// The probe is handled transparently by the service's request loop — the user
    /// handler is never invoked. Returns `true` if the service responds within
    /// [`PROBE_TIMEOUT`], `false` if unreachable.
    #[allow(clippy::too_many_arguments)]
    pub async fn is_reachable(
        messenger: &MessengerHandle,
        bound_core_node: &str,
        as_instance_id: &str,
        target_node_name: &str,
        iface_name: &str,
        iface_tag: &str,
        target_service_name: &str,
        target_core_node: Option<&str>,
        target_instance_id: Option<&str>,
    ) -> Result<bool> {
        match messenger
            .poll_service(
                "service",
                bound_core_node,
                as_instance_id,
                target_node_name,
                iface_name,
                iface_tag,
                target_service_name,
                target_core_node,
                target_instance_id,
                Payload::from_static(SERVICE_PROBE_PAYLOAD),
                PROBE_TIMEOUT,
            )
            .await
        {
            Ok(_) => Ok(true),
            Err(Error::ServiceUnreachable { .. }) => Ok(false),
            Err(Error::ServiceTimeout { .. }) => Ok(true),
            Err(e) => Err(e),
        }
    }
}
