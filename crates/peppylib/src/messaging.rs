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
/// Marker used in key expressions for broadcast requests (targeting any instance)
const BROADCAST_MARKER: &str = "_any_";

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

/// Formats an instance ID as a bound instance segment (appears right after MASTER_NODE in key expressions)
fn format_bound_instance_segment(instance_id: &str) -> Option<String> {
    (instance_id != INSTANCE_ID_WILDCARD).then(|| format!("{instance_id}"))
}

/// Formats an instance ID as a target instance segment (identifies a specific target/source instance)
fn format_target_instance_segment(instance_id: &str) -> Option<String> {
    (instance_id != INSTANCE_ID_WILDCARD).then(|| format!("{instance_id}"))
}

pub struct TopicMessenger;

pub struct ServiceMessenger;

pub struct ActionMessenger;

pub struct ServiceEndpoint {
    messenger: Arc<Mutex<Messenger>>,
    /// Subscriptions to service requests. Four patterns are needed to match:
    /// - [0] Requests targeting this specific master node and instance
    /// - [1] Requests targeting this specific master node with broadcast instance
    /// - [2] Broadcast requests (any master) targeting this specific instance
    /// - [3] Full broadcast requests (any master, any instance)
    subscriptions: [Subscription; 4],
    bound_master_node: String,
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
                        Ok((context, response_topic)) => return Ok((context, response_topic)),
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
        // Format: target_master/caller_master/target_instance/caller_instance/service_root/request/id
        let identifier = request.key_expr().to_string();
        let mut parts = identifier.split('/').filter(|segment| !segment.is_empty());

        // Parse target_master (first segment)
        let target_master_segment = parts.next().ok_or_else(|| Error::InvalidServiceRequest {
            identifier: identifier.clone(),
            reason: "missing target master node segment in request".to_string(),
        })?;

        // Parse caller_master (second segment)
        let caller_master_segment = parts.next().ok_or_else(|| Error::InvalidServiceRequest {
            identifier: identifier.clone(),
            reason: "missing caller master node segment in request".to_string(),
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
        let response_target_instance_segment =
            format_target_instance_segment(self.instance_id.as_str())
                .unwrap_or_else(|| self.instance_id.clone());

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
            target_master_segment,
            caller_master_segment,
            target_instance_segment,
            caller_instance_segment,
            self.service_root
        );

        // Response topic format: caller_master/responder_master/caller_instance/responder_instance/service_root/response/request_id
        // This ensures master_node() returns responder's master (position 1) and instance_id() returns responder's instance (position 3)
        let response_topic = format!(
            "{}/{}/{}/{}/{}/response/{request_id}",
            caller_master_segment,
            self.bound_master_node,
            caller_instance_segment,
            response_target_instance_segment,
            self.service_root
        );

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
    instance_id: String,
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
        as_master_node: &str,
        as_instance_id: &str,
        to_node_name: &str,
        to_topic: &str,
        to_master_node: Option<&str>,
        to_instance_id: Option<&str>,
        qos: QoSProfile,
    ) -> Result<Subscription> {
        messenger
            .subscribe_to_topic(
                as_master_node,
                as_instance_id,
                to_node_name,
                to_topic,
                to_master_node,
                to_instance_id,
                qos,
            )
            .await
    }

    /// Publishes a payload to a topic on the specified master node.
    pub async fn emit(
        messenger: &MessengerHandle,
        as_master_node: &str,
        as_instance_id: &str,
        as_node_name: &str,
        as_topic_name: &str,
        qos: QoSProfile,
        payload: Bytes,
    ) -> Result<()> {
        messenger
            .emit_topic_message(
                as_master_node,
                as_instance_id,
                as_node_name,
                as_topic_name,
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
        as_master_node: &str,
        as_instance_id: &str,
        as_node_name: &str,
        as_service_name: &str,
    ) -> Result<ServiceEndpoint> {
        messenger
            .expose_service(
                as_master_node,
                as_instance_id,
                as_node_name,
                as_service_name,
            )
            .await
    }

    /// If `target_instance_id` is `None`, this call returns with the first service instance that it hits
    pub async fn poll(
        messenger: &MessengerHandle,
        bound_master_node: &str,
        as_instance_id: &str,
        target_node_name: &str,
        target_service_name: &str,
        target_master_node: Option<&str>,
        target_instance_id: Option<&str>,
        request_payload: Bytes,
        response_timeout: Duration,
    ) -> Result<TopicMessage> {
        messenger
            .poll_service(
                "service",
                bound_master_node,
                as_instance_id,
                target_node_name,
                target_service_name,
                target_master_node,
                target_instance_id,
                request_payload,
                response_timeout,
            )
            .await
    }
}

impl ActionMessenger {
    pub async fn expose(
        messenger: &MessengerHandle,
        bound_master_node: &str,
        as_instance_id: &str,
        as_node_name: &str,
        as_action_name: &str,
    ) -> Result<ActionCreation> {
        messenger
            .expose_action(
                bound_master_node,
                as_node_name,
                as_action_name,
                as_instance_id,
            )
            .await
    }

