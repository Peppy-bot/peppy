use super::{MessengerHandle, TopicWireReceiver, TopicWireSender};
use crate::error::{Error, Result};
use crate::types::{Message, Payload};
use config::node::QoSProfile;
use pmi::{Iface, MessengerPublisher};
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
    /// Subscribe to a topic published by a specific node. `iface` must match
    /// the iface segments the publisher used in [`Self::emit`].
    #[allow(clippy::too_many_arguments)]
    pub async fn subscribe(
        messenger: &MessengerHandle,
        as_core_node: &str,
        as_instance_id: &str,
        to_node_name: &str,
        iface: Iface,
        to_topic: &str,
        target_core_node: Option<&str>,
        target_instance_id: Option<&str>,
        qos: QoSProfile,
    ) -> Result<Subscription> {
        let recv = TopicWireReceiver {
            as_core_node: as_core_node.to_string(),
            as_instance_id: as_instance_id.to_string(),
            to_core_node: target_core_node.map(str::to_string),
            to_instance_id: target_instance_id.map(str::to_string),
            to_node_name: to_node_name.to_string(),
            iface,
            to_topic: to_topic.to_string(),
        };
        let subscription = messenger.subscribe_to_topic(&recv, qos).await?;
        Ok(Subscription::new(subscription))
    }

    /// Consumes a topic from any node (external/unlinked topics).
    ///
    /// Unlike [`subscribe`], this does not target a specific publisher node.
    /// Uses `*` wildcards in the node and iface segments — matching whatever
    /// any publisher writes on the given topic.
    pub async fn consume_external(
        messenger: &MessengerHandle,
        as_core_node: &str,
        as_instance_id: &str,
        to_topic: &str,
        target_core_node: Option<&str>,
        target_instance_id: Option<&str>,
        qos: QoSProfile,
    ) -> Result<Subscription> {
        // External consumers don't know the producer's node/iface identity;
        // a wildcard `*` is spliced into those segments. This stays an
        // intentional escape hatch — the typed wire structs assume known
        // identity, so we feed `"*"` directly.
        let recv = TopicWireReceiver {
            as_core_node: as_core_node.to_string(),
            as_instance_id: as_instance_id.to_string(),
            to_core_node: target_core_node.map(str::to_string),
            to_instance_id: target_instance_id.map(str::to_string),
            to_node_name: "*".to_string(),
            iface: Iface::new("*", "*"),
            to_topic: to_topic.to_string(),
        };
        let subscription = messenger.subscribe_to_topic(&recv, qos).await?;
        Ok(Subscription::new(subscription))
    }

    /// Publishes a payload to a topic on the specified core node.
    #[allow(clippy::too_many_arguments)]
    pub async fn emit(
        messenger: &MessengerHandle,
        as_core_node: &str,
        as_instance_id: &str,
        as_node_name: &str,
        iface: Iface,
        as_topic_name: &str,
        qos: QoSProfile,
        payload: Payload,
    ) -> Result<()> {
        let sender = TopicWireSender {
            as_core_node: as_core_node.to_string(),
            as_instance_id: as_instance_id.to_string(),
            as_node_name: as_node_name.to_string(),
            iface,
            as_topic_name: as_topic_name.to_string(),
        };
        messenger.emit_topic_message(&sender, qos, payload).await
    }

    /// Pre-binds a topic publisher, bypassing the central `Messenger` mutex
    /// on every subsequent publish. Use this in publish loops; use [`emit`]
    /// for one-shot publishes.
    pub async fn declare_publisher(
        messenger: &MessengerHandle,
        as_core_node: &str,
        as_instance_id: &str,
        as_node_name: &str,
        iface: Iface,
        as_topic_name: &str,
        qos: QoSProfile,
    ) -> Result<TopicPublisher> {
        let sender = TopicWireSender {
            as_core_node: as_core_node.to_string(),
            as_instance_id: as_instance_id.to_string(),
            as_node_name: as_node_name.to_string(),
            iface,
            as_topic_name: as_topic_name.to_string(),
        };
        let inner = messenger
            .declare_topic_publisher(&sender, qos.into())
            .await?;
        Ok(TopicPublisher::new(Arc::new(inner)))
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
}

impl TopicPublisher {
    pub(crate) fn new(inner: Arc<MessengerPublisher>) -> Self {
        Self { inner }
    }

    pub async fn publish(&self, payload: Payload) -> Result<()> {
        self.inner
            .publish(payload.into_inner())
            .await
            .map_err(Error::PeppyMessagingInterface)
    }
}
