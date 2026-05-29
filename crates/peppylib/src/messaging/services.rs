use super::discovery::discover_producer;
use super::{MessengerHandle, PROBE_TIMEOUT, generate_short_id};
use crate::error::{Error, Result};
use crate::runtime::{TaskHandle, spawn};
use crate::types::{Message, Payload};
use pmi::{
    Messenger, ResponseToken, SenderTarget, ServiceKind, ServiceQueryKind, ServiceQueryable,
    ServiceWireReceiver, ServiceWireSender, TopicMessage,
};
use std::{fmt, sync::Arc, time::Instant};
use tokio::{sync::Mutex, time::Duration};
use tracing::{error, warn};

/// Outcome of running a user service handler — either a payload to surface
/// to the caller as a normal response, or a UTF-8 reason that the
/// framework wraps as `Error::ServiceError`. Splitting the outcome at this
/// layer lets the producer reply with the right `ServiceReplyKind` on the
/// attachment instead of smuggling a sentinel through the payload.
enum HandlerOutcome {
    Response(Payload),
    HandlerError(String),
}

async fn run_handler<F, Fut>(handler: F, context: ServiceRequestContext) -> HandlerOutcome
where
    F: FnOnce(ServiceRequestContext) -> Fut,
    Fut: std::future::Future<Output = Result<Payload>>,
{
    match handler(context).await {
        Ok(payload) => HandlerOutcome::Response(payload),
        Err(err) => {
            let reason = err.to_string();
            error!(%reason, "service handler returned error");
            HandlerOutcome::HandlerError(reason)
        }
    }
}

