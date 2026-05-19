use super::MessengerHandle;
use crate::error::{Error, Result};
use crate::types::{Message, Payload};
use config::node::QoSProfile;
use pmi::{MessengerPublisher, SenderTarget, TopicWireReceiver, TopicWireSender};
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
    /// Subscribe to a topic published by a specific target. `from_target`
    /// `Some(SenderTarget)` filters on the publisher's identity; `None`
    /// wildcards the target segment (any node or interface emits a match).
    /// See [`Self::subscribe_with_link_id`] for producer-side link_id
    /// pinning.
    #[allow(clippy::too_many_arguments)]
    pub async fn subscribe(
        messenger: &MessengerHandle,
        as_core_node: &str,
        as_instance_id: &str,
        from_target: Option<SenderTarget>,
        to_topic: &str,
        from_core_node: Option<&str>,
        from_instance_id: Option<&str>,
        qos: QoSProfile,
    ) -> Result<Subscription> {
        Self::subscribe_with_link_id(
            messenger,
            as_core_node,
            as_instance_id,
            from_target,
            None,
            to_topic,
            from_core_node,
            from_instance_id,
            qos,
        )
        .await
    }

    /// Subscribe to a topic, pinning the producer's bound link_id slot.
    /// `from_link_id` `Some(value)` filters to a producer's specific
    /// bound link_id; `None` matches any link_id (used for `from_any: true`
    /// consumers).
    #[allow(clippy::too_many_arguments)]
    pub async fn subscribe_with_link_id(
        messenger: &MessengerHandle,
        as_core_node: &str,
        as_instance_id: &str,
        from_target: Option<SenderTarget>,
        from_link_id: Option<&str>,
        to_topic: &str,
        from_core_node: Option<&str>,
        from_instance_id: Option<&str>,
        qos: QoSProfile,
    ) -> Result<Subscription> {
        let recv = TopicWireReceiver::new(
            as_core_node,
            as_instance_id,
            from_core_node,
            from_instance_id,
            from_target,
            from_link_id,
            to_topic,
        )?;
        let subscription = messenger.subscribe_to_topic(&recv, qos).await?;
        Ok(Subscription::new(subscription))
    }

    /// Consumes a topic from any publisher (external/unlinked topics).
    ///
    /// Unlike [`subscribe`], this does not target a specific publisher. The
    /// transport translates the absent target into its match-any segments at
    /// the wire layer.
    pub async fn consume_external(
        messenger: &MessengerHandle,
        as_core_node: &str,
        as_instance_id: &str,
        to_topic: &str,
        from_core_node: Option<&str>,
        from_instance_id: Option<&str>,
        qos: QoSProfile,
    ) -> Result<Subscription> {
        let recv = TopicWireReceiver::new(
            as_core_node,
            as_instance_id,
            from_core_node,
            from_instance_id,
            None,
            None,
            to_topic,
        )?;
        let subscription = messenger.subscribe_to_topic(&recv, qos).await?;
        Ok(Subscription::new(subscription))
    }

    /// Publishes a payload to a topic on the specified core node. See
    /// [`Self::emit_with_link_id`] for producer-side link_id pinning, and
    /// [`Self::emit_fan_out`] for emitting once across multiple bound link_ids.
    pub async fn emit(
        messenger: &MessengerHandle,
        as_core_node: &str,
        as_instance_id: &str,
        as_target: SenderTarget,
        as_topic_name: &str,
        qos: QoSProfile,
        payload: Payload,
    ) -> Result<()> {
        Self::emit_with_link_id(
            messenger,
            as_core_node,
            as_instance_id,
            as_target,
            None,
            as_topic_name,
            qos,
            payload,
        )
        .await
    }

    /// Publishes a payload to a topic, pinning the publisher's link_id slot.
    /// `link_id` `None` falls back to the reserved default `_` segment;
    /// `Some(value)` pins to a specific producer link_id. For multi-link_id
    /// fan-out from a single emission, prefer [`Self::emit_fan_out`].
    #[allow(clippy::too_many_arguments)]
    pub async fn emit_with_link_id(
        messenger: &MessengerHandle,
        as_core_node: &str,
        as_instance_id: &str,
        as_target: SenderTarget,
        link_id: Option<&str>,
        as_topic_name: &str,
        qos: QoSProfile,
        payload: Payload,
    ) -> Result<()> {
        let sender = TopicWireSender::new(
            as_core_node,
            as_instance_id,
            as_target,
            link_id,
            as_topic_name,
        )?;
        messenger.emit_topic_message(&sender, qos, payload).await
    }

    /// Pre-binds a topic publisher, bypassing the central `Messenger` mutex
    /// on every subsequent publish. Use this in publish loops; use [`emit`]
    /// for one-shot publishes.
    pub async fn declare_publisher(
        messenger: &MessengerHandle,
        as_core_node: &str,
        as_instance_id: &str,
        as_target: SenderTarget,
        as_topic_name: &str,
        qos: QoSProfile,
    ) -> Result<TopicPublisher> {
        Self::declare_publisher_with_link_id(
            messenger,
            as_core_node,
            as_instance_id,
            as_target,
            None,
            as_topic_name,
            qos,
        )
        .await
    }

    /// Pre-binds a topic publisher, pinning the publisher's link_id slot.
    /// `link_id` `None` falls back to the reserved default `_` segment.
    pub async fn declare_publisher_with_link_id(
        messenger: &MessengerHandle,
        as_core_node: &str,
        as_instance_id: &str,
        as_target: SenderTarget,
        link_id: Option<&str>,
        as_topic_name: &str,
        qos: QoSProfile,
    ) -> Result<TopicPublisher> {
        let sender = TopicWireSender::new(
            as_core_node,
            as_instance_id,
            as_target,
            link_id,
            as_topic_name,
        )?;
        let inner = messenger
            .declare_topic_publisher(&sender, qos.into())
            .await?;
        Ok(TopicPublisher::new(Arc::new(inner)))
    }

    /// Emit a single payload to a topic across multiple bound link_ids,
    /// one wire emission per link_id. Saves the per-link_id PyO3 round-trip
    /// (the Python codegen calls this once instead of looping per link_id),
    /// and centralizes the fan-out so the implementation can later batch
    /// the `Messenger` lock acquires (one acquire per link_id today). On
    /// the first error the loop aborts and the error is returned.
    #[allow(clippy::too_many_arguments)]
    pub async fn emit_fan_out(
        messenger: &MessengerHandle,
        as_core_node: &str,
        as_instance_id: &str,
        as_target: SenderTarget,
        link_ids: &[String],
        as_topic_name: &str,
        qos: QoSProfile,
        payload: Payload,
    ) -> Result<()> {
        for link_id in link_ids {
            let sender = TopicWireSender::new(
                as_core_node,
                as_instance_id,
                as_target.clone(),
                Some(link_id.as_str()),
                as_topic_name,
            )?;
            messenger
                .emit_topic_message(&sender, qos.clone(), payload.clone())
                .await?;
        }
        Ok(())
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
