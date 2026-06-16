use super::{ConsumerFilter, FromAnyTopicGuard, MessengerHandle, ProducerRef};
use crate::error::{Error, Result};
use crate::types::{Message, Payload};
use config::node::QoSProfile;
use pmi::{MessengerPublisher, SenderTarget, TopicWireReceiver, TopicWireSender};

use std::collections::HashSet;
use std::sync::Arc;

/// In-process acceptance / rejection filter applied by
/// [`Subscription::on_next_message`] above the wire layer. Synthesized
/// from a [`ConsumerFilter`] whose variant cannot be expressed as a
/// single wire-side producer pin. Sets hold full
/// `(core_node, instance_id)` pairs — instance_id alone is not a producer
/// identity on the wire, and comparing only half of it would let a
/// same-instance_id producer on another core_node leak through.
#[derive(Debug)]
enum AcceptanceFilter {
    /// Accept only messages whose source `(core_node, instance_id)` is in
    /// the set. Built from [`ConsumerFilter::OnlyFrom`] with more than one
    /// producer (single-producer collapses to a wire pin).
    Allow(HashSet<ProducerRef>),
    /// Drop messages whose source `(core_node, instance_id)` is in the
    /// set. Built from [`ConsumerFilter::AnyExcept`] with a non-empty set.
    Deny(HashSet<ProducerRef>),
}

impl AcceptanceFilter {
    fn accepts(&self, core_node: &str, instance_id: &str) -> bool {
        // Allocation-free pair lookup would need a borrowed key type;
        // filters are smallish and topic rates moderate, so a stack
        // ProducerRef per message is acceptable. Revisit if profiling
        // disagrees.
        let source = ProducerRef::new(core_node, instance_id);
        match self {
            AcceptanceFilter::Allow(set) => set.contains(&source),
            AcceptanceFilter::Deny(set) => !set.contains(&source),
        }
    }
}

pub struct Subscription {
    inner: pmi::Subscription,
    /// Live for the subscription's full lifetime when this is a from_any
    /// topic sub; releases the messenger's per-`(name, tag)` reservation
    /// on drop. `None` for pinned subs and target-less subscriptions.
    _from_any_guard: Option<FromAnyTopicGuard>,
    /// In-process filter applied to incoming messages before they
    /// surface to user code. `None` when the wire-layer pin already
    /// captures the consumer's filter (Pin / Any cases).
    accept_filter: Option<AcceptanceFilter>,
}

impl Subscription {
    pub(crate) fn new(inner: pmi::Subscription) -> Self {
        Self {
            inner,
            _from_any_guard: None,
            accept_filter: None,
        }
    }

    pub(crate) fn with_from_any_guard(mut self, guard: FromAnyTopicGuard) -> Self {
        self._from_any_guard = Some(guard);
        self
    }

    fn with_accept_filter(mut self, filter: AcceptanceFilter) -> Self {
        self.accept_filter = Some(filter);
        self
    }

    pub async fn on_next_message(&mut self) -> Option<Message> {
        loop {
            let raw = self.inner.rx.recv_async().await.ok()?;
            let msg = Message::from(raw);
            if self
                .accept_filter
                .as_ref()
                .is_none_or(|f| f.accepts(msg.core_node(), msg.instance_id()))
            {
                return Some(msg);
            }
        }
    }

    pub(crate) fn try_on_next_message(
        &mut self,
    ) -> std::result::Result<Message, crate::types::TryRecvError> {
        loop {
            match self.inner.rx.try_recv() {
                Ok(raw) => {
                    let msg = Message::from(raw);
                    if self
                        .accept_filter
                        .as_ref()
                        .is_none_or(|f| f.accepts(msg.core_node(), msg.instance_id()))
                    {
                        return Ok(msg);
                    }
                    // Filtered out — loop for the next message.
                }
                Err(err) => return Err(crate::types::TryRecvError::from(err)),
            }
        }
    }
}

pub struct TopicMessenger;

