use super::{
    MessengerHandle, PROBE_TIMEOUT, SERVICE_ACK_PAYLOAD, SERVICE_PROBE_PAYLOAD,
    encode_service_handler_error, generate_short_id, is_service_probe_payload,
};
use crate::error::{Error, Result};
use crate::runtime::{TaskHandle, spawn};
use crate::types::{Message, Payload};
use pmi::{
    Messenger, ResponseToken, SenderTarget, ServiceKind, ServiceQueryable, ServiceWireReceiver,
    ServiceWireSender, TopicMessage,
};
use std::{fmt, sync::Arc};
use tokio::{sync::Mutex, time::Duration};
use tracing::{error, warn};

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

/// Server-side endpoint for a single service. Wraps the per-link-id queryable
/// fan-in produced by [`pmi::MessengerBackend::listen_service`]: each inbound
/// request carries its own [`ResponseToken`], so responding no longer needs
/// the central messenger mutex.
///
/// `_messenger` is kept solely to anchor the underlying Zenoh session's
/// lifetime — the queryable's inbound callback (and the flume sender feeding
/// `queryable.rx`) lives in the session's queryable registry, so once every
/// strong reference to the messenger drops the session disappears and the
/// channel closes mid-flight.
pub struct ServiceEndpoint {
    queryable: ServiceQueryable,
    _messenger: Arc<Mutex<Messenger>>,
}

impl ServiceEndpoint {
    pub(crate) fn new(messenger: Arc<Mutex<Messenger>>, queryable: ServiceQueryable) -> Self {
        Self {
            queryable,
            _messenger: messenger,
        }
    }
}

/// Handle returned by [`ServiceEndpoint::recv_next_request`] that must be used
/// to send the response back to the caller. Wraps the inbound query's
/// [`ResponseToken`] — `respond` issues a single reply on it.
pub struct ServiceResponder {
    token: ResponseToken,
}

impl ServiceResponder {
    /// Send the response payload for this request.
    pub async fn respond(self, payload: Payload) -> Result<()> {
        self.token
            .respond(payload.into_inner().into())
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
            Ok((context, token)) => {
                // ACK reply before invoking the user handler — the caller's
                // poll loop uses this to distinguish ServiceUnreachable
                // (no ACK at all) from ServiceTimeout (ACK but no handler
                // response within the timeout). Zenoh queryables allow
                // multiple replies per query, so the same token is reused
                // for the real response.
                token
                    .respond(bytes::Bytes::from_static(SERVICE_ACK_PAYLOAD).into())
                    .await
                    .map_err(Error::PeppyMessagingInterface)?;
                Ok(Some((context, ServiceResponder { token })))
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

    async fn next_request(&mut self) -> Result<(ServiceRequestContext, ResponseToken)> {
        loop {
            match self.queryable.rx.recv().await {
                Some(incoming) => {
                    let payload_bytes = incoming.payload.to_bytes();

                    // Auto-handle probes: respond immediately without invoking
                    // the user handler, so is_reachable() checks are transparent.
                    if is_service_probe_payload(payload_bytes.as_ref()) {
                        if let Err(err) = incoming.token.respond(bytes::Bytes::new().into()).await {
                            warn!(%err, "failed to publish probe response");
                        }
                        continue;
                    }

                    // Synthesize a topic-shape keyexpr so the resulting
                    // `TopicMessage` exposes the caller's identity via
                    // `core_node()` / `instance_id()`. Segments 1 and 3 are the
                    // caller's slots; 0 and 2 are filler that the topic parser
                    // skips.
                    let synthetic_keyexpr = format!(
                        "svc/{}/svc/{}/req",
                        incoming.caller_core, incoming.caller_inst
                    );
                    let topic_message = TopicMessage::new(&synthetic_keyexpr, payload_bytes)
                        .map_err(Error::PeppyMessagingInterface)?;

                    let request_id = generate_short_id("request");
                    let context =
                        ServiceRequestContext::new(topic_message, request_id, incoming.link_id);
                    return Ok((context, incoming.token));
                }
                None => return Err(Error::ServiceRequestStreamClosed),
            }
        }
    }
}

pub struct ServiceRequestContext {
    message: Message,
    request_id: String,
    /// link_id segment parsed from the request keyexpr, already verified
    /// against the producer's bound set by the wire-format dispatch filter.
    /// Surfaced so action goal handlers can scope per-goal feedback under
    /// the link_id the consumer actually targeted, instead of inheriting
    /// the listener's pinned link_id (which doesn't exist under wildcard
    /// listen).
    link_id: String,
}

impl ServiceRequestContext {
    pub fn new(message: TopicMessage, request_id: String, link_id: String) -> Self {
        Self {
            message: Message(message),
            request_id,
            link_id,
        }
    }

    pub fn message(&self) -> &Message {
        &self.message
    }

    pub fn request_id(&self) -> &str {
        &self.request_id
    }

    pub fn link_id(&self) -> &str {
        &self.link_id
    }
}

impl fmt::Debug for ServiceRequestContext {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ServiceRequestContext")
            .field("core_node", &self.message.core_node())
            .field("instance_id", &self.message.instance_id())
            .field("request_id", &self.request_id)
            .field("link_id", &self.link_id)
            .finish()
    }
}

impl ServiceMessenger {
    /// Listen as a service. `link_ids` is the set of producer link_ids this
    /// process binds; the listener wildcards the wire link_id slot and the
    /// dispatch filter inside the wire-format module drops requests
    /// addressed to link_ids not in this set. An empty slice is normalized
    /// to the reserved default `_` segment, matching producers launched
    /// without `--link-id`.
    ///
    /// `as_identity` must match the [`SenderTarget`] callers will use in
    /// [`Self::poll`].
    #[allow(clippy::too_many_arguments)]
    pub async fn listen(
        messenger: &MessengerHandle,
        as_core_node: &str,
        as_instance_id: &str,
        as_identity: SenderTarget,
        link_ids: &[String],
        as_service_name: &str,
    ) -> Result<ServiceEndpoint> {
        let recv = ServiceWireReceiver::new(
            as_core_node,
            as_instance_id,
            as_identity,
            link_ids,
            as_service_name,
            ServiceKind::Service,
        )?;
        messenger.expose_service(&recv).await
    }

