use super::discovery::{budget_left_after_discovery, discover_producer};
use super::{MessengerHandle, generate_short_id};
use crate::error::{Error, Result};
use crate::messaging::ProducerRef;
use crate::runtime::{TaskHandle, spawn};
use crate::types::{Message, Payload};
use super::call_check::{CallCheck, ReceivedCall, VerifiedCaller};
use pmi::{
    Failure, IncomingRequest, Messenger, ResponseToken, SenderTarget, ServiceKind,
    ServiceQueryKind, ServiceQueryable, ServiceWireReceiver, ServiceWireSender, TopicMessage,
};
use std::{fmt, sync::Arc};
use tokio::{
    sync::Mutex,
    time::{Duration, Instant},
};
use tracing::warn;

/// What a service handler answers: the response payload, which is a
/// success, or the failure of the request, a failure kind and a message,
/// which the caller receives as an error reply.
pub type ServiceAnswer = std::result::Result<Payload, Failure>;

async fn run_handler<F, Fut>(handler: F, context: ServiceRequestContext) -> ServiceAnswer
where
    F: FnOnce(ServiceRequestContext) -> Fut,
    Fut: std::future::Future<Output = ServiceAnswer>,
{
    let answer = handler(context).await;
    if let Err(failure) = &answer {
        warn!(%failure, "service handler gave an error reply");
    }
    answer
}

async fn deliver_answer(responder: ServiceResponder, answer: ServiceAnswer) -> Result<()> {
    match answer {
        Ok(payload) => responder.respond(payload).await,
        Err(failure) => responder.respond_failure(failure).await,
    }
}

/// Runs `handler` on one request and replies to its caller. A failed reply
/// is logged and swallowed, so a single bad client cannot end the serving
/// loop.
async fn answer_request<F, Fut>(
    handler: F,
    context: ServiceRequestContext,
    responder: ServiceResponder,
) where
    F: FnOnce(ServiceRequestContext) -> Fut,
    Fut: std::future::Future<Output = ServiceAnswer>,
{
    let request_id = context.request_id().to_string();
    let answer = run_handler(handler, context).await;
    if let Err(err) = deliver_answer(responder, answer).await {
        warn!(
            %err,
            %request_id,
            "failed to send service response; dropping request"
        );
    }
}

pub struct ServiceMessenger;

/// Producer scope of a [`ServiceMessenger::poll`] / [`ServiceMessenger::is_reachable`]
/// / [`ServiceMessenger::probe_latency`] call: how much of the producer's
/// `(core_node, instance_id)` wire address the caller pins up front. Maps
/// onto [`ServiceWireSender`]'s two independent target slots; an enum rather
/// than two `Option`s so the invalid "instance without core" half-address is
/// unrepresentable.
#[derive(Debug, Clone, Copy)]
pub enum ServiceTarget<'a> {
    /// Genuine wildcard: any producer whose service root matches answers.
    /// `poll` runs a discover-then-pin sequence so only one producer's user
    /// handler ever sees the request.
    Any,
    /// Scope to producers hosted by this core node, leaving the instance
    /// slot wildcarded. For callers that know which core node must answer
    /// but cannot know the producer's per-boot instance_id, e.g.
    /// `node_stop`, whose service root (node name + tag) is not unique
    /// across daemons. `poll` still discovers, but only among that core
    /// node's producers.
    CoreNode(&'a str),
    /// Full `(core_node, instance_id)` pin: addresses that producer
    /// directly, no discovery probe and no discovery timeout.
    Producer(&'a ProducerRef),
}

impl ServiceTarget<'_> {
    /// Builds the wire sender for this scope, mapping the enum onto
    /// [`ServiceWireSender`]'s two target slots:
    /// `Any` → `(None, None)`, `CoreNode` → `(Some, None)`,
    /// `Producer` → `(Some, Some)`.
    fn wire_sender(
        &self,
        bound_core_node: &str,
        as_instance_id: &str,
        to_target: SenderTarget,
        to_service_name: &str,
    ) -> Result<ServiceWireSender> {
        let pinned = match self {
            ServiceTarget::Producer(producer) => Some(*producer),
            ServiceTarget::Any | ServiceTarget::CoreNode(_) => None,
        };
        let sender = ServiceWireSender::new(
            bound_core_node,
            as_instance_id,
            pinned,
            to_target,
            to_service_name,
            ServiceKind::Service,
        )?;
        match self {
            ServiceTarget::CoreNode(core_node) => Ok(sender.scoped_to_core_node(core_node)?),
            ServiceTarget::Any | ServiceTarget::Producer(_) => Ok(sender),
        }
    }
}