async fn deliver_outcome(responder: ServiceResponder, outcome: HandlerOutcome) -> Result<()> {
    match outcome {
        HandlerOutcome::Response(payload) => responder.respond(payload).await,
        HandlerOutcome::HandlerError(reason) => responder.respond_error(reason).await,
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
    /// Send the regular response payload for this request. The reply
    /// carries `ServiceReplyKind::Response` on the attachment; the
    /// payload bytes are opaque to the framework and round-trip
    /// unchanged — including the legacy byte-prefix patterns that the
    /// previous protocol used as sentinels.
    pub async fn respond(self, payload: Payload) -> Result<()> {
        self.token
            .respond_response(payload.into_inner().into())
            .await
            .map_err(Error::PeppyMessagingInterface)
    }

    /// Send a handler-error reply. `reason` rides in the reply payload
    /// as UTF-8 and the attachment is marked
    /// `ServiceReplyKind::HandlerError`; the caller's `poll` surfaces
    /// the reason as `Error::ServiceError { reason, .. }`.
    pub async fn respond_error(self, reason: String) -> Result<()> {
        self.token
            .respond_handler_error(reason)
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
                    // response within the timeout). The ACK kind lives on
                    // the reply attachment — the user payload is never
                    // touched.
                    if let Err(err) = token.respond_ack().await {
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
        let outcome = run_handler(handler, context).await;
        if let Err(err) = deliver_outcome(responder, outcome).await {
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
            let outcome = run_handler(&mut handler, context).await;
            if let Err(err) = deliver_outcome(responder, outcome).await {
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
        let task = spawn(async move {
            let outcome = run_handler(handler, context).await;
            deliver_outcome(responder, outcome).await
        });
        Ok(Some(task))
    }

    async fn next_request(&mut self) -> Result<(ServiceRequestContext, ResponseToken)> {
        loop {
            match self.queryable.rx.recv_async().await {
                Ok(incoming) => {
                    match incoming.kind {
                        ServiceQueryKind::Probe => {
                            // Auto-handle probes: reply with `Response` kind
                            // and an empty payload, never invoking the user
                            // handler. **Critical**: probes do NOT get an
                            // ACK — the consumer's poll loop pins the
                            // responder's identity off the first non-Ack
                            // reply, so an Ack-kind probe reply would
                            // deadlock the wildcard discover-then-pin flow.
                            if let Err(err) = incoming
                                .token
                                .respond_response(bytes::Bytes::new().into())
                                .await
                            {
                                warn!(%err, "failed to publish probe response");
                            }
                            continue;
                        }
                        ServiceQueryKind::UserRequest => {
                            let topic_message = TopicMessage::from_parts(
                                incoming.caller_core,
                                incoming.caller_inst,
                                incoming.payload,
                            );

                            let request_id = generate_short_id("request");
                            let context = ServiceRequestContext::new(
                                topic_message,
                                request_id,
                                incoming.link_id,
                            );
                            return Ok((context, incoming.token));
                        }
                    }
                }
                Err(_) => return Err(Error::ServiceRequestStreamClosed),
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
    /// Listen as a service. The producer declares one queryable under the
    /// reserved default `_` link_id segment; consumers pin a specific
    /// producer by `target_instance_id` derived from the consumer's
    /// binding map.
    ///
    /// `as_identity` must match the [`SenderTarget`] callers will use in
    /// [`Self::poll`].
    pub async fn listen(
        messenger: &MessengerHandle,
        as_core_node: &str,
        as_instance_id: &str,
        as_identity: SenderTarget,
        as_service_name: &str,
    ) -> Result<ServiceEndpoint> {
        let recv = ServiceWireReceiver::new(
            as_core_node,
            as_instance_id,
            as_identity,
            as_service_name,
            ServiceKind::Service,
        )?;
        messenger.expose_service(&recv).await
    }

    /// Poll a service. The link_id wire slot is always emitted as `*`;
    /// producers advertise under the reserved `_` segment and Zenoh's
    /// matcher unifies the two.
    ///
    /// When either `target_core_node` or `target_instance_id` is `None`
    /// (wildcard / from_any), this performs a discover-then-pin sequence:
    /// a lightweight probe is sent to identify a single responding
    /// producer's `(core_node, instance_id)`, then the real request is
    /// delivered pinned to that producer. The probe is filtered
    /// server-side before the user handler runs, so non-winning producers
    /// never see the request. This costs one extra round-trip; fully
    /// pinned callers (both `target_*` `Some`) skip discovery and pay no
    /// overhead.
    ///
    /// `to_target` must match the [`SenderTarget`] the responder used in
    /// [`Self::listen`].
    #[allow(clippy::too_many_arguments)]
    pub async fn poll(
        messenger: &MessengerHandle,
        bound_core_node: &str,
        as_instance_id: &str,
        to_target: SenderTarget,
        to_service_name: &str,
        target_core_node: Option<&str>,
        target_instance_id: Option<&str>,
        request_payload: Payload,
        response_timeout: impl Into<Option<Duration>>,
    ) -> Result<Message> {
        let response_timeout: Option<Duration> = response_timeout.into();

        let started_at = Instant::now();
        let (resolved_core, resolved_inst) =
            if target_instance_id.is_none() || target_core_node.is_none() {
                let probe_sender = ServiceWireSender::new(
                    bound_core_node,
                    as_instance_id,
                    target_core_node,
                    target_instance_id,
                    to_target.clone(),
                    to_service_name,
                    ServiceKind::Service,
                )?;
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

        // Discovery counts against the caller's single end-to-end budget;
        // pass only the remaining slice to `poll_service` so a tight
        // `response_timeout` can't be silently doubled by a slow probe.
        let elapsed = started_at.elapsed();
        let remaining_budget = match response_timeout {
            Some(total) => {
                let remaining = total.saturating_sub(elapsed);
                if remaining.is_zero() {
                    return Err(Error::ServiceTimeout {
                        instance_id: resolved_inst.clone(),
                        service_name: to_service_name.to_string(),
                    });
                }
                Some(remaining)
            }
            None => None,
        };

        let sender = ServiceWireSender::new(
            bound_core_node,
            as_instance_id,
            resolved_core.as_deref(),
            resolved_inst.as_deref(),
            to_target,
            to_service_name,
            ServiceKind::Service,
        )?;
        messenger
            .poll_service(
                &sender,
                request_payload,
                ServiceQueryKind::UserRequest,
                remaining_budget,
            )
            .await
    }

    /// Sends a lightweight probe to check whether a service is listening at
    /// the targeted producer. The probe is handled transparently by the
    /// service's request loop; the user handler is never invoked. Returns
    /// `true` if the service responds within [`PROBE_TIMEOUT`], `false` if
    /// unreachable.
    ///
    /// Bypasses `Self::poll`'s discover-then-pin sequence because a probe
    /// IS the discovery step; routing through `poll` would issue two probes
    /// back to back. Calls the raw messenger path directly with the same
    /// wire-sender shape `poll` builds.
    pub async fn is_reachable(
        messenger: &MessengerHandle,
        bound_core_node: &str,
        as_instance_id: &str,
        to_target: SenderTarget,
        to_service_name: &str,
        target_core_node: Option<&str>,
        target_instance_id: Option<&str>,
    ) -> Result<bool> {
        let sender = ServiceWireSender::new(
            bound_core_node,
            as_instance_id,
            target_core_node,
            target_instance_id,
            to_target,
            to_service_name,
            ServiceKind::Service,
        )?;
        match messenger
            .poll_service(
                &sender,
                Payload::new(),
                ServiceQueryKind::Probe,
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
