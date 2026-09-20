//! Consumer-side runtime for observer pairing slots: [`ObservationSlot`] and
//! [`ObservationSlotSet`] (observe the slot's member set) and
//! [`ObservedTopicSubscription`] (receive the observed sources' publishes on one
//! topic).
//!
//! An observer passively taps a producer's pairing topic without joining the
//! source's 1:1 pairing. It follows its sources through the shared
//! [`crate::runtime::slot_stream`] engine, which keeps one wire subscription per
//! followed pin converged with the slot's member set. Unlike a pairing slot, an
//! observer follows `(source generation, source pin)` per member, not the pin
//! alone: a source's own peer transitions never touch its subscription (the pin
//! is unchanged), while a source-incarnation change advances that member's
//! generation and so redeclares, even though a reused instance_id keeps the wire
//! triple byte-identical, which the keyexpr alone cannot tell apart. This module
//! supplies only that follow rule; the convergence, the fair merge across
//! members, and the stale filter live in the engine.

use crate::error::{Error, Result};
use crate::messaging::{
    MessengerHandle, NonEmptyObservedSources, ObservationState, ObservedSource, ProducerRef,
    SenderTarget,
};
use crate::runtime::NodeRunner;
use crate::runtime::slot_stream::{FollowedSlot, SlotStream, spawn_slot_stream};
use crate::types::Message;
use config::node::QoSProfile;
use pmi::{PairingRecipient, WirePeer};
use tokio::sync::watch;

/// The panic every cardinality-typed observer accessor raises when the shape it
/// was generated for is not the shape the slot has. Mirrors
/// [`crate::runtime::Processor::sole_bound_producer`] on the producer-binding
/// side: an accessor that does not match the manifest is stale codegen, a bug
/// rather than a user error.
pub(crate) fn observer_shape_panic(link_id: &str, accessor: &str, declared: &str) -> ! {
    panic!(
        "observer slot `{link_id}` is declared `{declared}` but was read through `{accessor}`: \
         the generated code and the manifest disagree (version skew / stale codegen); \
         regenerate bindings for this node"
    )
}

/// The panic a floored observer accessor raises when the slot observes nothing.
/// The too-few counterpart of [`observer_shape_panic`]'s too-many case, and the
/// observation twin of [`crate::runtime::Processor::non_empty_bound_producers`]:
/// the launcher sizes the slot at plan time and node startup re-checks the seed
/// against the same rule, so an empty set means the daemon and this node
/// disagree about the manifest rather than that the application has a case to
/// handle.
pub(crate) fn observer_empty_panic(link_id: &str, declared: &str) -> ! {
    panic!(
        "observer slot `{link_id}` is declared `{declared}` but observes nothing: \
         the plan sizes this slot and node startup re-checks its seed, so an empty set means \
         the daemon and the generated code disagree (version skew / stale codegen); \
         regenerate bindings for this node"
    )
}

/// Handle onto a scalar observer slot's live observation state (a `one` or
/// `zero_or_one` slot). Obtained via [`NodeRunner::observation_slot`]; the
/// generated per-slot modules of those slots expose `source()` delegating here.
/// Multi-member slots are read through [`ObservationSlotSet`] instead.
#[derive(Clone)]
pub struct ObservationSlot {
    link_id: String,
    /// The slot's declared cardinality, carried only so a shape panic names the
    /// spelling the manifest actually uses.
    cardinality: config::node::Cardinality,
    watch_rx: watch::Receiver<ObservationState>,
}

impl ObservationSlot {
    pub(crate) fn new(
        link_id: impl Into<String>,
        cardinality: config::node::Cardinality,
        watch_rx: watch::Receiver<ObservationState>,
    ) -> Self {
        Self {
            link_id: link_id.into(),
            cardinality,
            watch_rx,
        }
    }