/// Server-side endpoint for a single service. Wraps the per-link-id queryable
/// fan-in produced by [`pmi::MessengerBackend::listen_service`]: each inbound
/// request carries its own [`ResponseToken`], so responding does not take
/// the central messenger mutex.
///
/// `_messenger` is kept solely to anchor the underlying Zenoh session's
/// lifetime: the queryable's inbound callback (and the flume sender feeding
/// `queryable.rx`) lives in the session's queryable registry, so once every
/// strong reference to the messenger drops the session disappears and the
/// channel closes mid-flight.
///
/// `call_check` is the check of the session that exposed the endpoint: when
/// it has one, the endpoint runs it on each request before any handler reads
/// the request, and answers a refused request with the failure of the check.
pub struct ServiceEndpoint {
    queryable: ServiceQueryable,
    _messenger: Arc<Mutex<Messenger>>,
    call_check: Option<Arc<dyn CallCheck>>,
}

impl ServiceEndpoint {
    pub(crate) fn new(
        messenger: Arc<Mutex<Messenger>>,
        queryable: ServiceQueryable,
        call_check: Option<Arc<dyn CallCheck>>,
    ) -> Self {
        Self {
            queryable,
            _messenger: messenger,
            call_check,
        }
    }

    /// The verdict of the check of the endpoint on `incoming`: `Ok(None)`
    /// when the endpoint has no check.
    fn verify(
        &self,
        incoming: &IncomingRequest,
    ) -> std::result::Result<Option<VerifiedCaller>, Failure> {
        let Some(check) = &self.call_check else {
            return Ok(None);
        };
        let payload = incoming.payload.as_bytes();
        check
            .verify(&ReceivedCall {
                caller_core_node: &incoming.caller_core,
                caller_instance_id: &incoming.caller_inst,
                signed: incoming.signed_call(&payload),
                proof: incoming.proof.as_ref(),
            })
            .map(Some)
    }
}

/// Handle returned by [`ServiceEndpoint::recv_next_request`] that must be used
/// to send the response back to the caller. Wraps the inbound query's
/// [`ResponseToken`]: `respond` issues a single reply on it.
pub struct ServiceResponder {
    token: ResponseToken,
}

impl ServiceResponder {
    /// Send the regular response payload for this request. The reply
    /// carries `ServiceReplyKind::Response` on the attachment; the
    /// payload bytes are opaque to the framework and round-trip
    /// unchanged.
    pub async fn respond(self, payload: Payload) -> Result<()> {
        self.token
            .respond_response(payload.into_inner().into())
            .await
            .map_err(Error::PeppyMessagingInterface)
    }

    /// Send an error reply: `failure` rides in the reply payload and the
    /// attachment is marked `ServiceReplyKind::Failure`; the caller's `poll`
    /// surfaces it as `Error::ServiceFailed { failure, .. }`.
    pub async fn respond_failure(self, failure: Failure) -> Result<()> {
        self.token
            .respond_failure(&failure)
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
            let (context, token) = match self.next_request().await {
                Ok(request) => request,
                Err(Error::ServiceRequestStreamClosed) => return Ok(None),
                Err(err) => return Err(err),
            };
            // ACK reply before invoking the user handler. The caller's poll
            // loop uses this to distinguish ServiceUnreachable (no ACK at
            // all) from ServiceTimeout (ACK but no handler response within
            // the timeout). The ACK kind lives on the reply attachment, so
            // the user payload is never touched.
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
    }

    /// Handles a single incoming request using the provided callback.
    ///
    /// Returns `Ok(true)` after attempting to process a request (even if
    /// sending the response failed: that failure is logged and swallowed so
    /// a single bad client cannot bubble out as a hard error), or `Ok(false)`
    /// when the subscription stream has closed.
    pub async fn handle_next_request<F, Fut>(&mut self, handler: F) -> Result<bool>
    where
        F: FnOnce(ServiceRequestContext) -> Fut,
        Fut: std::future::Future<Output = ServiceAnswer>,
    {
        let Some((context, responder)) = self.recv_next_request().await? else {
            return Ok(false);
        };
        answer_request(handler, context, responder).await;
        Ok(true)
    }

