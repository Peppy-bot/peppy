//! Consumer-side runtime for pairing slots: [`PeerSlot`] and [`PeerSlotSet`]
//! (observe a slot's peer set), [`PeerSubscription`] (receive what the peers
//! publish) and [`PeerPublisher`] (publish to each peer).
//!
//! A `PeerSubscription` follows the slot's peers through the shared
//! [`crate::runtime::slot_stream`] engine, which keeps one wire subscription
//! per peer converged with the slot's live set (no peer, no subscription; a
//! peer added, one subscription pinned to it and to this slot; a peer gone,
//! its subscription dropped plus a delivery-time stale filter). This module
//! supplies what a pairing slot follows: the peers themselves.

use crate::error::{Error, Result};
use crate::messaging::{
    MessengerHandle, PeerInfo, PeerMember, PeerSetState, ProducerRef, SenderTarget, TopicMessenger,
    TopicPublisher,
};
use crate::runtime::NodeRunner;
use crate::runtime::slot_stream::{FollowedSlot, SlotStream, spawn_slot_stream};
use crate::types::{Message, Payload};
use config::node::{Cardinality, QoSProfile};
use pmi::PairingRecipient;
use tokio::sync::watch;

/// A scalar accessor read on a slot whose set cannot fit it: the generated
/// code and the manifest disagree, which regenerating the node's bindings
/// fixes.
pub(crate) fn pairing_shape_panic(link_id: &str, accessor: &str, cardinality: &str) -> ! {
    panic!(
        "pairing slot `{link_id}` declares cardinality `{cardinality}`, which `{accessor}` cannot read; \
         regenerate the node's bindings so the slot is read through the accessor its cardinality declares"
    )
}

/// Handle onto a scalar pairing slot's live state (a `one` or `zero_or_one`
/// slot). Obtained via [`NodeRunner::peer`]; the generated per-slot modules
/// of those slots expose `paired()` / `wait_paired()` delegating here.
#[derive(Clone)]
pub struct PeerSlot {
    link_id: String,
    cardinality: Cardinality,
    watch_rx: watch::Receiver<PeerSetState>,
}

impl PeerSlot {
    pub(crate) fn new(
        link_id: impl Into<String>,
        cardinality: Cardinality,
        watch_rx: watch::Receiver<PeerSetState>,
    ) -> Self {
        Self {
            link_id: link_id.into(),
            cardinality,
            watch_rx,
        }
    }

    /// The currently paired peer with its copy, or `None` while unpaired.
    ///
    /// Panics if the slot holds more than one pair, which a scalar slot
    /// cannot: reading a multi slot through this accessor is stale codegen.
    pub fn paired_member(&self) -> Option<PeerMember> {
        let state = self.watch_rx.borrow();
        match state.members.as_slice() {
            [] => None,
            [sole] => Some(sole.clone()),
            _ => pairing_shape_panic(&self.link_id, "paired()", self.cardinality.as_str()),
        }
    }

    /// The currently paired peer, or `None` while the slot is unpaired.
    pub fn paired(&self) -> Option<PeerInfo> {
        self.paired_member().map(|member| member.info)
    }

    /// Waits until the slot is paired and returns the peer. Returns
    /// immediately when already paired. Errors only if the runtime is torn
    /// down while waiting (the slot channel closed).
    pub async fn wait_paired(&mut self) -> Result<PeerInfo> {
        loop {
            if let Some(peer) = self.paired() {
                return Ok(peer);
            }
            self.watch_rx
                .changed()
                .await
                .map_err(|_| Error::PairingSlotClosed)?;
        }
    }
}

/// Handle onto a multi-peer pairing slot's live state (a `one_or_more` or
/// `zero_or_more` slot). Obtained via [`NodeRunner::peer_set`]; the generated
/// per-slot modules of those slots expose `peers()` delegating here.
///
/// The set is live: pairs join it when a peer's instance starts and leave it
/// when either side stops, so a set read now can differ from one read later.
/// A `one_or_more` slot's floor is a plan-time guarantee that at least one
/// peer is paired to it; the set still empties when every peer stops.
#[derive(Clone)]
pub struct PeerSlotSet {
    watch_rx: watch::Receiver<PeerSetState>,
}

impl PeerSlotSet {
    pub(crate) fn new(watch_rx: watch::Receiver<PeerSetState>) -> Self {
        Self { watch_rx }
    }

    /// Every pair the slot currently holds, in establishment order, each with
    /// the copy its peer belongs to. Empty while nothing has paired.
    pub fn members(&self) -> Vec<PeerMember> {
        self.watch_rx.borrow().members.clone()
    }

