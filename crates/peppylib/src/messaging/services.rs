use super::discovery::discover_producer;
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
    /// Returns `Ok(None)` when the subscription stream has closed. ACK send
    /// failures (e.g. the caller dropped its reply stream before we replied)
    /// are logged and the request is silently dropped so a single misbehaving
    /// client cannot tear down the listener.
    pub async fn recv_next_request(
        &mut self,
    ) -> Result<Option<(ServiceRequestContext, ServiceResponder)>> {
        loop {
            match self.next_request().await {
                Ok((context, token)) => {
                    // ACK reply before invoking the user handler. The caller's
                    // poll loop uses this to distinguish ServiceUnreachable
                    // (no ACK at all) from ServiceTimeout (ACK but no handler
                    // response within the timeout).
                    if let Err(err) = token
                        .respond(bytes::Bytes::from_static(SERVICE_ACK_PAYLOAD).into())
                        .await
                    {
                        warn!(
                            %err,
                            request_id = %context.request_id(),
                            "failed to send service ACK; dropping request and continuing"
                        );
                        continue;
                    }
                    return Ok(Some((context, ServiceResponder { token })));
                }
                Err(Error::ServiceRequestStreamClosed) => return Ok(None),
                Err(err) => return Err(err),
            }
        }
    }

    /// Handles a single incoming request using the provided callback.
    ///
    /// Returns `Ok(true)` after attempting to process a request (even if
    /// sending the response failed — that failure is logged and swallowed so
    /// a single bad client cannot bubble out as a hard error), or `Ok(false)`
    /// when the subscription stream has closed.
    pub async fn handle_next_request<F, Fut>(&mut self, handler: F) -> Result<bool>
    where
        F: FnOnce(ServiceRequestContext) -> Fut,
        Fut: std::future::Future<Output = Result<Payload>>,
    {
        let Some((context, responder)) = self.recv_next_request().await? else {
            return Ok(false);
        };
        let request_id = context.request_id().to_string();
        let response = run_handler(handler, context).await;
        if let Err(err) = responder.respond(response).await {
            warn!(
                %err,
                %request_id,
                "failed to send service response; dropping request"
            );
        }
        Ok(true)
    }

    /// Handles requests until the subscription stream ends. Response send
    /// failures for individual requests are logged and skipped so the loop
    /// keeps serving subsequent callers.
    pub async fn handle_requests<F, Fut>(&mut self, mut handler: F) -> Result<()>
    where
        F: FnMut(ServiceRequestContext) -> Fut,
        Fut: std::future::Future<Output = Result<Payload>>,
    {
        while let Some((context, responder)) = self.recv_next_request().await? {
            let request_id = context.request_id().to_string();
            let response = run_handler(&mut handler, context).await;
            if let Err(err) = responder.respond(response).await {
                warn!(
                    %err,
                    %request_id,
                    "failed to send service response; dropping request"
                );
            }
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
                    // Auto-handle probes: respond immediately without invoking
                    // the user handler, so is_reachable() checks are transparent.
                    if is_service_probe_payload(&incoming.payload.as_bytes()) {
                        if let Err(err) = incoming.token.respond(bytes::Bytes::new().into()).await {
                            warn!(%err, "failed to publish probe response");
                        }
                        continue;
                    }

                    let topic_message = TopicMessage::from_parts(
                        incoming.caller_core,
                        incoming.caller_inst,
                        incoming.payload,
                    );

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
    /// Producer-side link_id that received this request — whichever bound
    /// link_id's queryable yielded the inbound query. Surfaced so action
    /// goal handlers can scope per-goal feedback under the link_id the
    /// consumer actually targeted.
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
    /// process binds; the adapter declares one queryable per bound link_id
    /// so Zenoh's keyexpr matcher routes each request to the right
    /// queryable. An empty slice is normalized to the reserved default `_`
    /// segment, matching producers launched without `--link-id`.
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

    /// Poll a service. `to_link_id` `None` emits the wildcard `*` in the
    /// link_id slot so any matching producer queryable replies (used by
    /// consumers without a pinned link_id); `Some(value)` targets a
    /// specific producer link_id, used when a `depends_on` entry declares
    /// a link_id.
    ///
    /// When `target_instance_id` is `None` (wildcard / from_any), this
    /// performs a discover-then-pin sequence: a lightweight probe is sent
    /// to identify a single responding producer's `(core_node, instance_id)`,
    /// then the real request is delivered pinned to that producer. The
    /// probe is filtered server-side before the user handler runs, so
    /// non-winning producers never see the request. This costs one extra
    /// round-trip; pinned callers (`target_instance_id: Some`) skip
    /// discovery and pay no overhead.
    ///
    /// `to_target` must match the [`SenderTarget`] the responder used in
    /// [`Self::listen`].
    #[allow(clippy::too_many_arguments)]
    pub async fn poll(
        messenger: &MessengerHandle,
        bound_core_node: &str,
        as_instance_id: &str,
        to_target: SenderTarget,
        to_link_id: Option<&str>,
        to_service_name: &str,
        target_core_node: Option<&str>,
        target_instance_id: Option<&str>,
        request_payload: Payload,
        response_timeout: impl Into<Option<Duration>>,
    ) -> Result<Message> {
        let excluded = messenger.excluded_link_ids_for_wildcard(Some(&to_target), to_link_id);
        let response_timeout: Option<Duration> = response_timeout.into();

        let (resolved_core, resolved_inst) = if target_instance_id.is_none() {
            let probe_sender = ServiceWireSender::new(
                bound_core_node,
                as_instance_id,
                target_core_node,
                target_instance_id,
                to_target.clone(),
                to_link_id,
                to_service_name,
                ServiceKind::Service,
            )?
            .with_excluded_link_ids(&excluded)?;
            // Discovery is capped at PROBE_TIMEOUT or the caller's response
            // budget, whichever is shorter; this preserves the user contract
            // that a tight `response_timeout` fails fast against unreachable
            // targets.
            let discovery_timeout = response_timeout
                .map(|t| t.min(PROBE_TIMEOUT))
                .unwrap_or(PROBE_TIMEOUT);
            let (core, inst) =
                discover_producer(messenger, &probe_sender, discovery_timeout).await?;
            (Some(core), Some(inst))
        } else {
            (
                target_core_node.map(str::to_string),
                target_instance_id.map(str::to_string),
            )
        };

        let sender = ServiceWireSender::new(
            bound_core_node,
            as_instance_id,
            resolved_core.as_deref(),
            resolved_inst.as_deref(),
            to_target,
            to_link_id,
            to_service_name,
            ServiceKind::Service,
        )?
        .with_excluded_link_ids(&excluded)?;
        messenger
            .poll_service(&sender, request_payload, response_timeout)
            .await
    }

    /// Sends a lightweight probe to check whether a service is listening at
    /// the targeted link_id. The probe is handled transparently by the
    /// service's request loop; the user handler is never invoked. Returns
    /// `true` if the service responds within [`PROBE_TIMEOUT`], `false` if
    /// unreachable.
    ///
    /// Bypasses `Self::poll`'s discover-then-pin sequence because a probe
    /// IS the discovery step; routing through `poll` would issue two probes
    /// back to back. Calls the raw messenger path directly with the same
    /// wire-sender shape `poll` builds.
    #[allow(clippy::too_many_arguments)]
    pub async fn is_reachable(
        messenger: &MessengerHandle,
        bound_core_node: &str,
        as_instance_id: &str,
        to_target: SenderTarget,
        to_link_id: Option<&str>,
        to_service_name: &str,
        target_core_node: Option<&str>,
        target_instance_id: Option<&str>,
    ) -> Result<bool> {
        let excluded = messenger.excluded_link_ids_for_wildcard(Some(&to_target), to_link_id);
        let sender = ServiceWireSender::new(
            bound_core_node,
            as_instance_id,
            target_core_node,
            target_instance_id,
            to_target,
            to_link_id,
            to_service_name,
            ServiceKind::Service,
        )?
        .with_excluded_link_ids(&excluded)?;
        match messenger
            .poll_service(
                &sender,
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
