use super::{SERVICE_ACK_PAYLOAD, encode_service_handler_error, is_service_probe_payload};
use crate::error::{Error, Result};
use crate::runtime::{TaskHandle, spawn};
use crate::types::{Message, Payload};
use pmi::{Messenger, MessengerBackend, ServiceWireReceiver, Subscription, TopicMessage};
use std::{fmt, sync::Arc};
use tokio::sync::Mutex;
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

/// Server-side endpoint for a single service. Holds the four broadcast-Cartesian
/// listen subscriptions plus the addressing context needed to validate incoming
/// requests and route responses.
pub struct ServiceEndpoint {
    messenger: Arc<Mutex<Messenger>>,
    /// Subscriptions to service requests. Four patterns are needed to match:
    /// - [0] Requests targeting this specific core node and instance
    /// - [1] Requests targeting this specific core node with broadcast instance
    /// - [2] Broadcast requests (any core node) targeting this specific instance
    /// - [3] Full broadcast requests (any core node, any instance)
    subscriptions: [Subscription; 4],
    receiver: ServiceWireReceiver,
}

impl ServiceEndpoint {
    pub(crate) fn new(
        messenger: Arc<Mutex<Messenger>>,
        subscriptions: [Subscription; 4],
        receiver: ServiceWireReceiver,
    ) -> Self {
        Self {
            messenger,
            subscriptions,
            receiver,
        }
    }
}

/// Handle returned by [`ServiceEndpoint::recv_next_request`] that must be used to send the
/// response back to the caller.
pub struct ServiceResponder {
    messenger: Arc<Mutex<Messenger>>,
    receiver: ServiceWireReceiver,
    received_request: String,
}

impl ServiceResponder {
    /// Send the response payload for this request.
    pub async fn respond(self, payload: Payload) -> Result<()> {
        let mut messenger = self.messenger.lock().await;
        messenger
            .publish_service_response(
                &self.receiver,
                &self.received_request,
                payload.into_inner().into(),
            )
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
            Ok((context, received_request)) => {
                self.publish_ack(&received_request).await?;
                Ok(Some((
                    context,
                    ServiceResponder {
                        messenger: Arc::clone(&self.messenger),
                        receiver: self.receiver.clone(),
                        received_request,
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
            let [sub0, sub1, sub2, sub3] = &mut self.subscriptions;
            let request = tokio::select! {
                msg = sub0.rx.recv() => msg,
                msg = sub1.rx.recv() => msg,
                msg = sub2.rx.recv() => msg,
                msg = sub3.rx.recv() => msg,
            };

            match request {
                Some(request) => {
                    match self.build_request_context(request).await {
                        Ok((context, received_keyexpr)) => {
                            // Auto-handle probes: respond immediately without invoking
                            // the user handler, so is_reachable() checks are transparent.
                            if is_service_probe_payload(context.message().payload().as_ref()) {
                                if let Err(err) = self
                                    .publish_response(&received_keyexpr, Payload::new())
                                    .await
                                {
                                    warn!(
                                        %received_keyexpr,
                                        %err,
                                        "failed to publish probe response"
                                    );
                                }
                                continue;
                            }
                            return Ok((context, received_keyexpr));
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

    async fn build_request_context(
        &self,
        request: TopicMessage,
    ) -> Result<(ServiceRequestContext, String)> {
        let identifier = request.key_expr().to_string();
        let messenger = self.messenger.lock().await;
        let request_id = messenger
            .parse_service_request_id(&self.receiver, &identifier)
            .map_err(|err| Error::InvalidServiceRequest {
                identifier: identifier.clone(),
                reason: err.to_string(),
            })?;
        drop(messenger);

        let context = ServiceRequestContext::new(request, request_id);
        Ok((context, identifier))
    }

    async fn publish_ack(&self, received_request: &str) -> Result<()> {
        let mut messenger = self.messenger.lock().await;
        messenger
            .publish_service_response(
                &self.receiver,
                received_request,
                bytes::Bytes::from_static(SERVICE_ACK_PAYLOAD).into(),
            )
            .await
            .map_err(Error::PeppyMessagingInterface)
    }

    async fn publish_response(&self, received_request: &str, payload: Payload) -> Result<()> {
        let mut messenger = self.messenger.lock().await;
        messenger
            .publish_service_response(
                &self.receiver,
                received_request,
                payload.into_inner().into(),
            )
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
            .field("core_node", &self.message.core_node())
            .field("instance_id", &self.message.instance_id())
            .field("request_id", &self.request_id)
            .finish()
    }
}
