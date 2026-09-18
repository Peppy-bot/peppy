use super::{MessengerHandle, PeerInfo, ProducerRef};
use crate::error::{Error, Result};
use crate::runtime::CancellationToken;
use crate::types::{Message, Payload};
use config::node::QoSProfile;
use pmi::{
    MessengerPublisher, PairingRecipient, SenderTarget, TopicWireReceiver, TopicWireSender,
    WirePeer,
};

use std::sync::Arc;
use tracing::warn;

/// A consumer-side topic subscription. Producer selection happens entirely
/// on the wire — a dep-slot subscription pins the bound producer's full
/// `(core_node, instance_id)` pair in the keyexpr, and infra subscriptions
/// deliberately wildcard it — so every message the wire delivers surfaces
/// to user code; there is no in-process producer filtering.
pub struct Subscription {
    inner: pmi::Subscription,
}

impl Subscription {
    pub(crate) fn new(inner: pmi::Subscription) -> Self {
        Self { inner }
    }

    pub async fn on_next_message(&mut self) -> Option<Message> {
        let raw = self.inner.rx.recv_async().await.ok()?;
        Some(Message::from(raw))
    }

    pub(crate) fn try_on_next_message(
        &mut self,
    ) -> std::result::Result<Message, crate::types::TryRecvError> {
        match self.inner.rx.try_recv() {
            Ok(raw) => Ok(Message::from(raw)),
            Err(err) => Err(crate::types::TryRecvError::from(err)),
        }
    }

    /// The underlying wire receiver, so a pinned slot's subscription set can go
    /// through [`recv_first_ready`] (see [`crate::runtime::slot_stream`]).
    pub(crate) fn wire_receiver(&self) -> &flume::Receiver<pmi::TopicMessage> {
        &self.inner.rx
    }
}

/// First-ready-wins receive across a set of wire subscriptions, polled in a
/// rotated order starting at `start` so a busy source cannot indefinitely
/// starve a quiet one. Returns the winning index into `sources` and its receive
/// result; callers advance their own rotation cursor once per call.
///
/// The one fan-in rule for every multi-source consumer: a dep slot's bound
/// producer set ([`BoundSetSubscription`]) and a pinned slot's followed member
/// set ([`crate::runtime::slot_stream`]) merge identically, and differ only in
/// the arm each races this against (shutdown vs. slot update).
///
/// This is the per-message receive path of every consumed topic, so it
/// allocates no boxed futures: flume's `RecvFut` is `Unpin` (it goes into
/// `select_all` as-is) and cancel-safe, so the losing futures drop without
/// consuming a message. The ubiquitous single-source set recvs directly, with
/// no future collection at all.
///
/// `sources` must be non-empty; both callers park on their own idle path
/// rather than polling an empty set.
pub(crate) async fn recv_first_ready<T>(
    sources: &[T],
    receiver_of: impl Fn(&T) -> &flume::Receiver<pmi::TopicMessage>,
    start: usize,
) -> (
    usize,
    std::result::Result<pmi::TopicMessage, flume::RecvError>,
) {
    let len = sources.len();
    if len == 1 {
        return (0, receiver_of(&sources[0]).recv_async().await);
    }
    let start = start % len;
    let recvs: Vec<_> = (0..len)
        .map(|offset| receiver_of(&sources[(start + offset) % len]).recv_async())
        .collect();
    let (received, position, _) = futures::future::select_all(recvs).await;
    ((start + position) % len, received)
}

/// One producer's pinned wire subscription inside a
/// [`BoundSetSubscription`]: the producer tag yielded with every message,
/// plus the underlying subscription whose keyexpr pins that producer's full
/// `(core_node, instance_id)` pair.
struct BoundSource {
    producer: ProducerRef,
    subscription: pmi::Subscription,
}