    /// The observed source of a `zero_or_one` slot, or `None` where the
    /// deployment wrote it vacant. Purely local configuration state; there is no
    /// health-derived helper, because a third node's health is not knowable here
    /// (see the design's "Generated observer API").
    ///
    /// Generated `source()` module functions of `zero_or_one` slots call this; a
    /// `one` slot reads [`Self::sole_source`] instead, which has no empty case.
    ///
    /// Panics if the slot holds more than one member, which a scalar slot cannot
    /// have: reading a multi-member slot through this accessor is stale codegen.
    pub fn source(&self) -> Option<ObservedSource> {
        let state = self.watch_rx.borrow();
        match state.members.as_slice() {
            [] => None,
            [sole] => Some(ObservedSource::from(sole)),
            _ => observer_shape_panic(&self.link_id, "source()", self.cardinality.as_str()),
        }
    }

    /// The sole pairing a `one` slot observes. The plan binds exactly one
    /// pairing to the slot and node startup re-checks its seed against the same
    /// rule, so a member always exists and the accessor needs no `Option`.
    /// Purely local configuration state, on the same terms as [`Self::source`].
    ///
    /// Generated `source()` module functions of `one` slots call this.
    ///
    /// Panics if the slot observes nothing, or more than one pairing: either
    /// means the generated code and the manifest disagree. Mirrors
    /// [`Processor::sole_bound_producer`] on the producer-binding side.
    ///
    /// [`Processor::sole_bound_producer`]: crate::runtime::Processor::sole_bound_producer
    pub fn sole_source(&self) -> ObservedSource {
        self.source()
            .unwrap_or_else(|| observer_empty_panic(&self.link_id, self.cardinality.as_str()))
    }
}

/// Handle onto a multi-member observer slot's live observation state (a
/// `one_or_more` or `zero_or_more` slot). Obtained via
/// [`NodeRunner::observation_slot_set`]; the generated per-slot modules of those
/// slots expose `sources()` delegating here.
///
/// The member set is live in what it says about each member: the daemon owns it
/// and keeps every member's incarnation and liveness current, so a set read now
/// can differ from one read later and a member whose source is down stays in the
/// set, at its position. What does not move is the size: the launcher fixes it
/// at plan time and node startup re-checks the slot's seed against the same
/// rule, so the slot's declared floor holds on every read.
#[derive(Clone)]
pub struct ObservationSlotSet {
    link_id: String,
    /// The slot's declared cardinality, carried only so a shape panic names the
    /// spelling the manifest actually uses.
    cardinality: config::node::Cardinality,
    watch_rx: watch::Receiver<ObservationState>,
}

impl ObservationSlotSet {
    pub(crate) fn new(
        link_id: impl Into<String>,
        cardinality: config::node::Cardinality,
        watch_rx: watch::Receiver<ObservationState>,
    ) -> Self {
        Self {
            link_id: link_id.into(),
            cardinality,
            watch_rx,
        }
    }

    /// Every pairing a `zero_or_more` slot currently observes, in plan order:
    /// the order the launcher's array or the `--link` occurrences wrote,
    /// preserved end to end, so member N here is the deployment's Nth entry for
    /// this slot. The set is empty wherever the plan bound no pairing at all.
    ///
    /// Generated `sources()` module functions of `zero_or_more` slots call this;
    /// a `one_or_more` slot reads [`Self::non_empty_sources`] instead, which has
    /// no empty case.
    pub fn sources(&self) -> Vec<ObservedSource> {
        self.watch_rx
            .borrow()
            .members
            .iter()
            .map(ObservedSource::from)
            .collect()
    }

    /// Every pairing a `one_or_more` slot observes, in plan order, as a set
    /// whose [`first`](NonEmptyObservedSources::first) is infallible. The plan
    /// binds at least one pairing to the slot and node startup re-checks its
    /// seed against the same rule, so the set is never empty.
    ///
    /// Generated `sources()` module functions of `one_or_more` slots call this.
    ///
    /// Panics if the slot observes nothing, which means the generated code and
    /// the manifest disagree. Mirrors [`Processor::non_empty_bound_producers`]
    /// on the producer-binding side.
    ///
    /// [`Processor::non_empty_bound_producers`]: crate::runtime::Processor::non_empty_bound_producers
    pub fn non_empty_sources(&self) -> NonEmptyObservedSources {
        NonEmptyObservedSources::new(self.sources())
            .unwrap_or_else(|| observer_empty_panic(&self.link_id, self.cardinality.as_str()))
    }
}

