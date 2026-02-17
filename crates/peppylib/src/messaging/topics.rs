use super::MessengerHandle;
use crate::error::{Error, Result};
use bytes::Bytes;
use config::node::QoSProfile;
use pmi::{Message, Messenger, MessengerBackend, PublisherQoS, Subscription};
use std::sync::Arc;
use tokio::sync::Mutex;

pub struct TopicMessenger;

impl TopicMessenger {
    #[allow(clippy::too_many_arguments)]
    pub async fn subscribe(
        messenger: &MessengerHandle,
        as_daemon_node: &str,
        as_instance_id: &str,
        to_node_name: &str,
        to_topic: &str,
        to_daemon_node: Option<&str>,
        to_instance_id: Option<&str>,
        qos: QoSProfile,
    ) -> Result<Subscription> {
        messenger
            .subscribe_to_topic(
                as_daemon_node,
                as_instance_id,
                to_node_name,
                to_topic,
                to_daemon_node,
                to_instance_id,
                qos,
            )
            .await
    }

    /// Publishes a payload to a topic on the specified daemon node.
    pub async fn emit(
        messenger: &MessengerHandle,
        as_daemon_node: &str,
        as_instance_id: &str,
        as_node_name: &str,
        as_topic_name: &str,
        qos: QoSProfile,
        payload: Bytes,
    ) -> Result<()> {
        messenger
            .emit_topic_message(
                as_daemon_node,
                as_instance_id,
                as_node_name,
                as_topic_name,
                qos,
                payload,
            )
            .await
    }
}

#[derive(Clone)]
pub struct TopicPublisher {
    messenger: Arc<Mutex<Messenger>>,
    topic: String,
    qos: PublisherQoS,
}

impl TopicPublisher {
    pub(super) fn new(messenger: Arc<Mutex<Messenger>>, topic: String, qos: PublisherQoS) -> Self {
        Self {
            messenger,
            topic,
            qos,
        }
    }

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