/// A consumer-side subscription covering a dep slot's complete bound
/// producer set: one producer-pinned wire subscription per member, merged
/// client-side. The producer segments of a keyexpr are never wildcarded, so
/// a federated router forwards traffic only for the explicitly bound
/// producers and every subscriber stays fully pinned (and auditable) in the
/// zenoh admin space.
///
/// Merge semantics:
/// - Message order is preserved independently per producer; no total
///   ordering across producers is promised.
/// - Ready producers are merged fairly (rotating poll order), so one busy
///   producer cannot indefinitely starve another.
/// - A source whose channel fails is dropped with a warning naming the
///   producer; unrelated sources keep delivering, and the slot's bound
///   set is never mutated.
/// - Queued messages drain before shutdown is honored; once the node's
///   cancellation token fires and no message is ready, `on_next_message`
///   returns `None`. An empty set (a `zero_or_more` slot the application
///   bound nothing to, or a vacant `zero_or_one` slot) therefore stays
///   pending until shutdown and then returns `None`.
/// - Dropping the subscription closes every underlying wire subscription.
pub struct BoundSetSubscription {
    sources: Vec<BoundSource>,
    /// Rotating first-poll position: source `i` is polled first every
    /// `sources.len()`-th call, which keeps the merge fair when several
    /// producers are ready at once.
    next_start: usize,
    shutdown: CancellationToken,
}

impl BoundSetSubscription {
    /// The next message from any bound producer, tagged with the producer
    /// that published it. Returns `None` once the node is shutting down and
    /// no queued message remains (immediately-queued messages still win
    /// over a fired cancellation token), or when every source has closed.
    pub async fn on_next_message(&mut self) -> Option<(ProducerRef, Message)> {
        loop {
            if self.sources.is_empty() {
                // An empty set has nothing to yield: pend until shutdown so
                // the consumer loop parks instead of spinning.
                self.shutdown.cancelled().await;
                return None;
            }

            let start = self.next_start;
            self.next_start = self.next_start.wrapping_add(1);

            // `biased` polls the sources before the shutdown token, so queued
            // messages drain before a fired cancellation is honored.
            let outcome = tokio::select! {
                biased;
                (idx, received) = recv_first_ready(
                    &self.sources,
                    |source| &source.subscription.rx,
                    start,
                ) => Some((idx, received)),
                _ = self.shutdown.cancelled() => None,
            };

            match outcome {
                None => return None,
                Some((idx, Ok(raw))) => {
                    return Some((self.sources[idx].producer.clone(), Message::from(raw)));
                }
                Some((idx, Err(_))) => {
                    // One source's channel closed. Report it with producer
                    // context and keep serving the unrelated sources; the
                    // slot's bound set itself is startup-fixed and unchanged.
                    let gone = self.sources.remove(idx);
                    warn!(
                        core_node = %gone.producer.core_node,
                        instance_id = %gone.producer.instance_id,
                        "bound producer's subscription channel closed; \
                         continuing with the remaining bound producers"
                    );
                    if self.sources.is_empty() {
                        return None;
                    }
                }
            }
        }
    }
}

pub struct TopicMessenger;

impl TopicMessenger {
    /// Subscribe to a topic published by a specific target. `from_target`
    /// filters on the publisher's identity — consumer dep slots always
    /// know the producer's node / interface target; a stream with no
    /// target to consult is an infra topic and goes through
    /// [`Self::subscribe_target_scoped`].
    /// `from_producer` is one producer: its full `(core_node, instance_id)`
    /// pair is pinned on the wire, so only that producer's publishes ever
    /// reach this subscription. There is no separate core_node parameter —
    /// producer identity always travels as the whole pair. Generated
    /// consumed topics never splice this: they go through
    /// [`Self::subscribe_bound_set`], which covers the slot's complete
    /// bound set for every cardinality.
    pub async fn subscribe(
        messenger: &MessengerHandle,
        as_core_node: &str,
        as_instance_id: &str,
        from_target: SenderTarget,
        to_topic: &str,
        from_producer: &ProducerRef,
        qos: QoSProfile,
    ) -> Result<Subscription> {
        let subscription = Self::subscribe_pinned(
            messenger,
            as_core_node,
            as_instance_id,
            from_target,
            to_topic,
            from_producer,
            qos,
        )
        .await?;
        Ok(Subscription::new(subscription))
    }