    /// Handles requests until the subscription stream ends. Response send
    /// failures for individual requests are logged and skipped so the loop
    /// keeps serving subsequent callers.
    pub async fn handle_requests<F, Fut>(&mut self, mut handler: F) -> Result<()>
    where
        F: FnMut(ServiceRequestContext) -> Fut,
        Fut: std::future::Future<Output = ServiceAnswer>,
    {
        while let Some((context, responder)) = self.recv_next_request().await? {
            answer_request(&mut handler, context, responder).await;
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
        Fut: std::future::Future<Output = ServiceAnswer> + Send + 'static,
    {
        let Some((context, responder)) = self.recv_next_request().await? else {
            return Ok(None);
        };
        let task = spawn(async move {
            let answer = run_handler(handler, context).await;
            deliver_answer(responder, answer).await
        });
        Ok(Some(task))
    }

    async fn next_request(&mut self) -> Result<(ServiceRequestContext, ResponseToken)> {
        loop {
            let Ok(incoming) = self.queryable.rx.recv_async().await else {
                return Err(Error::ServiceRequestStreamClosed);
            };
            match incoming.kind {
                ServiceQueryKind::Probe => {
                    // Probes are answered by the transport adapter's query
                    // dispatch (see pmi's zenoh/mock adapters) and must never
                    // reach this channel: answering them here would starve
                    // discovery and liveness whenever user code holds this
                    // recv loop. Defensive drop so a misbehaving adapter
                    // can't wedge the request loop in release builds.
                    debug_assert!(
                        false,
                        "ServiceQueryKind::Probe leaked past the adapter dispatch"
                    );
                    warn!(
                        link_id = %incoming.link_id,
                        "dropping probe that leaked past the adapter dispatch"
                    );
                    continue;
                }
                ServiceQueryKind::UserRequest => {
                    let verified_caller = match self.verify(&incoming) {
                        Ok(verified_caller) => verified_caller,
                        Err(refusal) => {
                            warn!(
                                caller_core_node = %incoming.caller_core,
                                caller_instance_id = %incoming.caller_inst,
                                %refusal,
                                "refusing a call whose sender does not prove itself"
                            );
                            if let Err(err) = incoming.token.respond_failure(&refusal).await {
                                warn!(%err, "the refusal of a call did not reach its sender");
                            }
                            continue;
                        }
                    };
                    let topic_message = TopicMessage::from_parts(
                        incoming.caller_core,
                        incoming.caller_inst,
                        incoming.payload,
                    );

                    let request_id = generate_short_id("request");
                    let mut context =
                        ServiceRequestContext::new(topic_message, request_id, incoming.link_id);
                    context.verified_caller = verified_caller;
                    return Ok((context, incoming.token));
                }
            }
        }
    }
}

pub struct ServiceRequestContext {
    message: Message,
    request_id: String,
    /// Producer-side link_id that received this request: whichever bound
    /// link_id's queryable yielded the inbound query. Surfaced so action
    /// goal handlers can scope per-goal feedback under the link_id the
    /// consumer actually targeted.
    link_id: String,
    /// The caller that the check of the endpoint verified; `None` on an
    /// endpoint with no check.
    pub(crate) verified_caller: Option<VerifiedCaller>,
}

impl ServiceRequestContext {
    pub fn new(message: TopicMessage, request_id: String, link_id: String) -> Self {
        Self {
            message: Message(message),
            request_id,
            link_id,
            verified_caller: None,
        }
    }

    /// The caller that the check of the endpoint verified: the sender that
    /// the call proves. `None` on an endpoint whose session has no check,
    /// where the sender is the one that the call gives.
    pub fn verified_caller(&self) -> Option<&VerifiedCaller> {
        self.verified_caller.as_ref()
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
    /// producer by the `(core_node, instance_id)` target derived from the
    /// consumer's binding map.
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
    /// `target` scopes which producer answers.
    /// [`ServiceTarget::Producer`] (a dep slot bound to exactly one
    /// producer, or an infra caller that already knows the full address)
    /// addresses that producer directly:
    /// **no discovery probe is issued and no discovery timeout applies**;
    /// the call has the caller's whole `response_timeout` to itself.
    /// [`ServiceTarget::Any`] is a genuine wildcard (core-node infra
    /// calls only; generated dep-slot call sites always pin): a
    /// discover-then-pin sequence sends a lightweight probe to identify a
    /// single responding producer, then delivers the real request pinned
    /// to it. The probe is answered by the transport adapter before the
    /// user handler runs, so non-winning producers never see the request.
    /// [`ServiceTarget::CoreNode`] discovers the same way, but the probe's
    /// selector is scoped to that core node, so producers hosted elsewhere
    /// can never win the pin even when their service root matches.
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
        target: ServiceTarget<'_>,
        request_payload: Payload,
        response_timeout: impl Into<Option<Duration>>,
    ) -> Result<Message> {
        let response_timeout: Option<Duration> = response_timeout.into();

        let started_at = Instant::now();
        let resolved: ProducerRef = match target {
            ServiceTarget::Producer(producer) => producer.clone(),
            ServiceTarget::Any | ServiceTarget::CoreNode(_) => {
                let probe_sender = target.wire_sender(
                    bound_core_node,
                    as_instance_id,
                    to_target.clone(),
                    to_service_name,
                )?;
                discover_producer(messenger, &probe_sender, response_timeout).await?
            }
        };

        let remaining_budget = response_timeout
            .map(|total| budget_left_after_discovery(total, started_at, &resolved, to_service_name))
            .transpose()?;

        let sender = ServiceTarget::Producer(&resolved).wire_sender(
            bound_core_node,
            as_instance_id,
            to_target,
            to_service_name,
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

    /// Sends a lightweight probe to check whether a service is listening
    /// within the [`ServiceTarget`] scope (a full producer pin, one core
    /// node, or any matching producer). The probe is answered by the
    /// transport adapter; the user handler is never invoked. Returns `true`
    /// if the service replies within [`PROBE_TIMEOUT`](super::PROBE_TIMEOUT)
    /// or is reached but too slow to answer in time (a probe timeout still
    /// proves the producer is there); `false` only if the service is
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
        target: ServiceTarget<'_>,
    ) -> Result<bool> {
        let sender =
            target.wire_sender(bound_core_node, as_instance_id, to_target, to_service_name)?;
        super::discovery::probe_reachable(messenger, &sender).await
    }

    /// Measure the round-trip latency of a single `Probe`-kind query to a
    /// service: caller → router → producer's queryable → the framework's probe
    /// reply → back. Like [`Self::is_reachable`], the probe is auto-handled by
    /// the service request loop and the **user handler is never invoked**, so
    /// this measures only the messaging/routing path, not handler execution. The
    /// result is therefore clock-independent (a single-clock round-trip).
    ///
    /// `request_size`/`response_size` make the probe carry a real-payload-sized
    /// body and ask the producer to reply with `response_size` bytes, so the
    /// round-trip reflects serializing+moving real-sized messages rather than an
    /// empty sentinel, still without running the handler. Pass `0`/`0` for an
    /// empty probe.
    ///
    /// Returns `(elapsed, response_bytes_received)` on a clean reply, where
    /// `response_bytes_received` is the actual reply payload length (lets the
    /// caller detect a producer that did not honor `response_size`). Propagates
    /// the error otherwise (an unreachable producer or a probe that did not
    /// return within `response_timeout` is not a usable latency sample, and the
    /// caller should drop it rather than record the timeout as latency).
    #[allow(clippy::too_many_arguments)]
    pub async fn probe_latency(
        messenger: &MessengerHandle,
        bound_core_node: &str,
        as_instance_id: &str,
        to_target: SenderTarget,
        to_service_name: &str,
        target: ServiceTarget<'_>,
        response_timeout: Duration,
        request_size: usize,
        response_size: u32,
    ) -> Result<(Duration, usize)> {
        let sender =
            target.wire_sender(bound_core_node, as_instance_id, to_target, to_service_name)?;
        super::discovery::probe_round_trip(
            messenger,
            &sender,
            request_size,
            response_size,
            response_timeout,
        )
        .await
    }
}