/// The observer slot kind for the shared [`slot_stream`] engine. An observer
/// follows one `(source generation, source pin)` per member: the pin is the
/// source's wire triple, plus the peer when the member is pinned to one pair,
/// and the generation tells a reused-instance_id incarnation apart from its
/// predecessor, whose publishes are byte-identical on the wire.
///
/// [`slot_stream`]: crate::runtime::slot_stream
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ObservedPin {
    generation: u64,
    source: ObservedSource,
}

pub(crate) struct ObservedFollow;

impl FollowedSlot for ObservedFollow {
    type State = ObservationState;
    type Pin = ObservedPin;

    fn desired(state: &ObservationState) -> Vec<ObservedPin> {
        state
            .members
            .iter()
            .map(|member| ObservedPin {
                generation: member.source_generation,
                source: member.source.clone(),
            })
            .collect()
    }

    fn is_followed(state: &ObservationState, pin: &ObservedPin) -> bool {
        state
            .members
            .iter()
            .any(|member| member.source_generation == pin.generation && member.source == pin.source)
    }

    fn producer(pin: &ObservedPin) -> &ProducerRef {
        &pin.source.producer
    }

    fn producer_link_id(pin: &ObservedPin) -> &str {
        &pin.source.source_link_id
    }

    /// A member pinned to one pair subscribes to the source's publishes to
    /// that peer alone; every other member takes the slot's open recipient.
    fn recipient(pin: &ObservedPin, slot_recipient: &PairingRecipient) -> Result<PairingRecipient> {
        match &pin.source.peer {
            Some(peer) => Ok(PairingRecipient::Peer(WirePeer::new(
                &peer.producer.core_node,
                &peer.producer.instance_id,
                &peer.peer_link_id,
            )?)),
            None => Ok(slot_recipient.clone()),
        }
    }
}

/// Stream of the observed sources' publishes on one topic, fanned in across the
/// slot's whole member set. Yields nothing while the set is empty or no member
/// is emitting; delivery is a live stream, never a mailbox, so messages
/// published before observation became active are never replayed.
pub struct ObservedTopicSubscription {
    stream: SlotStream<ObservedFollow>,
}

impl ObservedTopicSubscription {
    /// Waits for the next `(source, message)` from any currently observed
    /// source incarnation. Every cardinality fans in the same way and every
    /// message is tagged with the [`ObservedSource`] that published it, the
    /// same type [`ObservationSlotSet::sources`] enumerates, so a multi-member
    /// slot's consumer routes on the source identity (wire address plus
    /// producer-side link_id) even when several members share one instance.
    /// Returns `None` when the runtime is torn down (slot channel closed). A
    /// message buffered under a superseded source generation, or under a
    /// member the slot has since dropped, never surfaces (see
    /// [`SlotStream::next`]).
    pub async fn on_next_message(&mut self) -> Option<(ObservedSource, Message)> {
        self.stream
            .next()
            .await
            .map(|(pin, message)| (pin.source.clone(), message))
    }
}

/// Subscribe to one topic emitted by an observer slot's sources, for every
/// cardinality. Spliced by the generated
/// `peppygen::paired_topics::<link_id>::<topic>::subscribe` call sites of
/// observer modules; `pairing_name` / `pairing_tag` / `topic` come from the
/// pairing doc via codegen constants.
pub async fn subscribe_observed(
    node_runner: &NodeRunner,
    link_id: &str,
    pairing_name: &str,
    pairing_tag: &str,
    topic: &str,
    qos: QoSProfile,
) -> Result<ObservedTopicSubscription> {
    let processor = node_runner.processor();
    let watch_rx =
        processor
            .observation_slot_watch(link_id)
            .ok_or_else(|| Error::UnknownObservationSlot {
                link_id: link_id.to_string(),
            })?;
    let target = SenderTarget::pairing(pairing_name, pairing_tag)?;
    Ok(subscribe_observed_with_watch(
        node_runner.messenger().clone(),
        processor.bound_core_node().to_string(),
        processor.bound_instance_id().to_string(),
        watch_rx,
        target,
        topic.to_string(),
        qos,
    ))
}