    /// Poll a service. `to_link_id` `None` broadcasts on the default
    /// link_id (used by consumers without a pinned link_id); `Some(value)`
    /// targets a specific producer link_id, used when a `depends_on` entry
    /// declares a link_id.
    ///
    /// If `to_instance_id` is `None`, this call returns with the first
    /// service instance that responds. `to_target` must match the
    /// [`SenderTarget`] the responder used in [`Self::listen`].
    #[allow(clippy::too_many_arguments)]
    pub async fn poll(
        messenger: &MessengerHandle,
        bound_core_node: &str,
        as_instance_id: &str,
        to_target: SenderTarget,
        to_link_id: Option<&str>,
        to_service_name: &str,
        to_core_node: Option<&str>,
        to_instance_id: Option<&str>,
        request_payload: Payload,
        response_timeout: impl Into<Option<Duration>>,
    ) -> Result<Message> {
        let sender = ServiceWireSender::new(
            bound_core_node,
            as_instance_id,
            to_core_node,
            to_instance_id,
            to_target,
            to_link_id,
            to_service_name,
            ServiceKind::Service,
        )?;
        messenger
            .poll_service(&sender, request_payload, response_timeout)
            .await
    }

    /// Sends a lightweight probe to check whether a service is listening at
    /// the targeted link_id. The probe is handled transparently by the
    /// service's request loop; the user handler is never invoked. Returns
    /// `true` if the service responds within [`PROBE_TIMEOUT`], `false` if
    /// unreachable.
    #[allow(clippy::too_many_arguments)]
    pub async fn is_reachable(
        messenger: &MessengerHandle,
        bound_core_node: &str,
        as_instance_id: &str,
        to_target: SenderTarget,
        to_link_id: Option<&str>,
        to_service_name: &str,
        to_core_node: Option<&str>,
        to_instance_id: Option<&str>,
    ) -> Result<bool> {
        match Self::poll(
            messenger,
            bound_core_node,
            as_instance_id,
            to_target,
            to_link_id,
            to_service_name,
            to_core_node,
            to_instance_id,
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