    /// The identity of every peer the slot currently holds, in
    /// establishment order.
    pub fn peers(&self) -> Vec<PeerInfo> {
        self.watch_rx.borrow().peers().cloned().collect()
    }
}

/// The pairing slot kind for the shared [`slot_stream`] engine: a pairing slot
/// follows one pin per pair it holds, so its followed set is empty while
/// unpaired and grows and shrinks with the slot's pairs.
///
/// [`slot_stream`]: crate::runtime::slot_stream
pub(crate) struct PeerFollow;

impl FollowedSlot for PeerFollow {
    type State = PeerSetState;
    type Pin = PeerInfo;

    fn desired(state: &PeerSetState) -> Vec<PeerInfo> {
        state.peers().cloned().collect()
    }

    fn is_followed(state: &PeerSetState, pin: &PeerInfo) -> bool {
        state.peers().any(|peer| peer == pin)
    }

    fn producer(pin: &PeerInfo) -> &ProducerRef {
        &pin.producer
    }

    fn producer_link_id(pin: &PeerInfo) -> &str {
        &pin.peer_link_id
    }
}

/// Stream of the paired peers' publishes on one topic of a pairing slot.
/// Yields nothing while the slot holds no pair; delivery starts from the
/// pairing moment, never retroactively: a pairing is a live stream, not a
/// mailbox.
pub struct PeerSubscription {
    stream: SlotStream<PeerFollow>,
}

impl PeerSubscription {
    /// Waits for the next `(peer, message)` from any currently paired peer:
    /// the peer's identity (the same [`PeerInfo`] the slot's accessors
    /// enumerate) and the message it published. Returns `None` when the
    /// runtime is torn down (slot channel closed). Messages buffered under a
    /// peer the slot has since dropped never surface (see
    /// [`SlotStream::next`]).
    pub async fn on_next_message(&mut self) -> Option<(PeerInfo, Message)> {
        self.stream
            .next()
            .await
            .map(|(pin, message)| ((*pin).clone(), message))
    }
}

/// Subscribe to one peer-emitted topic of the pairing slot at `link_id`.
/// Spliced by the generated `peppygen::paired_topics::<link_id>::<topic>::subscribe`
/// call sites; `pairing_name` / `pairing_tag` / `topic` come from the
/// pairing doc via codegen constants.
pub async fn subscribe_peer(
    node_runner: &NodeRunner,
    link_id: &str,
    pairing_name: &str,
    pairing_tag: &str,
    topic: &str,
    qos: QoSProfile,
) -> Result<PeerSubscription> {
    let processor = node_runner.processor();
    let watch_rx = processor
        .peer_set_watch(link_id)
        .ok_or_else(|| Error::UnknownPairingSlot {
            link_id: link_id.to_string(),
        })?;
    let target = SenderTarget::pairing(pairing_name, pairing_tag)?;
    subscribe_peer_with_watch(
        node_runner.messenger().clone(),
        processor.bound_core_node().to_string(),
        processor.bound_instance_id().to_string(),
        link_id.to_string(),
        watch_rx,
        target,
        topic.to_string(),
        qos,
    )
}

/// Messenger-level core of [`subscribe_peer`]: the same forwarding-task
/// machinery driven by an explicit watch channel instead of a `NodeRunner`'s
/// processor-owned slot. `own_link_id` is this node's slot in every pair, the
/// recipient each peer publishes to. Prefer [`subscribe_peer`] in nodes; this
/// seam exists for embedders and tests that manage peer state themselves.
#[allow(clippy::too_many_arguments)]
pub fn subscribe_peer_with_watch(
    messenger: MessengerHandle,
    as_core_node: String,
    as_instance_id: String,
    own_link_id: String,
    watch_rx: watch::Receiver<PeerSetState>,
    pairing_target: SenderTarget,
    topic: String,
    qos: QoSProfile,
) -> Result<PeerSubscription> {
    let recipient = PairingRecipient::Slot(
        pmi::Segment::try_link_id(&own_link_id)
            .map_err(|e| Error::PeppyMessagingInterface(e.into()))?,
    );
    Ok(PeerSubscription {
        stream: spawn_slot_stream::<PeerFollow>(
            messenger,
            as_core_node,
            as_instance_id,
            watch_rx,
            pairing_target,
            recipient,
            topic,
            qos,
        ),
    })
}