    pub async fn send_goal(
        messenger: &MessengerHandle,
        as_master_node: &str,
        as_instance_id: &str,
        to_node_name: &str,
        to_action_name: &str,
        target_master_node: Option<&str>,
        target_instance_id: Option<&str>,
        goal_payload: Bytes,
        feedback_qos: QoSProfile,
        goal_timeout: Duration,
    ) -> Result<ActionGoalHandle> {
        let feedback_topic = {
            let sender_master = target_master_node.unwrap_or("*");
            match target_instance_id {
                Some(target_instance_id) => {
                    format!(
                        "{as_master_node}/{sender_master}/{as_instance_id}/{target_instance_id}/action/{to_node_name}/{to_action_name}/feedback/{target_instance_id}"
                    )
                }
                None => format!(
                    "{as_master_node}/*/{as_instance_id}/*/action/{to_node_name}/{to_action_name}/feedback/*"
                ),
            }
        };
        let goal_service_name = format!("{to_action_name}/goal");

        let feedback_subscription = {
            let subscriber_qos = map_node_qos_to_subscriber_qos(feedback_qos);
            let messenger = messenger.messenger.lock().await;
            messenger.subscribe(&feedback_topic, subscriber_qos).await
        }
        .map_err(Error::PeppyMessagingInterface)?;

        let goal_response = messenger
            .poll_service(
                "action",
                as_master_node,
                as_instance_id,
                to_node_name,
                &goal_service_name,
                None,
                target_instance_id,
                goal_payload,
                goal_timeout,
            )
            .await?;

        Ok(ActionGoalHandle {
            master_node: as_master_node.to_string(),
            instance_id: as_instance_id.to_string(),
            node_name: to_node_name.to_string(),
            action_name: to_action_name.to_string(),
            target_instance_id: target_instance_id.map(|id| id.to_string()),
            goal_response,
            feedback: feedback_subscription,
        })
    }

    pub async fn cancel_goal(
        messenger: &MessengerHandle,
        handle: &ActionGoalHandle,
        cancel_timeout: Duration,
    ) -> Result<TopicMessage> {
        let cancel_service_name = format!("{}/cancel", handle.action_name);

        messenger
            .poll_service(
                "action",
                &handle.master_node,
                &handle.instance_id,
                &handle.node_name,
                &cancel_service_name,
                None,
                handle.target_instance_id.as_deref(),
                Bytes::new(),
                cancel_timeout,
            )
            .await
    }