    /// One producer-pinned wire subscription: the single wire rule shared
    /// by [`Self::subscribe`] and [`Self::subscribe_bound_set`]. The
    /// producer's full `(core_node, instance_id)` pair is pinned in the
    /// keyexpr — never wildcarded — so only that producer's publishes ever
    /// reach the subscription.
    async fn subscribe_pinned(
        messenger: &MessengerHandle,
        as_core_node: &str,
        as_instance_id: &str,
        from_target: SenderTarget,
        to_topic: &str,
        from_producer: &ProducerRef,
        qos: QoSProfile,
    ) -> Result<pmi::Subscription> {
        let recv = TopicWireReceiver::new(
            as_core_node,
            as_instance_id,
            Some(from_producer.core_node.as_str()),
            Some(from_producer.instance_id.as_str()),
            Some(from_target),
            None,
            to_topic,
        )?;
        messenger.subscribe_to_topic(&recv, qos).await
    }

    /// Subscribe to a topic across a dep slot's complete bound producer
    /// set. The wire follows the same rule as [`Self::subscribe`], once per
    /// member: one subscription per bound producer, each pinning the full
    /// `(core_node, instance_id)` pair in its keyexpr, merged client-side
    /// behind one [`BoundSetSubscription`]. An empty set (a `zero_or_more`
    /// slot with no binding, or a vacant `zero_or_one` slot) opens zero
    /// subscriptions and the returned subscription yields nothing until
    /// `shutdown` fires. Wildcarding the
    /// producer segments and filtering in-process is deliberately not
    /// offered: it would express interest in every same-namespace producer
    /// of the contract, pulling unbound producers' traffic across a
    /// federated mesh and making the bound set unauditable on the wire.
    ///
    /// `shutdown` is the node's cancellation token: it bounds the empty-set
    /// wait and lets a non-empty subscription return `None` at node stop
    /// after draining queued messages.
    #[allow(clippy::too_many_arguments)]
    pub async fn subscribe_bound_set(
        messenger: &MessengerHandle,
        as_core_node: &str,
        as_instance_id: &str,
        from_target: SenderTarget,
        to_topic: &str,
        bound_producers: &[ProducerRef],
        qos: QoSProfile,
        shutdown: CancellationToken,
    ) -> Result<BoundSetSubscription> {
        let mut sources = Vec::with_capacity(bound_producers.len());
        for producer in bound_producers {
            let subscription = Self::subscribe_pinned(
                messenger,
                as_core_node,
                as_instance_id,
                from_target.clone(),
                to_topic,
                producer,
                qos.clone(),
            )
            .await?;
            sources.push(BoundSource {
                producer: producer.clone(),
                subscription,
            });
        }
        Ok(BoundSetSubscription {
            sources,
            next_start: 0,
            shutdown,
        })
    }

    /// Subscribe to a framework infra topic, scoped by the publisher's
    /// target identity alone while its per-boot `(core_node, instance_id)`
    /// pair stays wildcarded on the wire. Used for streams whose producer
    /// identity is unknowable or deliberately open: a node following its
    /// daemon's `clock` / `daemon_heartbeat` (a daemon's node name IS its
    /// core_node name, so the target pins which daemon matches), the
    /// daemon's own name-collision watch (the point is to hear foreign
    /// publishers), an external simulator's clock, and the benchmark
    /// prober. Deliberately separate from [`Self::subscribe`]: consumer
    /// dep slots only ever receive from explicitly bound producers, while
    /// infra topics have no binding to consult.
    pub async fn subscribe_target_scoped(
        messenger: &MessengerHandle,
        as_core_node: &str,
        as_instance_id: &str,
        from_target: SenderTarget,
        to_topic: &str,
        qos: QoSProfile,
    ) -> Result<Subscription> {
        Self::subscribe_target_scoped_with_link_id(
            messenger,
            as_core_node,
            as_instance_id,
            from_target,
            None,
            to_topic,
            qos,
        )
        .await
    }

    /// [`Self::subscribe_target_scoped`] for an infra topic whose publisher
    /// claims one producer-side `link_id`: the segment is a literal, while the
    /// publisher's `(core_node, instance_id)` pair stays wildcarded.
    ///
    /// A daemon publishes `clock` and `daemon_heartbeat` under the reserved
    /// default segment ([`pmi::DEFAULT_LINK_ID`]), and a clock domain hosted on
    /// the same machine publishes on the same topic and target under the
    /// domain's own `link_id`. Naming the segment is what keeps the two streams
    /// apart for a subscriber that cannot know the daemon's per-boot
    /// `instance_id`.
    pub(crate) async fn subscribe_target_scoped_with_link_id(
        messenger: &MessengerHandle,
        as_core_node: &str,
        as_instance_id: &str,
        from_target: SenderTarget,
        from_link_id: Option<&str>,
        to_topic: &str,
        qos: QoSProfile,
    ) -> Result<Subscription> {
        let recv = TopicWireReceiver::new(
            as_core_node,
            as_instance_id,
            None,
            None,
            Some(from_target),
            from_link_id,
            to_topic,
        )?;
        let subscription = messenger.subscribe_to_topic(&recv, qos).await?;
        Ok(Subscription::new(subscription))
    }