/// Publisher on one topic of a multi pairing slot. A pairing emission from a
/// slot holding several pairs names the peer it is for, so the publisher
/// keeps one declared wire publisher per peer the slot currently holds,
/// converged with the slot's live set at each publish, and
/// [`publish_to`](Self::publish_to) names the peer. A scalar slot publishes
/// through the [`TopicPublisher`] [`declare_sole_peer_publisher`] declares,
/// addressed to whichever peer holds its one pair.
pub struct PeerPublisher {
    messenger: MessengerHandle,
    as_core_node: String,
    as_instance_id: String,
    link_id: String,
    pairing_target: SenderTarget,
    topic: String,
    qos: QoSProfile,
    watch_rx: watch::Receiver<PeerSetState>,
    /// One declared wire publisher per peer, in the slot's order.
    declared: tokio::sync::Mutex<Vec<(PeerInfo, TopicPublisher)>>,
}

impl PeerPublisher {
    /// The identity of every peer the slot currently holds.
    pub fn peers(&self) -> Vec<PeerInfo> {
        self.watch_rx.borrow().peers().cloned().collect()
    }

    /// Publishes to `peer`, one of the pairs the slot currently holds; a peer
    /// the slot does not hold is refused, so a message never leaves for a
    /// pair that ended.
    pub async fn publish_to(&self, peer: &PeerInfo, payload: Payload) -> Result<()> {
        let publisher = self.publisher_for(peer).await?;
        publisher.publish(payload).await
    }

    /// The declared wire publisher for `peer`, after converging the declared
    /// set with the slot's current peers: a peer gone loses its publisher, a
    /// peer new to the set gets one.
    async fn publisher_for(&self, peer: &PeerInfo) -> Result<TopicPublisher> {
        let mut declared = self.declared.lock().await;
        let peers: Vec<PeerInfo> = self.watch_rx.borrow().peers().cloned().collect();
        declared.retain(|(known, _)| peers.contains(known));
        if !peers.contains(peer) {
            return Err(Error::PeerNotPaired {
                link_id: self.link_id.clone(),
                peer: format!(
                    "{}/{} (slot `{}`)",
                    peer.producer.core_node, peer.producer.instance_id, peer.peer_link_id
                ),
            });
        }
        if let Some((_, publisher)) = declared.iter().find(|(known, _)| known == peer) {
            return Ok(publisher.clone());
        }
        let publisher = TopicMessenger::declare_pairing_publisher(
            &self.messenger,
            &self.as_core_node,
            &self.as_instance_id,
            self.pairing_target.clone(),
            &self.link_id,
            &self.topic,
            self.qos.clone(),
            peer,
        )
        .await?;
        declared.push((peer.clone(), publisher.clone()));
        Ok(publisher)
    }
}

/// Declares the publisher for one topic of the scalar pairing slot at
/// `link_id`: a plain [`TopicPublisher`] whose `publish` reaches the one
/// paired peer and is a no-op while unpaired. Spliced by the generated
/// `peppygen::paired_topics::<link_id>::<topic>::declare_publisher` call
/// sites of scalar slots; `pairing_name` / `pairing_tag` / `topic` come from
/// the pairing doc via codegen constants.
pub async fn declare_sole_peer_publisher(
    node_runner: &NodeRunner,
    link_id: &str,
    pairing_name: &str,
    pairing_tag: &str,
    topic: &str,
    qos: QoSProfile,
) -> Result<TopicPublisher> {
    let processor = node_runner.processor();
    let target = SenderTarget::pairing(pairing_name, pairing_tag)?;
    TopicMessenger::declare_sole_peer_publisher(
        node_runner.messenger(),
        processor.bound_core_node(),
        processor.bound_instance_id(),
        target,
        link_id,
        topic,
        qos,
    )
    .await
}

/// Declares the publisher for one topic of the multi pairing slot at
/// `link_id`. Spliced by the generated
/// `peppygen::paired_topics::<link_id>::<topic>::declare_publisher` call
/// sites of multi slots; `pairing_name` / `pairing_tag` / `topic` come from
/// the pairing doc via codegen constants.
pub fn declare_peer_publisher(
    node_runner: &NodeRunner,
    link_id: &str,
    pairing_name: &str,
    pairing_tag: &str,
    topic: &str,
    qos: QoSProfile,
) -> Result<PeerPublisher> {
    let processor = node_runner.processor();
    let watch_rx = processor
        .peer_set_watch(link_id)
        .ok_or_else(|| Error::UnknownPairingSlot {
            link_id: link_id.to_string(),
        })?;
    let target = SenderTarget::pairing(pairing_name, pairing_tag)?;
    Ok(declare_peer_publisher_with_watch(
        node_runner.messenger().clone(),
        processor.bound_core_node().to_string(),
        processor.bound_instance_id().to_string(),
        link_id.to_string(),
        watch_rx,
        target,
        topic.to_string(),
        qos,
    ))
}