    pub async fn request_result(
        messenger: &MessengerHandle,
        handle: &ActionGoalHandle,
        result_request_payload: Bytes,
        result_timeout: Duration,
    ) -> Result<TopicMessage> {
        let result_service_name = format!("{}/result", handle.action_name);

        messenger
            .poll_service(
                "action",
                &handle.master_node,
                &handle.instance_id,
                &handle.node_name,
                &result_service_name,
                None,
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
        as_master_node: &str,
        as_instance_id: &str,
        to_node_name: &str,
        to_topic: &str,
        to_master_node: Option<&str>,
        to_instance_id: Option<&str>,
        qos: QoSProfile,
    ) -> Result<Subscription> {
        let to_master_node = match to_master_node {
            Some(master_node) => master_node,
            None => "*",
        };
        let to_instance_id = match to_instance_id {
            Some(instance_id) => instance_id,
            None => "*",
        };
        let key_expr = format!(
            "{as_master_node}/{to_master_node}/{as_instance_id}/{to_instance_id}/topic/{to_node_name}/{to_topic}"
        );
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
        as_master_node: &str,
        as_instance_id: &str,
        as_node_name: &str,
        as_topic_name: &str,
        qos: QoSProfile,
        payload: Bytes,
    ) -> Result<()> {
        let key_expr = format!(
            "*/{}/*/{}/topic/{}/{}",
            as_master_node, as_instance_id, as_node_name, as_topic_name
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
        bound_master_node: &str,
        as_instance_id: &str,
        as_node_name: &str,
        as_service_name: &str,
    ) -> Result<ServiceEndpoint> {
        let service_root = format!("service/{as_node_name}/{as_service_name}");
        self.create_service_endpoint(bound_master_node, service_root, as_instance_id)
            .await
    }

    async fn create_service_endpoint(
        &self,
        bound_master_node: &str,
        service_root: String,
        as_instance_id: &str,
    ) -> Result<ServiceEndpoint> {
        // Format: target_master/caller_master/target_instance/caller_instance/service_root/request/id
        // We need 4 subscription patterns to match all valid request combinations:
        let patterns = [
            // 1. Specific master, specific instance
            format!("{bound_master_node}/*/{as_instance_id}/*/{service_root}/request/**"),
            // 2. Specific master, broadcast instance
            format!("{bound_master_node}/*/{BROADCAST_MARKER}/*/{service_root}/request/**"),
            // 3. Broadcast master, specific instance
            format!("{BROADCAST_MARKER}/*/{as_instance_id}/*/{service_root}/request/**"),
            // 4. Broadcast master, broadcast instance
            format!("{BROADCAST_MARKER}/*/{BROADCAST_MARKER}/*/{service_root}/request/**"),
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

        Ok(ServiceEndpoint {
            messenger: Arc::clone(&self.messenger),
            subscriptions: [sub0, sub1, sub2, sub3],
            bound_master_node: bound_master_node.to_string(),
            service_root,
            instance_id: as_instance_id.to_string(),
        })
    }

    async fn poll_service(
        &self,
        message_type: &str,
        bound_master_node: &str,
        as_instance_id: &str,
        target_node_name: &str,
        target_service_name: &str,
        target_master_node: Option<&str>,
        target_instance_id: Option<&str>,
        request_payload: Bytes,
        response_timeout: Duration,
    ) -> Result<TopicMessage> {
        let service_root = format!(
            "{}/{}/{}",
            message_type, target_node_name, target_service_name
        );
        // Caller's instance as TARGET (identifies who is calling in request)
        let caller_target_instance_segment = format_target_instance_segment(as_instance_id)
            .unwrap_or_else(|| INSTANCE_ID_WILDCARD.to_string());
        // Caller's instance as BOUND (caller receives response)
        let caller_bound_instance_segment = format_bound_instance_segment(as_instance_id)
            .unwrap_or_else(|| INSTANCE_ID_WILDCARD.to_string());

        let target_instance_id = target_instance_id.map(str::to_string);

        // If no target specified, use BROADCAST_MARKER for broadcast requests
        // This allows Zenoh subscription patterns to filter at the key expression level
        let (effective_target_master, effective_target_instance) =
            match (target_master_node, target_instance_id.as_deref()) {
                (Some(master), Some(instance)) => (master.to_string(), instance.to_string()),
                (Some(master), None) => (master.to_string(), BROADCAST_MARKER.to_string()),
                (None, Some(instance)) => (BROADCAST_MARKER.to_string(), instance.to_string()),
                (None, None) => (BROADCAST_MARKER.to_string(), BROADCAST_MARKER.to_string()),
            };

        // Target's instance as BOUND (service is bound to receive requests)
        let target_bound_instance_segment =
            format_bound_instance_segment(&effective_target_instance);
        // Target's instance as TARGET (identifies who responded)
        let target_response_instance_segment =
            format_target_instance_segment(&effective_target_instance);
        let request_id = generate_request_id();

        // Format: target_master/caller_master/target_instance/caller_instance/service_root/request/id
        let target_master = target_bound_instance_segment
            .as_ref()
            .map(|_| effective_target_master.as_str())
            .unwrap_or(BROADCAST_MARKER);
        let target_instance = target_bound_instance_segment
            .as_deref()
            .unwrap_or(BROADCAST_MARKER);
        let request_topic = format!(
            "{}/{}/{}/{}/{}/request/{request_id}",
            target_master,
            bound_master_node,
            target_instance,
            caller_target_instance_segment,
            service_root
        );

        // Response topic format: caller_master/responder_master/caller_instance/responder_instance/service_root/response/request_id
        // For response subscriptions, use wildcard (*) instead of _any_ because we're subscribing (not publishing)
        // The responder will publish with its actual master node, so we need a wildcard to match any responder
        let response_master_pattern = if effective_target_master == BROADCAST_MARKER {
            "*"
        } else {
            &effective_target_master
        };
        let response_topic = match target_response_instance_segment.as_deref() {
            Some(segment) => {
                // When targeting a specific instance, use wildcard for master if we don't know it
                let response_instance_pattern = if segment == BROADCAST_MARKER {
                    "*"
                } else {
                    segment
                };
                format!(
                    "{}/{}/{}/{}/{}/response/{request_id}",
                    bound_master_node,
                    response_master_pattern,
                    caller_bound_instance_segment,
                    response_instance_pattern,
                    service_root
                )
            }
            None => {
                format!(
                    "{}/*/{}/*/{}/response/{request_id}",
                    bound_master_node, caller_bound_instance_segment, service_root
                )
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
        bound_master_node: &str,
        as_node_name: &str,
        as_action_name: &str,
        as_instance_id: &str,
    ) -> Result<ActionCreation> {
        let action_root = format!("action/{}/{}", as_node_name, as_action_name);

        let goal_service_root = format!("{action_root}/goal");
        let cancel_service_root = format!("{action_root}/cancel");
        let result_service_root = format!("{action_root}/result");

        let bound_instance_segment = format_bound_instance_segment(as_instance_id)
            .unwrap_or_else(|| as_instance_id.to_string());
        let feedback_topic_suffix = format!(
            "*/{bound_master_node}/*/{bound_instance_segment}/{action_root}/feedback/{as_instance_id}"
        );

        let goal_service = self
            .create_service_endpoint(bound_master_node, goal_service_root, as_instance_id)
            .await?;
        let cancel_service = self
            .create_service_endpoint(bound_master_node, cancel_service_root, as_instance_id)
            .await?;
        let result_service = self
            .create_service_endpoint(bound_master_node, result_service_root, as_instance_id)
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
