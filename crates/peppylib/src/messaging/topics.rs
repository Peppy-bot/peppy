use super::MessengerHandle;
use crate::error::{Error, Result};
use crate::types::{Message, Payload};
use config::node::QoSProfile;
use pmi::MessengerPublisher;
use std::sync::Arc;

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
        iface_name: &str,
        iface_tag: &str,
        to_topic: &str,
        target_core_node: Option<&str>,
        target_instance_id: Option<&str>,
        qos: QoSProfile,
    ) -> Result<Subscription> {
        let subscription = messenger
            .subscribe_to_topic(
                as_core_node,
                as_instance_id,
                to_node_name,
                iface_name,
                iface_tag,
                to_topic,
                target_core_node,
                target_instance_id,
                qos,
            )
            .await?;
        Ok(Subscription::new(subscription))
    }

    /// Consumes a topic from any node (external/unlinked topics).
    ///
    /// Unlike [`subscribe`], this does not target a specific publisher node.
    /// Internally uses a wildcard for the node name, so messages from any
    /// node publishing on the given topic will be received. External
    /// consumers can't know the producer's interface namespace, so this uses
    /// wildcards (`*`) for the two iface segments — matching whatever the
    /// publisher wrote.
    pub async fn consume_external(
        messenger: &MessengerHandle,
        as_core_node: &str,
        as_instance_id: &str,
        to_topic: &str,
        target_core_node: Option<&str>,
        target_instance_id: Option<&str>,
        qos: QoSProfile,
    ) -> Result<Subscription> {
        let subscription = messenger
            .subscribe_to_topic(
                as_core_node,
                as_instance_id,
                "*",
                "*",
                "*",
                to_topic,
                target_core_node,
                target_instance_id,
                qos,
            )
            .await?;
        Ok(Subscription::new(subscription))
    }

    /// Publishes a payload to a topic on the specified core node.
    #[allow(clippy::too_many_arguments)]
    pub async fn emit(
        messenger: &MessengerHandle,
        as_core_node: &str,
        as_instance_id: &str,
        as_node_name: &str,
        iface_name: &str,
        iface_tag: &str,
        as_topic_name: &str,
        qos: QoSProfile,
        payload: Payload,
    ) -> Result<()> {
        messenger
            .emit_topic_message(
                as_core_node,
                as_instance_id,
                as_node_name,
                iface_name,
                iface_tag,
                as_topic_name,
                qos,
                payload,
            )
            .await
    }

    /// Pre-binds a topic publisher to a fixed key + QoS, bypassing the
    /// central `Messenger` mutex on every subsequent publish. Use this in
    /// publish loops (per-tick clock ticks, action feedback streams); use
    /// [`emit`] for one-shot publishes where the per-call setup is in the
    /// noise.
    ///
    /// The key follows the same
    /// `*/<core_node>/*/<instance>/topic/<node>/<iface_name>/<iface_tag>/<topic>`
    /// shape as [`MessengerHandle::emit_topic_message`] — keep this in sync if
    /// that format changes.
    #[allow(clippy::too_many_arguments)]
    pub async fn declare_publisher(
        messenger: &MessengerHandle,
        as_core_node: &str,
        as_instance_id: &str,
        as_node_name: &str,
        iface_name: &str,
        iface_tag: &str,
        as_topic_name: &str,
        qos: QoSProfile,
    ) -> Result<TopicPublisher> {
        // Normalize the tag here so the pre-bound publisher key matches the
        // one `emit_topic_message` would have built dynamically.
        let iface_tag = iface_tag.replace('-', "_");
        let topic = format!(
            "*/{as_core_node}/*/{as_instance_id}/topic/{as_node_name}/{iface_name}/{iface_tag}/{as_topic_name}"
        );
        let inner = messenger
            .declare_publisher(topic.clone(), qos.into())
            .await?;
        Ok(TopicPublisher::new(Arc::new(inner), topic))
    }
}

/// Lock-free per-topic publisher returned by
/// [`TopicMessenger::declare_publisher`]. Wraps a [`pmi::MessengerPublisher`]
/// so `publish` skips the central `Arc<Mutex<Messenger>>` lock — callers in a
/// publish loop don't contend with all other messenger operations.
///
/// Cloneable so action handlers (e.g. feedback streams) can hand the same
/// publisher to multiple background tasks; clones share the same underlying
/// adapter handle (`Arc<zenoh::Session>` or mock `Arc<Mutex<HashMap>>`).
#[derive(Clone)]
pub struct TopicPublisher {
    inner: Arc<MessengerPublisher>,
    topic: String,
}

impl TopicPublisher {
    pub(crate) fn new(inner: Arc<MessengerPublisher>, topic: String) -> Self {
        Self { inner, topic }
    }

    pub fn topic(&self) -> &str {
        &self.topic
    }

    pub async fn publish(&self, payload: Payload) -> Result<()> {
        self.inner
            .publish(payload.into_inner())
            .await
            .map_err(Error::PeppyMessagingInterface)
    }
}
