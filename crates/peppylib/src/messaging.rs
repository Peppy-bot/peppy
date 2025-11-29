#[cfg(all(test, feature = "zenoh"))]
mod tests;

use crate::error::{Error, Result};
use bytes::Bytes;
use config::node::QoSProfile;
use pmi::{
    Message, Messenger, MessengerAdapter, MessengerBackend, PeppyMessagingInterfaceError,
    PublisherQoS, SubscriberQoS, Subscription, TopicMessage, ZenohAdapter, ZenohNetProtocol,
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
            match self.build_request_context(request) {
                Ok((context, response_topic)) => return Ok((context, response_topic)),
                Err(Error::InvalidServiceRequest { .. }) => {
                    // Skip messages that do not match this service endpoint.
                    continue;
                }
                Err(err) => return Err(err),
            }
        }

        Err(Error::ServiceRequestStreamClosed)
    }

    fn build_request_context(
        &self,
        request: TopicMessage,
    ) -> Result<(ServiceRequestContext, String)> {
        let identifier = request.key_expr().to_string();
        let (master_segment, identifier_without_master) =
            identifier
                .split_once('/')
                .ok_or_else(|| Error::InvalidServiceRequest {
                    identifier: identifier.clone(),
                    reason: "missing master node segment in request".to_string(),
                })?;
        let service_root_with_master = format!("{master_segment}/{}", self.service_root);
        let service_instance_segment = format_instance_segment(self.instance_id.as_str());
        let response_instance_segment = service_instance_segment
            .clone()
            .unwrap_or_else(|| format!("<INSTANCE_ID:{}>", self.instance_id));

        let specific_prefix = service_instance_segment.as_ref().map(|instance_segment| {
            format!("{}/{instance_segment}/request/", service_root_with_master)
        });
        let direct_prefix = format!("{service_root_with_master}/request/");
        let wildcard_prefix = format!("{service_root_with_master}/**/request/");
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

        let remainder = identifier_without_master
            .strip_prefix(
                request_prefix
                    .strip_prefix(&format!("{master_segment}/"))
                    .unwrap_or(request_prefix),
            )
            .ok_or_else(|| {
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
        let request_id = match remainder_parts
            .next()
            .filter(|segment| !segment.is_empty())
            .map(str::to_string)
        {
            Some(id) => id,
            None => {
                error!(%identifier, "service received request without request id segment");
                return Err(Error::InvalidServiceRequest {
                    identifier,
                    reason: "missing request id segment".to_string(),
                });
            }
        };
        let response_topic_base = format!("{service_root_with_master}/response");
        let response_topic = format!(
            "{}/{}/{}/{}",
            response_topic_base, caller_segment, request_id, response_instance_segment
        );

        let message_identifier = match service_instance_segment.as_deref() {
            Some(instance_segment) => {
                format!(
                    "{}/{instance_segment}/request/{caller_segment}",
                    service_root_with_master
                )
            }
            None => format!("{}/**/request/{caller_segment}", service_root_with_master),
        };
        let message = TopicMessage::new(&message_identifier, request.into_payload())?;
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
    message: TopicMessage,
    request_id: String,
}

impl ServiceRequestContext {
    pub fn new(message: TopicMessage, request_id: String) -> Self {
        Self {
            message,
            request_id,
        }
    }

    pub fn message(&self) -> &TopicMessage {
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
        self.publish_on(self.topic.clone(), payload).await
    }

    pub async fn publish_with_prefix(&self, prefix: &str, payload: Bytes) -> Result<()> {
        let topic = format!("{prefix}/{}", self.topic);
        self.publish_on(topic, payload).await
    }

    async fn publish_on(&self, topic: String, payload: Bytes) -> Result<()> {
        let message = Message::new(&topic, payload);
        let mut messenger = self.messenger.lock().await;
        messenger
            .publish(message, self.qos)
            .await
            .map_err(Error::PeppyMessagingInterface)
    }
}

pub struct ActionGoalHandle {
    master_node: String,
    node_name: String,
    action_name: String,
    target_instance_id: Option<String>,
    goal_response: TopicMessage,
    feedback: Subscription,
}

impl ActionGoalHandle {
    pub fn goal_response(&self) -> &TopicMessage {
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
    pub async fn subscribe(
        messenger: &MessengerHandle,
        as_instance_id: &str,
        to_node_name: &str,
        to_topic: &str,
        qos: QoSProfile,
    ) -> Result<Subscription> {
        messenger
            .subscribe_to_topic(as_instance_id, to_node_name, to_topic, qos)
            .await
    }

    /// Publishes a payload to a topic on the specified master node.
    ///
    /// Arguments:
    /// - `messenger`: shared messenger handle used to publish.
    /// - `bound_master_node`: master node segment to scope the topic under.
    /// - `as_node`: name of the node emitting the message.
    /// - `as_topic`: topic name to publish to.
    /// - `as_instance_id`: instance identifier for the emitting node
    /// - `qos`: QoS profile that is mapped to the publisher QoS.
    /// - `payload`: message body to send.
    pub async fn emit(
        messenger: &MessengerHandle,
        bound_master_node: &str,
        as_node: &str,
        as_topic: &str,
        as_instance_id: &str,
        to_instance_id: Option<&str>,
        qos: QoSProfile,
        payload: Bytes,
    ) -> Result<()> {
        messenger
            .emit_topic_message(
                bound_master_node,
                as_node,
                as_topic,
                as_instance_id,
                to_instance_id,
                qos,
                payload,
            )
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

    /// If `target_instance_id` is `None`, this call returns with the first service instance that it hits
    pub async fn poll(
        messenger: &MessengerHandle,
        bound_master_node: &str,
        as_instance_id: &str,
        target_node_name: &str,
        target_service_name: &str,
        target_instance_id: Option<&str>,
        request_payload: Bytes,
        response_timeout: Duration,
    ) -> Result<TopicMessage> {
        messenger
            .poll_service(
                "service",
                bound_master_node,
                target_node_name,
                target_service_name,
                as_instance_id,
                target_instance_id,
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
        master_node: &str,
        as_instance_id: &str,
        node_name: &str,
        action_name: &str,
        target_instance_id: Option<&str>,
        goal_payload: Bytes,
        feedback_qos: QoSProfile,
        goal_timeout: Duration,
    ) -> Result<ActionGoalHandle> {
        let action_root = format!("{}/action/{}/{}", master_node, node_name, action_name);
        let feedback_topic = match target_instance_id {
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
                master_node,
                node_name,
                &goal_service_name,
                as_instance_id,
                target_instance_id,
                goal_payload,
                goal_timeout,
            )
            .await?;

        Ok(ActionGoalHandle {
            master_node: master_node.to_string(),
            node_name: node_name.to_string(),
            action_name: action_name.to_string(),
            target_instance_id: target_instance_id.map(|id| id.to_string()),
            goal_response,
            feedback: feedback_subscription,
        })
    }

    pub async fn cancel_goal(
        messenger: &MessengerHandle,
        as_instance_id: &str,
        handle: &ActionGoalHandle,
        cancel_timeout: Duration,
    ) -> Result<TopicMessage> {
        let cancel_service_name = format!("{}/cancel", handle.action_name);

        messenger
            .poll_service(
                "action",
                &handle.master_node,
                &handle.node_name,
                &cancel_service_name,
                as_instance_id,
                handle.target_instance_id.as_deref(),
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
    ) -> Result<TopicMessage> {
        let result_service_name = format!("{}/result", handle.action_name);

        messenger
            .poll_service(
                "action",
                &handle.master_node,
                &handle.node_name,
                &result_service_name,
                as_instance_id,
                handle.target_instance_id.as_deref(),
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

    pub async fn from_host_port(host: &str, port: u16) -> Result<Self> {
        let adapter = ZenohAdapter::from_host_port(ZenohNetProtocol::Tcp, host, port);
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
        as_instance_id: &str,
        to_node_name: &str,
        to_topic: &str,
        qos: QoSProfile,
    ) -> Result<Subscription> {
        // Key is: `master_node_name/instance_id/topic/to_node_name/to_topic/received_instance_id`
        let key_expr = format!("*/{as_instance_id}/topic/{to_node_name}/{to_topic}/*");
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
        bound_master_node: &str,
        as_node_name: &str,
        as_topic: &str,
        as_instance_id: &str,
        to_instance_id: Option<&str>,
        qos: QoSProfile,
        payload: Bytes,
    ) -> Result<()> {
        // Uses the zenoh key expression ID matching to save bytes on sending the node ID on the other side
        // key_expr is of the form `<MASTER_NODE:bound_master_node>/to_instance_id/topic/as_node_name/as_topic/<INSTANCE_ID:as_instance_id>`.
        // where `to_instance_id` is `*` when the value is `None`
        let to_instance_id = match to_instance_id {
            Some(instance_id) => instance_id,
            None => "*",
        };
        let key_expr = format!(
            "<MASTER_NODE:{}>/{}/topic/{}/{}/<INSTANCE_ID:{}>",
            bound_master_node, to_instance_id, as_node_name, as_topic, as_instance_id
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
        let key_expr = format!("service/{}/{}", as_node_name, as_service_name);
        self.create_service_endpoint(key_expr, as_instance_id).await
    }

    async fn create_service_endpoint(
        &self,
        key_expr: String,
        as_instance_id: &str,
    ) -> Result<ServiceEndpoint> {
        let service_root = key_expr;
        let request_subscription_topic = format!("**/{service_root}/**/request/**");

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
        })
    }

    async fn poll_service(
        &self,
        message_type: &str,
        bound_master_node: &str,
        target_node_name: &str,
        target_service_name: &str,
        as_instance_id: &str,
        target_instance_id: Option<&str>,
        request_payload: Bytes,
        response_timeout: Duration,
    ) -> Result<TopicMessage> {
        let service_root = format!(
            "{}/{}/{}/{}",
            bound_master_node, message_type, target_node_name, target_service_name
        );
        let caller_instance_segment = format_instance_segment(as_instance_id)
            .unwrap_or_else(|| INSTANCE_ID_WILDCARD.to_string());

        let target_instance_id = target_instance_id.map(str::to_string);
        let target_instance_segment = target_instance_id
            .as_deref()
            .and_then(format_instance_segment);
        let request_id = generate_request_id();
        let request_topic = match target_instance_segment.as_deref() {
            Some(segment) => {
                format!("{service_root}/{segment}/request/{caller_instance_segment}/{request_id}")
            }
            None => {
                format!("{service_root}/**/request/{caller_instance_segment}/{request_id}")
            }
        };

        let response_topic = match target_instance_segment.as_deref() {
            Some(segment) => {
                format!("{service_root}/response/{caller_instance_segment}/{request_id}/{segment}")
            }
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
                        instance_id: target_instance_id.clone(),
                        service_name: target_service_name.to_string(),
                    });
                } else {
                    return Err(Error::ServiceUnreachable {
                        instance_id: target_instance_id,
                        service_name: target_service_name.to_string(),
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
        let action_root = format!("action/{}/{}", as_node_name, as_action_name);

        let goal_service_root = format!("{action_root}/goal");
        let cancel_service_root = format!("{action_root}/cancel");
        let result_service_root = format!("{action_root}/result");
        let feedback_topic_suffix =
            format!("{action_root}/feedback/<INSTANCE_ID:{as_instance_id}>");

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
            topic: feedback_topic_suffix,
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