impl TopicMessenger {
    /// Subscribe to a topic published by a specific target. `from_target`
    /// `Some(SenderTarget)` filters on the publisher's identity; `None`
    /// wildcards the target segment (any node or interface emits a match).
    /// `is_from_any` marks this subscription as a `from_any: true` slot,
    /// gating the messenger's per-`(name, tag)` reservation. The
    /// [`ConsumerFilter`] selects whether the wire pins a producer's full
    /// `(core_node, instance_id)` or wildcards plus an in-process pair
    /// filter; see the enum's variants. There is no separate core_node
    /// parameter — producer identity always travels as the whole pair.
    #[allow(clippy::too_many_arguments)]
    pub async fn subscribe(
        messenger: &MessengerHandle,
        as_core_node: &str,
        as_instance_id: &str,
        from_target: Option<SenderTarget>,
        is_from_any: bool,
        to_topic: &str,
        filter: &ConsumerFilter,
        qos: QoSProfile,
    ) -> Result<Subscription> {
        // Translate the ConsumerFilter into the wire-side producer pin
        // (both keyexpr slots) plus an optional in-process pair filter.
        let (wire_from_producer, accept_filter): (Option<&ProducerRef>, _) = match filter {
            ConsumerFilter::Pin(producer) => (Some(producer), None),
            ConsumerFilter::OnlyFrom(producers) if producers.len() == 1 => {
                (Some(&producers[0]), None)
            }
            ConsumerFilter::OnlyFrom(producers) => (
                None,
                Some(AcceptanceFilter::Allow(producers.iter().cloned().collect())),
            ),
            ConsumerFilter::AnyExcept(producers) if producers.is_empty() => (None, None),
            ConsumerFilter::AnyExcept(producers) => (
                None,
                Some(AcceptanceFilter::Deny(producers.iter().cloned().collect())),
            ),
            ConsumerFilter::Any => (None, None),
        };

        // Reserve the from_any `(name, tag)` slot before any wire work.
        // The manifest validator enforces "at most one from_any topic
        // sub per (name, tag) per messenger" at config time; this is
        // the runtime guard at the wire's trust boundary.
        let from_any_guard = match (&from_target, is_from_any) {
            (Some(target), true) => Some(messenger.reserve_from_any_topic(
                wire_from_producer,
                target.name(),
                target.tag(),
            )?),
            _ => None,
        };
        let recv = TopicWireReceiver::new(
            as_core_node,
            as_instance_id,
            wire_from_producer.map(|p| p.core_node.as_str()),
            wire_from_producer.map(|p| p.instance_id.as_str()),
            from_target,
            None,
            to_topic,
        )?;
        let subscription = messenger.subscribe_to_topic(&recv, qos).await?;
        let mut subscription = Subscription::new(subscription);
        if let Some(guard) = from_any_guard {
            subscription = subscription.with_from_any_guard(guard);
        }
        if let Some(filter) = accept_filter {
            subscription = subscription.with_accept_filter(filter);
        }
        Ok(subscription)
    }

    /// Waits until a subscriber for this topic is known to the publisher's
    /// session, or `timeout` elapses; returns whether a match was observed.
    ///
    /// In peer mode a freshly-connected publisher learns about existing
    /// subscribers through gossip, which is not instantaneous, so its first
    /// publish can be dropped before discovery propagates. Call
    /// this first when the very first publish must reach an already-running
    /// subscriber; it returns as soon as a match is observed (no fixed sleep).
    /// A `false` return means no subscriber appeared within `timeout`.
    pub async fn wait_for_subscriber(
        messenger: &MessengerHandle,
        as_core_node: &str,
        as_instance_id: &str,
        as_target: SenderTarget,
        as_topic_name: &str,
        timeout: std::time::Duration,
    ) -> Result<bool> {
        let sender =
            TopicWireSender::new(as_core_node, as_instance_id, as_target, None, as_topic_name)?;
        messenger
            .wait_for_matching_subscriber(&sender, timeout)
            .await
    }

    /// Declares a topic publisher bound under a single producer-side link_id,
    /// bypassing the central `Messenger` mutex on every subsequent publish.
    /// `link_id` `None` falls back to the reserved default `_` segment.
    ///
    /// This is the only topic-publish path: declare a publisher once, then
    /// call [`TopicPublisher::publish`] per message. The publisher always tags
    /// its publishes as primary on the wire.
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
