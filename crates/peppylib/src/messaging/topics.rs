use super::MessengerHandle;
use crate::error::{Error, Result};
use crate::types::{Message, Payload};
use config::node::QoSProfile;
use pmi::{Message as PmiMessage, Messenger, MessengerBackend, PublisherQoS};
use std::sync::Arc;
use tokio::sync::Mutex;

pub struct Subscription {
    inner: pmi::Subscription,
}

impl Subscription {
    pub(crate) fn new(inner: pmi::Subscription) -> Self {
        Self { inner }
    }

    pub async fn on_next_message(&mut self) -> Option<Message> {
        self.inner.rx.recv().await.map(Message::from)
    }

    pub(crate) fn try_on_next_message(
        &mut self,
    ) -> std::result::Result<Message, crate::types::TryRecvError> {
        self.inner
            .rx
            .try_recv()
            .map(Message::from)
            .map_err(crate::types::TryRecvError::from)
    }
}

pub struct TopicMessenger;

impl TopicMessenger {
    #[allow(clippy::too_many_arguments)]
    pub async fn subscribe(
        messenger: &MessengerHandle,
        as_core_node: &str,
        as_instance_id: &str,
        to_node_name: &str,
        to_topic: &str,
        to_core_node: Option<&str>,
        to_instance_id: Option<&str>,
        qos: QoSProfile,
    ) -> Result<Subscription> {
        let subscription = messenger
            .subscribe_to_topic(
                as_core_node,
                as_instance_id,
                to_node_name,
                to_topic,
                to_core_node,
                to_instance_id,
                qos,
            )
            .await?;
        Ok(Subscription::new(subscription))
    }

    /// Subscribes to a topic from any node (external/unlinked topics).
    ///
    /// Unlike [`subscribe`], this does not target a specific publisher node.
    /// Internally uses a wildcard for the node name, so messages from any
    /// node publishing on the given topic will be received.
    pub async fn subscribe_external(
        messenger: &MessengerHandle,
        as_core_node: &str,
        as_instance_id: &str,
        to_topic: &str,
        to_core_node: Option<&str>,
        to_instance_id: Option<&str>,
        qos: QoSProfile,
    ) -> Result<Subscription> {
        let subscription = messenger
            .subscribe_to_topic(
                as_core_node,
                as_instance_id,
                "*",
                to_topic,
                to_core_node,
                to_instance_id,
                qos,
            )
            .await?;
        Ok(Subscription::new(subscription))
    }

    /// Publishes a payload to a topic on the specified core node.
    pub async fn emit(
        messenger: &MessengerHandle,
        as_core_node: &str,
        as_instance_id: &str,
        as_node_name: &str,
        as_topic_name: &str,
        qos: QoSProfile,
        payload: Payload,
    ) -> Result<()> {
        messenger
            .emit_topic_message(
                as_core_node,
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
    pub(crate) fn new(messenger: Arc<Mutex<Messenger>>, topic: String, qos: PublisherQoS) -> Self {
        Self {
            messenger,
            topic,
            qos,
        }
    }

    pub fn topic(&self) -> &str {
        &self.topic
    }

    pub async fn publish(&self, payload: Payload) -> Result<()> {
        self.publish_on(&self.topic, payload).await
    }

    async fn publish_on(&self, topic: &str, payload: Payload) -> Result<()> {
        let message = PmiMessage::new(topic, payload.into_inner());
        let mut messenger = self.messenger.lock().await;
        messenger
            .publish(message, self.qos)
            .await
            .map_err(Error::PeppyMessagingInterface)
    }
}