/// Messenger-level core of [`subscribe_observed`]: the same forwarding-task
/// machinery driven by an explicit watch channel instead of a `NodeRunner`'s
/// processor-owned slot. Prefer [`subscribe_observed`] in nodes; this seam
/// exists for embedders and tests that manage observation state themselves.
pub fn subscribe_observed_with_watch(
    messenger: MessengerHandle,
    as_core_node: String,
    as_instance_id: String,
    watch_rx: watch::Receiver<ObservationState>,
    pairing_target: SenderTarget,
    topic: String,
    qos: QoSProfile,
) -> ObservedTopicSubscription {
    ObservedTopicSubscription {
        stream: spawn_slot_stream::<ObservedFollow>(
            messenger,
            as_core_node,
            as_instance_id,
            watch_rx,
            pairing_target,
            pmi::PairingRecipient::Any,
            topic,
            qos,
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::messaging::ObservedMemberState;

    fn member(instance: &str, generation: u64) -> ObservedMemberState {
        ObservedMemberState {
            source: ObservedSource {
                producer: ProducerRef::new("core_a", instance),
                source_link_id: "commander".to_string(),
                peer: None,
            },
            source_generation: generation,
            source_live: true,
        }
    }

    /// A member of `instance`'s slot pinned to its pair with `peer_instance`.
    fn pinned_member(instance: &str, peer_instance: &str) -> ObservedMemberState {
        let mut member = member(instance, 1);
        member.source.peer = Some(crate::messaging::PeerInfo {
            producer: ProducerRef::new("core_a", peer_instance),
            peer_link_id: "arm".to_string(),
        });
        member
    }

    fn state(members: Vec<ObservedMemberState>) -> ObservationState {
        ObservationState {
            sequence: 1,
            members,
        }
    }

    /// A `zero_or_one` slot is the one scalar cardinality with an empty state to
    /// report, so it is the one that reads through the `Option`-returning
    /// accessor: `None` for as long as the deployment leaves it vacant.
    #[tokio::test]
    async fn a_zero_or_one_slot_reports_an_absent_source_as_none() {
        let (tx, rx) = watch::channel(ObservationState::unregistered());
        let slot = ObservationSlot::new("observed_arm", config::node::Cardinality::ZeroOrOne, rx);
        assert_eq!(slot.source(), None, "a vacant slot observes nothing");

        tx.send(state(vec![member("arm_1", 1)])).unwrap();
        let src = slot.source().expect("slot should be resolved");
        assert_eq!(src.producer, ProducerRef::new("core_a", "arm_1"));
        assert_eq!(src.source_link_id, "commander");
    }

    /// A `one` slot always observes a pairing, so its accessor hands the member
    /// back directly and the caller has no empty branch to write.
    #[tokio::test]
    async fn a_one_slot_reads_its_member_without_an_option() {
        let (tx, rx) = watch::channel(ObservationState::unregistered());
        let slot = ObservationSlot::new("observed_arm", config::node::Cardinality::One, rx);
        tx.send(state(vec![member("arm_1", 1)])).unwrap();

        let src = slot.sole_source();
        assert_eq!(src.producer, ProducerRef::new("core_a", "arm_1"));
        assert_eq!(src.source_link_id, "commander");
    }

    /// A `one` slot that observes nothing is a manifest disagreement, not a case
    /// the application handles, and the panic names the declared spelling.
    #[tokio::test]
    #[should_panic(
        expected = "observer slot `observed_arm` is declared `one` but observes nothing"
    )]
    async fn sole_source_panics_when_a_one_slot_observes_nothing() {
        let (_tx, rx) = watch::channel(ObservationState::unregistered());
        let slot = ObservationSlot::new("observed_arm", config::node::Cardinality::One, rx);
        let _ = slot.sole_source();
    }

    #[tokio::test]
    async fn slot_set_reports_every_member_in_plan_order() {
        let (tx, rx) = watch::channel(ObservationState::unregistered());
        let set =
            ObservationSlotSet::new("observed_arms", config::node::Cardinality::ZeroOrMore, rx);
        assert!(
            set.sources().is_empty(),
            "a `zero_or_more` slot the plan left unobserved is empty, not absent"
        );

        tx.send(state(vec![member("arm_2", 1), member("arm_1", 1)]))
            .unwrap();
        assert_eq!(
            set.sources()
                .iter()
                .map(|s| s.producer.instance_id.clone())
                .collect::<Vec<_>>(),
            ["arm_2", "arm_1"]
        );

        // The set is live: a replan that drops a member shrinks it.
        tx.send(state(vec![member("arm_2", 1)])).unwrap();
        assert_eq!(set.sources().len(), 1);
    }

    /// A `one_or_more` slot hands its members back as a set whose head needs no
    /// unwrap, in the same plan order the permissive accessor reports.
    #[tokio::test]
    async fn non_empty_sources_carries_every_member_in_plan_order() {
        let (tx, rx) = watch::channel(ObservationState::unregistered());
        let set =
            ObservationSlotSet::new("observed_arms", config::node::Cardinality::OneOrMore, rx);
        tx.send(state(vec![member("arm_2", 1), member("arm_1", 1)]))
            .unwrap();

        let sources = set.non_empty_sources();
        assert_eq!(sources.len(), 2);
        assert_eq!(
            sources.first().producer,
            ProducerRef::new("core_a", "arm_2"),
            "first() is the plan's head, not the lowest instance_id"
        );
        assert_eq!(
            sources
                .iter()
                .map(|s| s.producer.instance_id.clone())
                .collect::<Vec<_>>(),
            ["arm_2", "arm_1"]
        );
    }

    /// A `one_or_more` slot that observes nothing is a manifest disagreement on
    /// the same terms as an empty `one` slot.
    #[tokio::test]
    #[should_panic(
        expected = "observer slot `observed_arms` is declared `one_or_more` but observes nothing"
    )]
    async fn non_empty_sources_panics_when_a_one_or_more_slot_observes_nothing() {
        let (_tx, rx) = watch::channel(ObservationState::unregistered());
        let set =
            ObservationSlotSet::new("observed_arms", config::node::Cardinality::OneOrMore, rx);
        let _ = set.non_empty_sources();
    }

    /// Reading a multi-member slot through the singular accessor is stale
    /// codegen, so it panics rather than silently reporting the first member,
    /// and the panic names the cardinality the manifest declared.
    #[tokio::test]
    #[should_panic(expected = "observer slot `observed_joints` is declared `zero_or_one`")]
    async fn a_scalar_accessor_on_a_multi_member_slot_panics() {
        let (tx, rx) = watch::channel(ObservationState::unregistered());
        let slot =
            ObservationSlot::new("observed_joints", config::node::Cardinality::ZeroOrOne, rx);
        tx.send(state(vec![member("arm_1", 1), member("arm_2", 1)]))
            .unwrap();
        let _ = slot.source();
    }

    /// The followed set is one pin per member, and a member's generation is
    /// part of its pin so an incarnation change redeclares only that member.
    #[test]
    fn followed_pins_are_one_per_member_and_generation_keyed() {
        let before = state(vec![member("arm_1", 1), member("arm_2", 1)]);
        let after = state(vec![member("arm_1", 1), member("arm_2", 2)]);

        let before_pins = ObservedFollow::desired(&before);
        let after_pins = ObservedFollow::desired(&after);
        assert_eq!(before_pins.len(), 2);
        assert_eq!(
            before_pins[0], after_pins[0],
            "an untouched member keeps its pin, so its subscription survives"
        );
        assert_ne!(
            before_pins[1], after_pins[1],
            "an incarnation change must redeclare that member"
        );
        assert!(ObservedFollow::desired(&ObservationState::unregistered()).is_empty());
    }

    /// A member pinned to a pair subscribes as that pair's peer; a member of
    /// the whole slot subscribes as any peer. Two members of one source slot
    /// pinned to different pairs are two pins.
    #[test]
    fn a_pinned_member_subscribes_as_its_pairs_peer() {
        let open = PairingRecipient::Any;
        let pins = ObservedFollow::desired(&state(vec![
            pinned_member("engine_1", "ctrl_1"),
            pinned_member("engine_1", "ctrl_2"),
            member("engine_1", 1),
        ]));
        assert_ne!(pins[0], pins[1]);
        assert_eq!(
            ObservedFollow::recipient(&pins[0], &open).unwrap(),
            PairingRecipient::Peer(WirePeer::new("core_a", "ctrl_1", "arm").unwrap())
        );
        assert_eq!(ObservedFollow::recipient(&pins[2], &open).unwrap(), open);
    }
}
