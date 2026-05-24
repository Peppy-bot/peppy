use super::{FromAnyTopicGuard, MessengerHandle};
use crate::error::{Error, Result};
use crate::types::{Message, Payload};
use config::node::QoSProfile;
use pmi::{MessengerPublisher, SenderTarget, TopicWireReceiver, TopicWireSender};

use std::sync::Arc;

pub struct Subscription {
    inner: pmi::Subscription,
    /// Live for the subscription's full lifetime when this is a from_any
    /// topic sub; releases the messenger's per-`(name, tag)` reservation
    /// on drop. `None` for pinned subs and target-less subscriptions.
    _from_any_guard: Option<FromAnyTopicGuard>,
}

impl Subscription {
    pub(crate) fn new(inner: pmi::Subscription) -> Self {
        Self {
            inner,
            _from_any_guard: None,
        }
    }

    pub(crate) fn with_from_any_guard(mut self, guard: FromAnyTopicGuard) -> Self {
        self._from_any_guard = Some(guard);
        self
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
    /// `from_link_id` `Some(value)` filters to a producer's specific bound
    /// link_id; `None` matches any link_id (used for `from_any: true`
    /// consumers and for unscoped subscribes).
    #[allow(clippy::too_many_arguments)]
    pub async fn subscribe(
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
        // Reserve the from_any `(name, tag)` slot before any wire work. The
        // manifest validator enforces "at most one from_any topic sub per
        // (name, tag) per messenger" at config time; this is the runtime
        // guard at the wire's trust boundary. If wire setup fails afterwards
        // the guard is dropped as a local and the slot is released.
        let from_any_guard = match (&from_target, from_link_id) {
            (Some(target), None) => Some(messenger.reserve_from_any_topic(
                from_core_node,
                from_instance_id,
                target.name(),
                target.tag(),
            )?),
            _ => None,
        };
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
        let mut subscription = Subscription::new(subscription);
        if let Some(guard) = from_any_guard {
            subscription = subscription.with_from_any_guard(guard);
        }
        Ok(subscription)
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

    /// Publishes a payload to a topic. The producer advertises under the
    /// reserved default `_` segment; consumers pin a specific producer by
    /// `from_instance_id` derived from the consumer's binding map.
    pub async fn emit(
        messenger: &MessengerHandle,
        as_core_node: &str,
        as_instance_id: &str,
        as_target: SenderTarget,
        as_topic_name: &str,
        qos: QoSProfile,
        payload: Payload,
    ) -> Result<()> {
        let sender =
            TopicWireSender::new(as_core_node, as_instance_id, as_target, None, as_topic_name)?;
        messenger
            .emit_topic_message(&sender, qos, payload, true)
            .await?;
        Ok(())
    }

    /// Pre-binds a topic publisher under a single producer-side link_id,
    /// bypassing the central `Messenger` mutex on every subsequent publish.
    /// Use this in publish loops; use [`emit`] for one-shot publishes.
    /// `link_id` `None` falls back to the reserved default `_` segment.
    ///
    /// A pre-bound publisher always tags its publishes as primary on the
    /// wire (it can't know about a parallel multi-link `emit` loop), so
    /// mixing this with [`emit`] on the *same* topic isn't supported — a
    /// wildcard subscriber would observe the pre-bound publish and the
    /// `emit`'s primary publish as two separate deliveries. Pick one
    /// publication path per topic.
    #[allow(clippy::too_many_arguments)]
    pub async fn declare_publisher(
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
