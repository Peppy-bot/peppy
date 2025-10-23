#[cfg(test)]
mod tests;

use crate::error::{Error, Result};
use bytes::Bytes;
use config::node::QoSProfile;
use pmi::{
    Message, Messenger, MessengerAdapter, MessengerBackend, PeppyMessagingInterfaceError,
    PublisherQoS, SubscriberQoS, Subscription, ZenohAdapter, ZenohNetProtocol,
};
use sha2::{Digest, Sha256};
use std::{
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};
use tokio::{
    sync::Mutex,
    time::{Duration, timeout},
};
use tracing::error;

/// This struct represent one deployment instance messaging
pub struct PeppyMessenger {
    node_name: String,
    messenger: Arc<Mutex<Messenger>>,
}

pub struct ServiceEndpoint {
    messenger: Arc<Mutex<Messenger>>,
    subscription: Subscription,
    request_topic_base: String,
    request_topic_prefix: String,
    response_topic_base: String,
}

impl ServiceEndpoint {
    /// We need to use a callback system here to force the service to send back a response
    pub async fn handle_next_request<F, Fut>(&mut self, handler: F) -> Result<bool>
    where
        F: FnOnce(ServiceRequestContext) -> Fut,
        Fut: std::future::Future<Output = Result<Bytes>>,
    {
        let mut handler = Some(handler);

        while let Some(request) = self.subscription.rx.recv().await {
            if let Some((context, response_topic)) = self.build_request_context(request) {
                let handler = handler
                    .take()
                    .expect("service handler invoked more than once in handle_next_request");
                let response_payload = handler(context).await?;
                self.publish_response(response_topic, response_payload)
                    .await?;
                return Ok(true);
            }
        }

        Ok(false)
    }

    /// Handles requests until the subscription stream ends.
    pub async fn handle_requests<F, Fut>(&mut self, mut handler: F) -> Result<()>
    where
        F: FnMut(ServiceRequestContext) -> Fut,
        Fut: std::future::Future<Output = Result<Bytes>>,
    {
        while let Some(request) = self.subscription.rx.recv().await {
            if let Some((context, response_topic)) = self.build_request_context(request) {
                let response_payload = handler(context).await?;
                self.publish_response(response_topic, response_payload)
                    .await?;
            }
        }

        Ok(())
    }

    fn build_request_context(&self, request: Message) -> Option<(ServiceRequestContext, String)> {
        let Message { topic, payload } = request;

        if !topic.starts_with(&self.request_topic_prefix) {
            error!(%topic, "service received request on unexpected topic");
            return None;
        }

        let request_id = topic
            .strip_prefix(&self.request_topic_prefix)
            .and_then(|rest| rest.split('/').next())
            .filter(|segment| !segment.is_empty())
            .map(str::to_string);

        let response_topic = request_id
            .as_ref()
            .map(|id| format!("{}/{}", self.response_topic_base, id))
            .unwrap_or_else(|| self.response_topic_base.clone());

        let message = Message {
            topic: self.request_topic_base.clone(),
            payload,
        };

        let context = ServiceRequestContext {
            message,
            request_id,
        };

        Some((context, response_topic))
    }

    async fn publish_response(&self, topic: String, payload: Bytes) -> Result<()> {
        let response = Message { topic, payload };
        let mut messenger = self.messenger.lock().await;
        messenger
            .publish(response, PublisherQoS::Standard)
            .await
            .map_err(Error::PeppyMessagingInterface)
    }
}

pub struct ServiceRequestContext {
    pub message: Message,
    request_id: Option<String>,
}

impl ServiceRequestContext {
    pub fn request_id(&self) -> Option<&str> {
        self.request_id.as_deref()
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
        let message = Message {
            topic: self.topic.clone(),
            payload,
        };
        let mut messenger = self.messenger.lock().await;
        messenger
            .publish(message, self.qos)
            .await
            .map_err(Error::PeppyMessagingInterface)
    }
}

pub struct ActionGoalHandle {
    server_node: String,
    namespace: String,
    action_name: String,
    goal_response: Bytes,
    feedback: Subscription,
}