/// Messenger-level core of [`declare_peer_publisher`], driven by an explicit
/// watch channel; the seam for embedders and tests that manage peer state
/// themselves.
#[allow(clippy::too_many_arguments)]
pub fn declare_peer_publisher_with_watch(
    messenger: MessengerHandle,
    as_core_node: String,
    as_instance_id: String,
    link_id: String,
    watch_rx: watch::Receiver<PeerSetState>,
    pairing_target: SenderTarget,
    topic: String,
    qos: QoSProfile,
) -> PeerPublisher {
    PeerPublisher {
        messenger,
        as_core_node,
        as_instance_id,
        link_id,
        pairing_target,
        topic,
        qos,
        watch_rx,
        declared: tokio::sync::Mutex::new(Vec::new()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn member(instance: &str) -> PeerMember {
        PeerMember {
            info: PeerInfo {
                producer: ProducerRef::new("core_a", instance),
                peer_link_id: "controller".to_string(),
            },
            copy: None,
        }
    }

    fn state(sequence: u64, members: Vec<PeerMember>) -> PeerSetState {
        PeerSetState { sequence, members }
    }

    #[tokio::test]
    async fn a_scalar_slot_reports_its_one_pair() {
        let (tx, rx) = watch::channel(PeerSetState::empty());
        let slot = PeerSlot::new("arm", Cardinality::ZeroOrOne, rx);
        assert_eq!(slot.paired(), None);
        tx.send(state(1, vec![member("arm_1")])).unwrap();
        assert_eq!(slot.paired(), Some(member("arm_1").info));
        assert_eq!(slot.paired_member(), Some(member("arm_1")));
    }

    #[tokio::test]
    #[should_panic(expected = "cannot read")]
    async fn a_scalar_accessor_on_a_slot_holding_several_pairs_is_stale_codegen() {
        let (tx, rx) = watch::channel(PeerSetState::empty());
        let slot = PeerSlot::new("arms", Cardinality::ZeroOrMore, rx);
        tx.send(state(1, vec![member("arm_1"), member("arm_2")]))
            .unwrap();
        let _ = slot.paired();
    }

    #[tokio::test]
    async fn a_set_lists_its_pairs_in_order() {
        let (tx, rx) = watch::channel(PeerSetState::empty());
        let set = PeerSlotSet::new(rx);
        assert!(set.peers().is_empty());
        tx.send(state(1, vec![member("arm_2"), member("arm_1")]))
            .unwrap();
        assert_eq!(
            set.peers()
                .iter()
                .map(|peer| peer.producer.instance_id.as_str())
                .collect::<Vec<_>>(),
            ["arm_2", "arm_1"]
        );
        assert_eq!(set.members().first(), Some(&member("arm_2")));
    }

    /// A floored slot's floor is the plan's, not the live set's: every peer
    /// stopping empties it, and reading it then is an ordinary empty read.
    #[tokio::test]
    async fn a_one_or_more_set_reads_empty_once_every_peer_has_gone() {
        let (tx, rx) = watch::channel(PeerSetState::empty());
        let set = PeerSlotSet::new(rx);
        tx.send(state(1, vec![member("arm_1")])).unwrap();
        assert_eq!(set.members().len(), 1);
        tx.send(state(2, Vec::new())).unwrap();
        assert!(set.members().is_empty());
    }

    #[tokio::test]
    async fn wait_paired_returns_immediately_when_already_paired() {
        let (_tx, rx) = watch::channel(state(1, vec![member("arm_1")]));
        let mut slot = PeerSlot::new("arm", Cardinality::One, rx);
        let peer = tokio::time::timeout(Duration::from_millis(100), slot.wait_paired())
            .await
            .expect("no wait needed")
            .expect("paired");
        assert_eq!(peer, member("arm_1").info);
    }

    #[tokio::test]
    async fn wait_paired_wakes_on_live_pair() {
        let (tx, rx) = watch::channel(PeerSetState::empty());
        let mut slot = PeerSlot::new("arm", Cardinality::One, rx);
        let waiter = tokio::spawn(async move { slot.wait_paired().await });
        tokio::task::yield_now().await;
        tx.send(state(1, vec![member("arm_1")])).unwrap();
        let peer = waiter.await.unwrap().unwrap();
        assert_eq!(peer, member("arm_1").info);
    }

    #[tokio::test]
    async fn wait_paired_errors_when_runtime_tears_down() {
        let (tx, rx) = watch::channel(PeerSetState::empty());
        let mut slot = PeerSlot::new("arm", Cardinality::One, rx);
        drop(tx);
        assert!(matches!(
            slot.wait_paired().await,
            Err(Error::PairingSlotClosed)
        ));
    }
}