    /// Subscribe to one topic of a pairing, pinned to a producer's full wire
    /// triple: `(core_node, instance_id)` plus the producer-side link_id segment
    /// of its publishes. Unlike [`Self::subscribe`], the link_id slot is a
    /// literal, never a wildcard. Pairing traffic rides the `pairing` wire
    /// discriminator, which no interface subscription can match.
    ///
    /// This is the shared pinned-subscribe seam for both pairing forms: a
    /// participant pins its current peer (an unpaired slot has no wire
    /// subscription at all, so there is no wildcard shape to build), and an
    /// observer pins its configured source, and the one peer of it when the
    /// observation is pinned to a pair. Both are pairing-discriminator
    /// traffic and both pin every wire slot, so the `is_pairing` assertion holds
    /// for either caller. Applications never construct raw key expressions;
    /// every pinned subscription comes through this seam.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn subscribe_peer_pinned(
        messenger: &MessengerHandle,
        as_core_node: &str,
        as_instance_id: &str,
        pairing_target: SenderTarget,
        peer: &ProducerRef,
        peer_link_id: &str,
        recipient: PairingRecipient,
        to_topic: &str,
        qos: QoSProfile,
    ) -> Result<Subscription> {
        let producer = WirePeer::new(&peer.core_node, &peer.instance_id, peer_link_id)?;
        let recv = match recipient {
            PairingRecipient::Slot(own_link_id) => TopicWireReceiver::paired(
                as_core_node,
                as_instance_id,
                pairing_target,
                &producer,
                own_link_id.as_str(),
                to_topic,
            )?,
            PairingRecipient::Any => TopicWireReceiver::observing(
                as_core_node,
                as_instance_id,
                pairing_target,
                &producer,
                to_topic,
            )?,
            PairingRecipient::Peer(peer) => TopicWireReceiver::observing_pair(
                as_core_node,
                as_instance_id,
                pairing_target,
                &producer,
                &peer,
                to_topic,
            )?,
        };
        let subscription = messenger.subscribe_to_topic(&recv, qos).await?;
        Ok(Subscription::new(subscription))
    }

    /// Subscribe to one topic pinned to the exact publisher that emits it:
    /// its `(core_node, instance_id)` plus the producer-side `link_id`
    /// segment, on a target the caller names.
    ///
    /// This is how a node follows one clock domain. A domain's ticks ride the
    /// ordinary `clock` topic of the machine hosting the domain, under the
    /// domain's own `link_id`, so they share that topic with the daemon's wall
    /// ticks, which ride the reserved default segment. Every subscriber names
    /// the segment it reads: a domain's here, the daemon's through
    /// [`Self::subscribe_target_scoped_with_link_id`], so each stream carries
    /// one publisher's ticks. Pinning every slot is also what makes a stale
    /// lifetime's ticks unreachable: they carry a different `link_id`.
    ///
    /// Distinct from [`Self::subscribe_peer_pinned`], which asserts a
    /// pairing-shaped target and names the recipient it stands for, and from
    /// [`Self::subscribe_target_scoped`], which leaves the publisher's identity
    /// and `link_id` wildcarded.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn subscribe_publisher_pinned(
        messenger: &MessengerHandle,
        as_core_node: &str,
        as_instance_id: &str,
        from: &ProducerRef,
        from_target: SenderTarget,
        from_link_id: &str,
        to_topic: &str,
        qos: QoSProfile,
    ) -> Result<Subscription> {
        let recv = TopicWireReceiver::new(
            as_core_node,
            as_instance_id,
            Some(from.core_node.as_str()),
            Some(from.instance_id.as_str()),
            Some(from_target),
            Some(from_link_id),
            to_topic,
        )?;
        let subscription = messenger.subscribe_to_topic(&recv, qos).await?;
        Ok(Subscription::new(subscription))
    }

    /// Declares the publisher for pairing emissions from this node's scalar
    /// slot `link_id`: one wire publisher addressed to whichever peer holds
    /// the slot's one pair, so a publish while unpaired reaches nobody.
    pub async fn declare_sole_peer_publisher(
        messenger: &MessengerHandle,
        as_core_node: &str,
        as_instance_id: &str,
        pairing_target: SenderTarget,
        link_id: &str,
        as_topic_name: &str,
        qos: QoSProfile,
    ) -> Result<TopicPublisher> {
        let sender = TopicWireSender::to_sole_peer(
            as_core_node,
            as_instance_id,
            pairing_target,
            link_id,
            as_topic_name,
        )?;
        let inner = messenger
            .declare_topic_publisher(&sender, qos.into())
            .await?;
        Ok(TopicPublisher::new(Arc::new(inner)))
    }

    /// Whether a subscriber matching a pairing emission from this node's
    /// slot `link_id` to `peer` is visible, within `timeout`.
    #[allow(clippy::too_many_arguments)]
    pub async fn wait_for_pairing_subscriber(
        messenger: &MessengerHandle,
        as_core_node: &str,
        as_instance_id: &str,
        pairing_target: SenderTarget,
        link_id: &str,
        as_topic_name: &str,
        peer: &PeerInfo,
        timeout: std::time::Duration,
    ) -> Result<bool> {
        let sender = pairing_sender(
            as_core_node,
            as_instance_id,
            pairing_target,
            link_id,
            as_topic_name,
            peer,
        )?;
        messenger
            .wait_for_matching_subscriber(&sender, timeout)
            .await
    }

    /// Declares the publisher for pairing emissions from this node's slot
    /// `link_id` to `peer`: one wire publisher per pair, which is what a
    /// [`crate::runtime::PeerPublisher`] keeps per peer of its slot.
    #[allow(clippy::too_many_arguments)]
    pub async fn declare_pairing_publisher(
        messenger: &MessengerHandle,
        as_core_node: &str,
        as_instance_id: &str,
        pairing_target: SenderTarget,
        link_id: &str,
        as_topic_name: &str,
        qos: QoSProfile,
        peer: &PeerInfo,
    ) -> Result<TopicPublisher> {
        let sender = pairing_sender(
            as_core_node,
            as_instance_id,
            pairing_target,
            link_id,
            as_topic_name,
            peer,
        )?;
        let inner = messenger
            .declare_topic_publisher(&sender, qos.into())
            .await?;
        Ok(TopicPublisher::new(Arc::new(inner)))
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
        Self::wait_for_subscriber_with_link_id(
            messenger,
            as_core_node,
            as_instance_id,
            as_target,
            None,
            as_topic_name,
            timeout,
        )
        .await
    }

    /// [`Self::wait_for_subscriber`] for a publisher bound under a concrete
    /// producer-side `link_id` (a pairing slot publisher, or any
    /// `--link-id`-scoped publisher): the match is checked against the same
    /// keyexpr the publisher will emit on, link_id segment included.
    pub async fn wait_for_subscriber_with_link_id(
        messenger: &MessengerHandle,
        as_core_node: &str,
        as_instance_id: &str,
        as_target: SenderTarget,
        link_id: Option<&str>,
        as_topic_name: &str,
        timeout: std::time::Duration,
    ) -> Result<bool> {
        let sender = TopicWireSender::new(
            as_core_node,
            as_instance_id,
            as_target,
            link_id,
            as_topic_name,
        )?;
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

/// The wire sender of a pairing emission: this node's slot to one peer.
fn pairing_sender(
    as_core_node: &str,
    as_instance_id: &str,
    pairing_target: SenderTarget,
    link_id: &str,
    as_topic_name: &str,
    peer: &PeerInfo,
) -> Result<TopicWireSender> {
    let to_peer = WirePeer::new(
        &peer.producer.core_node,
        &peer.producer.instance_id,
        &peer.peer_link_id,
    )?;
    Ok(TopicWireSender::to_peer(
        as_core_node,
        as_instance_id,
        pairing_target,
        link_id,
        as_topic_name,
        to_peer,
    )?)
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