impl ActionGoalHandle {
    pub fn goal_response(&self) -> &Bytes {
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

impl PeppyMessenger {
    pub async fn new(node_name: &str) -> Result<Self> {
        let adapter = ZenohAdapter::default();
        let messenger = PeppyMessenger::new_session(adapter).await?;
        Ok(Self {
            node_name: String::from(node_name),
            messenger: Arc::new(Mutex::new(messenger)),
        })
    }

    pub async fn from_host_port(node_name: &str, host: &str, port: u16) -> Result<Self> {
        let adapter = ZenohAdapter::from_host_port(ZenohNetProtocol::Tcp, host, port);
        let messenger = PeppyMessenger::new_session(adapter).await?;
        Ok(Self {
            node_name: String::from(node_name),
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

    /// Generates a unique request ID using SHA256 hash of node name + timestamp + thread ID
    /// This ensures each service call has a unique correlation ID
    fn generate_request_id(node_name: &str) -> String {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let thread_id = std::thread::current().id();

        let mut hasher = Sha256::new();
        hasher.update(node_name.as_bytes());
        hasher.update(timestamp.to_le_bytes());
        hasher.update(format!("{:?}", thread_id).as_bytes());

        let result = hasher.finalize();
        format!("{:x}", result)[..16].to_string() // Use first 16 hex chars for compactness
    }

    pub fn build_full_namespace(
        node_name: &str,
        namespace: &str,
        message_type_name: &str,
    ) -> String {
        [node_name, namespace, message_type_name]
            .into_iter()
            .flat_map(|part| part.split('/'))
            .filter(|segment| !segment.is_empty())
            .collect::<Vec<_>>()
            .join("/")
    }

    pub async fn receive_topic_msg(
        &self,
        from_node_name: &str,
        namespace: &str,
        topic_name: &str,
        qos: QoSProfile,
    ) -> Result<Subscription> {
        let topic_path = Self::build_full_namespace(from_node_name, namespace, topic_name);
        let subscriber_qos = Self::map_node_qos_to_subscriber_qos(qos);

        let subscription = {
            let messenger = self.messenger.lock().await;
            messenger.subscribe(&topic_path, subscriber_qos).await
        }
        .map_err(Error::PeppyMessagingInterface)?;

        Ok(subscription)
    }

    pub async fn emit_topic_message(
        &self,
        namespace: &str,
        topic_name: &str,
        qos: QoSProfile,
        payload: Bytes,
    ) -> Result<()> {
        let full_ns = Self::build_full_namespace(&self.node_name, namespace, topic_name);
        let msg = Message {
            topic: full_ns,
            payload,
        };

        let publisher_qos = PeppyMessenger::map_node_qos_to_publisher_qos(qos);

        let mut messenger = self.messenger.lock().await;
        messenger
            .publish(msg, publisher_qos)
            .await
            .map_err(Error::PeppyMessagingInterface)
    }

    pub async fn expose_service(
        &self,
        namespace: &str,
        service_name: &str,
    ) -> Result<ServiceEndpoint> {
        let service_root = Self::build_full_namespace(&self.node_name, namespace, service_name);
        self.create_service_endpoint(service_root).await
    }

    async fn create_service_endpoint(&self, service_root: String) -> Result<ServiceEndpoint> {
        let request_topic_base = format!("{service_root}/request");
        let request_topic_prefix = format!("{request_topic_base}/");
        let request_subscription_topic = format!("{request_topic_base}/**");
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
            request_topic_base,
            request_topic_prefix,
            response_topic_base,
        })
    }

    pub async fn poll_service(
        &self,
        from_node_name: &str,
        namespace: &str,
        service_name: &str,
        request_payload: Bytes,
        response_timeout: Duration,
    ) -> Result<Bytes> {
        let service_root =
            PeppyMessenger::build_full_namespace(from_node_name, namespace, service_name);

        let request_id = Self::generate_request_id(&self.node_name);

        let request_topic = format!("{service_root}/request/{request_id}");
        let response_topic = format!("{service_root}/response/{request_id}");

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
                    Message {
                        topic: request_topic,
                        payload: request_payload,
                    },
                    PublisherQoS::Standard,
                )
                .await
                .map_err(Error::PeppyMessagingInterface)?;
        }

        let response = timeout(response_timeout, response_subscription.rx.recv())
            .await
            .map_err(|_| Error::ServiceTimeout {
                service_node: from_node_name.to_string(),
                namespace: namespace.to_string(),
                service_name: service_name.to_string(),
            })?
            .ok_or_else(|| {
                Error::PeppyMessagingInterface(PeppyMessagingInterfaceError::BackendError(
                    "service response channel closed".to_string(),
                ))
            })?;

        Ok(response.payload)
    }

    pub async fn expose_action(
        &self,
        namespace: &str,
        service_name: &str,
    ) -> Result<ActionCreation> {
        let action_root = Self::build_full_namespace(&self.node_name, namespace, service_name);

        let goal_service_root = format!("{action_root}/goal");
        let cancel_service_root = format!("{action_root}/cancel");
        let result_service_root = format!("{action_root}/result");
        let feedback_topic = format!("{action_root}/feedback");

        let goal_service = self.create_service_endpoint(goal_service_root).await?;
        let cancel_service = self.create_service_endpoint(cancel_service_root).await?;
        let result_service = self.create_service_endpoint(result_service_root).await?;

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

    pub async fn send_action_goal(
        &self,
        server_node: &str,
        namespace: &str,
        action_name: &str,
        goal_payload: Bytes,
        feedback_qos: QoSProfile,
        goal_timeout: Duration,
    ) -> Result<ActionGoalHandle> {
        let feedback_topic = format!("{action_name}/feedback");
        let goal_service_name = format!("{action_name}/goal");

        let feedback_subscription = self
            .receive_topic_msg(server_node, namespace, &feedback_topic, feedback_qos)
            .await?;

        let goal_response = self
            .poll_service(
                server_node,
                namespace,
                &goal_service_name,
                goal_payload,
                goal_timeout,
            )
            .await?;

        Ok(ActionGoalHandle {
            server_node: server_node.to_string(),
            namespace: namespace.to_string(),
            action_name: action_name.to_string(),
            goal_response,
            feedback: feedback_subscription,
        })
    }

    pub async fn poll_action_result(
        &self,
        handle: &ActionGoalHandle,
        result_request_payload: Bytes,
        result_timeout: Duration,
    ) -> Result<Bytes> {
        let result_service_name = format!("{}/result", handle.action_name);

        self.poll_service(
            &handle.server_node,
            &handle.namespace,
            &result_service_name,
            result_request_payload,
            result_timeout,
        )
        .await
        .map_err(|err| match err {
            Error::ServiceTimeout { .. } => Error::ActionResultTimeout {
                action_node: handle.server_node.clone(),
                namespace: handle.namespace.clone(),
                action_name: handle.action_name.clone(),
            },
            other => other,
        })
    }
}
