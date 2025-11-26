#[cfg(all(test, feature = "zenoh"))]
mod tests;

use crate::error::{Error, Result};
use bytes::Bytes;
use config::node::QoSProfile;
use pmi::{
    Message, Messenger, MessengerAdapter, MessengerBackend, PeppyMessagingInterfaceError,
    PublisherQoS, ReceivedMessage, SenderMessage, SubscriberQoS, ZenohAdapter, ZenohNetProtocol,
};
use sha2::{Digest, Sha256};
use std::{
    fmt,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};
use tokio::{
    sync::Mutex,
    task::JoinHandle,
    time::{Duration, timeout},
};
use tracing::error;

pub use pmi::Subscription;

const INSTANCE_ID_WILDCARD: &str = "**";

pub struct MessengerHandle {
    messenger: Arc<Mutex<Messenger>>,
}

fn map_node_qos_to_publisher_qos(qos: QoSProfile) -> PublisherQoS {
    match qos {
        QoSProfile::Standard => PublisherQoS::Standard,
        QoSProfile::Reliable => PublisherQoS::Important,
        QoSProfile::SensorData => PublisherQoS::BestEffort,
        QoSProfile::Critical => PublisherQoS::Critical,
    }
}

fn map_node_qos_to_subscriber_qos(qos: QoSProfile) -> SubscriberQoS {
    match qos {
        QoSProfile::SensorData => SubscriberQoS::HighThroughput,
        _ => SubscriberQoS::Standard,
    }
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

pub fn build_key_expr(message_type: &str, node_name: &str, message_type_name: &str) -> String {
    [message_type, node_name, message_type_name]
        .into_iter()
        .flat_map(|part| part.split('/'))
        .filter(|segment| !segment.is_empty())
        .collect::<Vec<_>>()
        .join("/")
}

fn format_instance_segment(instance_id: &str) -> Option<String> {
    (instance_id != INSTANCE_ID_WILDCARD).then(|| format!("<INSTANCE_ID:{instance_id}>"))
}

pub struct TopicMessenger;

pub struct ServiceMessenger;

pub struct ActionMessenger;

pub struct ServiceEndpoint {
    messenger: Arc<Mutex<Messenger>>,
    subscription: Subscription,
    service_root: String,
    instance_id: String,
    response_topic_base: String,
}

impl ServiceEndpoint {
    /// We need to use a callback system here to force the service to send back a response
    pub async fn handle_next_request<F, Fut>(&mut self, handler: F) -> Result<bool>
    where
        F: FnOnce(ServiceRequestContext) -> Fut,
        Fut: std::future::Future<Output = Result<Bytes>>,
    {
        match self.next_request().await {
            Ok((context, response_topic)) => {
                let response_payload = handler(context).await?;
                self.publish_response(response_topic, response_payload)
                    .await?;
                Ok(true)
            }
            Err(Error::ServiceRequestStreamClosed) => Ok(false),
            Err(err) => Err(err),
        }
    }

    /// Handles requests until the subscription stream ends.
    pub async fn handle_requests<F, Fut>(&mut self, mut handler: F) -> Result<()>
    where
        F: FnMut(ServiceRequestContext) -> Fut,
        Fut: std::future::Future<Output = Result<Bytes>>,
    {
        loop {
            let (context, response_topic) = match self.next_request().await {
                Ok(value) => value,
                Err(Error::ServiceRequestStreamClosed) => break,
                Err(err) => return Err(err),
            };
            let response_payload = handler(context).await?;
            self.publish_response(response_topic, response_payload)
                .await?;
        }

        Ok(())
    }

    /// Spawns the handler on its own task so multiple requests can progress concurrently.
    /// Returns `Ok(None)` when the subscription closes before yielding a request.
    pub async fn spawn_next_request_handler<F, Fut>(
        &mut self,
        handler: F,
    ) -> Result<Option<JoinHandle<Result<()>>>>
    where
        F: FnOnce(ServiceRequestContext) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = Result<Bytes>> + Send + 'static,
    {
        let (context, response_topic) = match self.next_request().await {
            Ok(value) => value,
            Err(Error::ServiceRequestStreamClosed) => return Ok(None),
            Err(err) => return Err(err),
        };

        let messenger = Arc::clone(&self.messenger);
        let task = tokio::spawn(async move {
            let response_payload = handler(context).await?;
            ServiceEndpoint::publish_response_with_messenger(
                messenger,
                response_topic,
                response_payload,
            )
            .await
        });

        Ok(Some(task))
    }

    async fn next_request(&mut self) -> Result<(ServiceRequestContext, String)> {
        while let Some(request) = self.subscription.rx.recv().await {
            let (context, response_topic) = self.build_request_context(request)?;
            return Ok((context, response_topic));
        }

        Err(Error::ServiceRequestStreamClosed)
    }

    fn build_request_context(
        &self,
        request: ReceivedMessage,
    ) -> Result<(ServiceRequestContext, String)> {
        let identifier = request.key_expr().to_string();
        let service_instance_segment = format_instance_segment(self.instance_id.as_str());
        let response_instance_segment = service_instance_segment
            .clone()
            .unwrap_or_else(|| format!("<INSTANCE_ID:{}>", self.instance_id));

        let specific_prefix = service_instance_segment
            .as_ref()
            .map(|instance_segment| format!("{}/{instance_segment}/request/", self.service_root));
        let direct_prefix = format!("{}/request/", self.service_root);
        let wildcard_prefix = format!("{}/**/request/", self.service_root);
        let expected_prefixes = {
            let mut prefixes = Vec::new();
            if let Some(prefix) = specific_prefix.as_ref() {
                prefixes.push(prefix.as_str());
            }
            prefixes.push(direct_prefix.as_str());
            prefixes.push(wildcard_prefix.as_str());
            prefixes.join(" or ")
        };

        let matched_prefix = specific_prefix
            .as_ref()
            .filter(|prefix| identifier.starts_with(*prefix))
            .map(String::as_str)
            .or_else(|| {
                if identifier.starts_with(&direct_prefix) {
                    Some(direct_prefix.as_str())
                } else {
                    None
                }
            })
            .or_else(|| {
                if identifier.starts_with(&wildcard_prefix) {
                    Some(wildcard_prefix.as_str())
                } else {
                    None
                }
            });

        let request_prefix = matched_prefix.ok_or_else(|| {
            let reason =
                format!("unexpected request topic; expected to start with {expected_prefixes}");
            error!(%identifier, %reason, "service received invalid request");
            Error::InvalidServiceRequest {
                identifier: identifier.clone(),
                reason,
            }
        })?;

        let remainder = identifier.strip_prefix(&request_prefix).ok_or_else(|| {
            let reason =
                "request topic is missing the expected prefix after validation".to_string();
            error!(%identifier, %reason, "service received invalid request");
            Error::InvalidServiceRequest {
                identifier: identifier.clone(),
                reason,
            }
        })?;
        let mut remainder_parts = remainder.split('/').filter(|segment| !segment.is_empty());

        let caller_segment = match remainder_parts.next().map(str::to_string) {
            Some(segment) => segment,
            None => {
                error!(%identifier, "service received request without caller instance segment");
                return Err(Error::InvalidServiceRequest {
                    identifier,
                    reason: "missing caller instance segment".to_string(),
                });
            }
        };
        let request_id = remainder_parts
            .next()
            .filter(|segment| !segment.is_empty())
            .map(str::to_string);

        let response_topic = match request_id.as_ref() {
            Some(id) => format!("{}/{}/{}", self.response_topic_base, caller_segment, id),
            None => self.response_topic_base.clone(),
        };
        let response_topic = format!("{response_topic}/{response_instance_segment}");

        let message_identifier = match service_instance_segment.as_deref() {
            Some(instance_segment) => {
                format!(
                    "{}/{instance_segment}/request/{caller_segment}",
                    self.service_root
                )
            }
            None => format!("{}/**/request/{caller_segment}", self.service_root),
        };
        let message = SenderMessage::new(&message_identifier, request.into_payload())?;
        let context = ServiceRequestContext::new(message, request_id);

        Ok((context, response_topic))
    }

    async fn publish_response(&self, topic: String, payload: Bytes) -> Result<()> {
        Self::publish_response_with_messenger(Arc::clone(&self.messenger), topic, payload).await
    }

    async fn publish_response_with_messenger(
        messenger: Arc<Mutex<Messenger>>,
        topic: String,
        payload: Bytes,
    ) -> Result<()> {
        let response = Message::new(&topic, payload);
        let mut messenger = messenger.lock().await;
        messenger
            .publish(response, PublisherQoS::Standard)
            .await
            .map_err(Error::PeppyMessagingInterface)
    }
}

pub struct ServiceRequestContext {
    message: SenderMessage,
    request_id: Option<String>,
}

impl ServiceRequestContext {
    pub fn new(message: SenderMessage, request_id: Option<String>) -> Self {
        Self {
            message,
            request_id,
        }
    }

    pub fn message(&self) -> &SenderMessage {
        &self.message
    }

    pub fn request_id(&self) -> Option<&str> {
        self.request_id.as_deref()
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

pub struct TopicPublisher {
    messenger: Arc<Mutex<Messenger>>,
    topic: String,
    qos: PublisherQoS,
}

impl TopicPublisher {
    pub fn topic(&self) -> &str {
        &self.topic
    }

    pub async fn publish(&self, payload: Bytes) -> Result<()> {
        let message = Message::new(&self.topic, payload);
        let mut messenger = self.messenger.lock().await;
        messenger
            .publish(message, self.qos)
            .await
            .map_err(Error::PeppyMessagingInterface)
    }
}

pub struct ActionGoalHandle {
    node_name: String,
    action_name: String,
    target_instance_id: Option<String>,
    goal_response: ReceivedMessage,
    feedback: Subscription,
}

impl ActionGoalHandle {
    pub fn goal_response(&self) -> &ReceivedMessage {
        &self.goal_response
    }

    pub fn feedback_mut(&mut self) -> &mut Subscription {
        &mut self.feedback
    }
}

// https://docs.ros.org/en/foxy/_images/Action-SingleActionClient.gif
pub struct ActionCreation {
    pub goal_service: ServiceEndpoint,
    pub cancel_service: ServiceEndpoint,
    pub feedback_publisher: TopicPublisher,
    pub result_service: ServiceEndpoint,
}

impl TopicMessenger {
    pub async fn listen(
        messenger: &MessengerHandle,
        as_node_name: &str,
        as_topic: &str,
        qos: QoSProfile,
    ) -> Result<Subscription> {
        messenger
            .receive_topic_msg(as_node_name, as_topic, qos)
            .await
    }

    pub async fn emit(
        messenger: &MessengerHandle,
        to_node_name: &str,
        to_topic: &str,
        as_instance_id: &str,
        qos: QoSProfile,
        payload: Bytes,
    ) -> Result<()> {
        messenger
            .emit_topic_message(to_node_name, to_topic, as_instance_id, qos, payload)
            .await
    }
}

impl ServiceMessenger {
    /// Listening as a service is a 2 way stream, so the process that exposes the service needs to provide its instance_id
    pub async fn listen(
        messenger: &MessengerHandle,
        as_node_name: &str,
        as_service_name: &str,
        as_instance_id: &str,
    ) -> Result<ServiceEndpoint> {
        messenger
            .expose_service(as_node_name, as_service_name, as_instance_id)
            .await
    }

    pub async fn poll(
        messenger: &MessengerHandle,
        as_instance_id: &str,
        node_name: &str,
        service_name: &str,
        instance_id: Option<&str>,
        request_payload: Bytes,
        response_timeout: Duration,
    ) -> Result<ReceivedMessage> {
        messenger
            .poll_service(
                "service",
                node_name,
                service_name,
                instance_id,
                as_instance_id,
                request_payload,
                response_timeout,
            )
            .await
    }
}

impl ActionMessenger {
    pub async fn listen(
        messenger: &MessengerHandle,
        as_node_name: &str,
        as_action_name: &str,
        as_instance_id: &str,
    ) -> Result<ActionCreation> {
        messenger
            .expose_action(as_node_name, as_action_name, as_instance_id)
            .await
    }

    pub async fn send_goal(
        messenger: &MessengerHandle,
        as_instance_id: &str,
        node_name: &str,
        action_name: &str,
        instance_id: Option<&str>,
        goal_payload: Bytes,
        feedback_qos: QoSProfile,
        goal_timeout: Duration,
    ) -> Result<ActionGoalHandle> {
        let action_root = build_key_expr("action", node_name, action_name);
        let feedback_topic = match instance_id {
            Some(target_instance_id) => {
                format!("{action_root}/feedback/<INSTANCE_ID:{target_instance_id}>")
            }
            None => format!("{action_root}/feedback/**"),
        };
        let goal_service_name = format!("{action_name}/goal");

        let feedback_subscription = {
            let subscriber_qos = map_node_qos_to_subscriber_qos(feedback_qos);
            let messenger = messenger.messenger.lock().await;
            messenger.subscribe(&feedback_topic, subscriber_qos).await
        }
        .map_err(Error::PeppyMessagingInterface)?;

        let goal_response = messenger
            .poll_service(
                "action",
                node_name,
                &goal_service_name,
                instance_id,
                as_instance_id,
                goal_payload,
                goal_timeout,
            )
            .await?;

        Ok(ActionGoalHandle {
            node_name: node_name.to_string(),
            action_name: action_name.to_string(),
            target_instance_id: instance_id.map(|id| id.to_string()),
            goal_response,
            feedback: feedback_subscription,
        })
    }

    pub async fn cancel_goal(
        messenger: &MessengerHandle,
        as_instance_id: &str,
        handle: &ActionGoalHandle,
        cancel_timeout: Duration,
    ) -> Result<ReceivedMessage> {
        let cancel_service_name = format!("{}/cancel", handle.action_name);

        messenger
            .poll_service(
                "action",
                &handle.node_name,
                &cancel_service_name,
                handle.target_instance_id.as_deref(),
                as_instance_id,
                Bytes::new(),
                cancel_timeout,
            )
            .await
    }

    pub async fn poll_result(
        messenger: &MessengerHandle,
        as_instance_id: &str,
        handle: &ActionGoalHandle,
        result_request_payload: Bytes,
        result_timeout: Duration,
    ) -> Result<ReceivedMessage> {
        let result_service_name = format!("{}/result", handle.action_name);

        messenger
            .poll_service(
                "action",
                &handle.node_name,
                &result_service_name,
                handle.target_instance_id.as_deref(),
                as_instance_id,
                result_request_payload,
                result_timeout,
            )
            .await
            .map_err(|err| match err {
                Error::ServiceTimeout { instance_id, .. } => Error::ActionResultTimeout {
                    instance_id,
                    action_name: handle.action_name.clone(),
                },
                Error::ServiceUnreachable { instance_id, .. } => Error::ActionResultUnreachable {
                    instance_id,
                    action_name: handle.action_name.clone(),
                },
                other => other,
            })
    }
}

impl MessengerHandle {
    pub fn from_shared(messenger: Arc<Mutex<Messenger>>) -> Self {
        Self { messenger }
    }

    pub async fn new() -> Result<Self> {
        let adapter = ZenohAdapter::default();
        Self::from_adapter(adapter).await
    }

    pub async fn from_host_port(host: &str, port: u16) -> Result<Self> {
        let adapter = ZenohAdapter::from_host_port(ZenohNetProtocol::Tcp, host, port);
        Self::from_adapter(adapter).await
    }

    async fn from_adapter(adapter: ZenohAdapter) -> Result<Self> {
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

    async fn receive_topic_msg(
        &self,
        from_node_name: &str,
        from_topic: &str,
        qos: QoSProfile,
    ) -> Result<Subscription> {
        let key_expr = format!("topic/{}/{}/**", from_node_name, from_topic);
        let subscriber_qos = map_node_qos_to_subscriber_qos(qos);

        let subscription = {
            let messenger = self.messenger.lock().await;
            messenger.subscribe(&key_expr, subscriber_qos).await
        }
        .map_err(Error::PeppyMessagingInterface)?;

        Ok(subscription)
    }

    async fn emit_topic_message(
        &self,
        to_node_name: &str,
        to_topic: &str,
        as_instance_id: &str,
        qos: QoSProfile,
        payload: Bytes,
    ) -> Result<()> {
        // Uses the zenoh key expression ID matching to save bytes on sending the node ID on the other side
        let key_expr = format!(
            "topic/{}/{}/<INSTANCE_ID:{}>",
            to_node_name, to_topic, as_instance_id
        );
        let msg = Message::new(&key_expr, payload);

        let publisher_qos = map_node_qos_to_publisher_qos(qos);

        let mut messenger = self.messenger.lock().await;
        messenger
            .publish(msg, publisher_qos)
            .await
            .map_err(Error::PeppyMessagingInterface)
    }

    async fn expose_service(
        &self,
        as_node_name: &str,
        as_service_name: &str,
        as_instance_id: &str,
    ) -> Result<ServiceEndpoint> {
        let key_expr = build_key_expr("service", as_node_name, as_service_name);
        self.create_service_endpoint(key_expr, as_instance_id).await
    }

    async fn create_service_endpoint(
        &self,
        key_expr: String,
        as_instance_id: &str,
    ) -> Result<ServiceEndpoint> {
        let service_root = key_expr;
        let request_subscription_topic = format!("{service_root}/**/request/**");
        let response_topic_base = format!("{service_root}/response");

        let subscription = {
            let messenger = self.messenger.lock().await;
            messenger
                .subscribe(&request_subscription_topic, SubscriberQoS::Standard)
                .await
        }
        .map_err(Error::PeppyMessagingInterface)?;

        Ok(ServiceEndpoint {
            messenger: Arc::clone(&self.messenger),
            subscription,
            service_root,
            instance_id: as_instance_id.to_string(),
            response_topic_base,
        })
    }

    async fn poll_service(
        &self,
        message_type: &str,
        node_name: &str,
        service_name: &str,
        instance_id: Option<&str>,
        as_instance_id: &str,
        request_payload: Bytes,
        response_timeout: Duration,
    ) -> Result<ReceivedMessage> {
        let service_root = build_key_expr(message_type, node_name, service_name);
        let target_instance_segment = instance_id.and_then(format_instance_segment);
        let target_instance_id = instance_id.map(|id| id.to_string());
        let caller_instance_segment = format_instance_segment(as_instance_id)
            .unwrap_or_else(|| INSTANCE_ID_WILDCARD.to_string());

        let request_id = generate_request_id();

        let request_topic = match &target_instance_segment {
            Some(instance_segment) => format!(
                "{service_root}/{instance_segment}/request/{caller_instance_segment}/{request_id}"
            ),
            None => format!("{service_root}/**/request/{caller_instance_segment}/{request_id}"),
        };

        let response_topic = match &target_instance_segment {
            Some(instance_segment) => format!(
                "{service_root}/response/{caller_instance_segment}/{request_id}/{instance_segment}"
            ),
            None => {
                format!("{service_root}/response/{caller_instance_segment}/{request_id}/*")
            }
        };

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

        let response = match timeout(response_timeout, response_subscription.rx.recv()).await {
            Ok(Some(message)) => message,
            Ok(None) => {
                return Err(Error::PeppyMessagingInterface(
                    PeppyMessagingInterfaceError::BackendError(
                        "service response channel closed".to_string(),
                    ),
                ));
            }
            Err(_) => {
                let has_matching_subscribers = {
                    let messenger = self.messenger.lock().await;
                    messenger.has_matching_subscribers(&request_topic).await
                }
                .map_err(Error::PeppyMessagingInterface)?;

                if has_matching_subscribers {
                    return Err(Error::ServiceTimeout {
                        instance_id: target_instance_id,
                        service_name: service_name.to_string(),
                    });
                } else {
                    return Err(Error::ServiceUnreachable {
                        instance_id: target_instance_id,
                        service_name: service_name.to_string(),
                    });
                }
            }
        };

        Ok(response)
    }

    async fn expose_action(
        &self,
        as_node_name: &str,
        as_action_name: &str,
        as_instance_id: &str,
    ) -> Result<ActionCreation> {
        let action_root = build_key_expr("action", as_node_name, as_action_name);

        let goal_service_root = format!("{action_root}/goal");
        let cancel_service_root = format!("{action_root}/cancel");
        let result_service_root = format!("{action_root}/result");
        let feedback_topic = format!("{action_root}/feedback/<INSTANCE_ID:{as_instance_id}>");

        let goal_service = self
            .create_service_endpoint(goal_service_root, as_instance_id)
            .await?;
        let cancel_service = self
            .create_service_endpoint(cancel_service_root, as_instance_id)
            .await?;
        let result_service = self
            .create_service_endpoint(result_service_root, as_instance_id)
            .await?;

        let feedback_publisher = TopicPublisher {
            messenger: Arc::clone(&self.messenger),
            topic: feedback_topic,
            qos: PublisherQoS::Standard,
        };

        Ok(ActionCreation {
            goal_service,
            cancel_service,
            feedback_publisher,
            result_service,
        })
    }
}
