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
    pub async fn next_request(&mut self) -> Option<ServiceRequest> {
        while let Some(request) = self.subscription.rx.recv().await {
            let Message { topic, payload } = request;

            if !topic.starts_with(&self.request_topic_prefix) {
                error!(%topic, "service received request on unexpected topic");
                continue;
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

            return Some(ServiceRequest {
                message,
                request_id,
                response_topic,
                messenger: Arc::clone(&self.messenger),
            });
        }

        None
    }
}

pub struct ServiceRequest {
    pub message: Message,
    request_id: Option<String>,
    response_topic: String,
    messenger: Arc<Mutex<Messenger>>,
}

impl ServiceRequest {
    pub fn request_id(&self) -> Option<&str> {
        self.request_id.as_deref()
    }

    pub async fn respond(self, payload: Bytes) -> Result<()> {
        let response = Message::new(&self.response_topic, payload.as_ref());
        let mut messenger = self.messenger.lock().await;
        messenger
            .publish(response, PublisherQoS::Standard)
            .await
            .map_err(Error::PeppyMessagingInterface)
    }
}

impl PeppyMessenger {
    pub async fn new(node_name: &str) -> Self {
        let adapter = ZenohAdapter::default();
        Self {
            node_name: String::from(node_name),
            messenger: Arc::new(Mutex::new(PeppyMessenger::new_session(adapter).await)),
        }
    }

    pub async fn from_host_port(node_name: &str, host: &str, port: u16) -> Self {
        let adapter = ZenohAdapter::from_host_port(ZenohNetProtocol::Tcp, host, port);
        Self {
            node_name: String::from(node_name),
            messenger: Arc::new(Mutex::new(PeppyMessenger::new_session(adapter).await)),
        }
    }

    async fn new_session(adapter: ZenohAdapter) -> Messenger {
        let mut messenger = Messenger::new(MessengerAdapter::Zenoh(adapter));
        messenger
            .start_session()
            .await
            .expect("Failed to start session");

        messenger
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
    ) -> Result<Subscription> {
        let qos = QoSProfile::Reliable; // Always the same QoSProfile for services
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
        let msg = Message::new(&full_ns, &payload);

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
                    Message::new(&request_topic, request_payload.as_ref()),
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

    // pub async fn expose_action<F, Fut>(
    //     &self,
    //     namespace: &str,
    //     action_name: &str,
    //     handler: F,
    // ) -> Result<JoinHandle<Result<()>>>
    // where
    //     F: Fn(Message) -> Fut + Send + Sync + 'static,
    //     Fut: Future<Output = Result<Bytes>> + Send + 'static,
    // {
    //     // TODO finish
    //     Ok(Bytes::new())
    // }
}
