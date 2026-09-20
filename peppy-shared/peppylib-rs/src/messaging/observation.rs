//! Observation state for observer pairing slots. An observer passively taps a
//! producer's pairing topic without joining the 1:1 pairing. A node's runtime
//! holds one [`tokio::sync::watch`] channel of [`ObservationState`] per declared
//! observer slot (see `runtime::Processor`); the daemon mutates it live over the
//! `observation_update` service and the slot's
//! [`crate::runtime::ObservedTopicSubscription`] /
//! [`crate::runtime::ObservationSlot`] observe it.

use super::{PeerInfo, ProducerRef};

/// One observed source: the observed instance's full `(core_node,
/// instance_id)` address plus the producer-side link_id of the pairing slot
/// being observed, and, when the plan named one pair of that slot by its other
/// end, that peer. The triple pins the source's publishes exactly (core,
/// instance, producer-side link_id segment), the peer narrows them to the one
/// pair, and together they are both what an observer subscription is declared
/// against and the member's full identity, so members sharing one instance,
/// or one slot, stay distinct.
///
/// Returned by `NodeRunner::observation_slot(link_id)`'s `source()` and
/// `observation_slot_set(link_id)`'s `sources()`, surfaced by the generated
/// per-slot `source()` / `sources()` helpers, and tagged onto every message an
/// observed-topic subscription yields. Hashable and ordered so consumers key
/// demux maps on it. Purely local configuration state known to the observer
/// from its own registration; it needs no daemon push to read.
///
/// Its derived `PartialEq` is the follow key: a member's wire subscription is
/// redeclared when its source changes, and a buffered message is dropped at
/// delivery once its source leaves the followed set. A field added here for
/// presentation alone would move that predicate, so wrap it the way
/// [`ObservedMemberState`] wraps this type to carry the generation.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ObservedSource {
    /// The observed source instance's full wire address.
    pub producer: ProducerRef,
    /// The producer-side link_id of the observed pairing slot.
    pub source_link_id: String,
    /// The pair's other end, when the plan named the pair by it: the peer the
    /// source publishes to and the link_id of the peer's slot. `None`
    /// observes every pair of the source's slot, each tagged with this same
    /// source.
    pub peer: Option<PeerInfo>,
}

/// The documented demux idiom keys a map on an `ObservedSource`, so the bounds
/// that needs are pinned here rather than left to the derive list. `Ord` has no
/// other user in the crate and would otherwise be droppable without a failure.
const _: fn() = || {
    fn assert_map_key<T: std::hash::Hash + Eq + Ord>() {}
    assert_map_key::<ObservedSource>();
};

/// The observed member set of a `cardinality: "one_or_more"` observer slot: an
/// ordered snapshot of the slot's members in plan order that is never empty by
/// construction. Generated `sources()` accessors of `one_or_more` slots return
/// this instead of a plain `Vec` so the plan-validated "at least one" guarantee
/// lives in the type rather than in a comment: [`first`](Self::first) is
/// infallible and there is no empty branch to write. The sibling cardinalities
/// keep their own shapes (`one` returns the sole [`ObservedSource`] directly,
/// `zero_or_one` an `Option<ObservedSource>`, `zero_or_more` a plain, possibly
/// empty `Vec<ObservedSource>`), so flipping a slot's cardinality changes the
/// accessor's type and surfaces every affected call site at compile time.
///
/// It owns its members where the producer-binding counterpart
/// [`NonEmptyProducers`](super::NonEmptyProducers) borrows: a bound producer set
/// is a stable startup cache, while an observer slot keeps its state behind a
/// [`tokio::sync::watch`] channel, so every read materializes the set from a
/// borrow guard that ends with the call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NonEmptyObservedSources {
    sources: Vec<ObservedSource>,
}

// `is_empty` is deliberately absent: the constructor rejects an empty vector,
// so it would be a constant `false`.
#[allow(clippy::len_without_is_empty)]
impl NonEmptyObservedSources {
    /// Wraps `sources` as a non-empty set, or `None` when the vector is empty.
    /// Runtime callers go through [`ObservationSlotSet::non_empty_sources`],
    /// which reads a slot the launcher sized at plan time and node startup
    /// re-checked against its seed; this checked constructor exists so the
    /// invariant cannot be sidestepped elsewhere.
    ///
    /// [`ObservationSlotSet::non_empty_sources`]: crate::runtime::ObservationSlotSet::non_empty_sources
    pub fn new(sources: Vec<ObservedSource>) -> Option<Self> {
        if sources.is_empty() {
            return None;
        }
        Some(Self { sources })
    }

    /// The first member in plan order. Infallible: the set is never empty, so
    /// unlike `slice::first` there is no `Option` to unwrap.
    pub fn first(&self) -> &ObservedSource {
        &self.sources[0]
    }

    /// Iterates the members in plan order.
    pub fn iter(&self) -> std::slice::Iter<'_, ObservedSource> {
        self.sources.iter()
    }

    /// Number of members, always at least 1.
    pub fn len(&self) -> usize {
        self.sources.len()
    }

    /// The members as a plain slice, for slice-shaped APIs and order assertions
    /// in tests.
    pub fn as_slice(&self) -> &[ObservedSource] {
        &self.sources
    }
}

impl IntoIterator for NonEmptyObservedSources {
    type Item = ObservedSource;
    type IntoIter = std::vec::IntoIter<ObservedSource>;

    fn into_iter(self) -> Self::IntoIter {
        self.sources.into_iter()
    }
}

impl<'a> IntoIterator for &'a NonEmptyObservedSources {
    type Item = &'a ObservedSource;
    type IntoIter = std::slice::Iter<'a, ObservedSource>;

    fn into_iter(self) -> Self::IntoIter {
        self.sources.iter()
    }
}

/// One member of an observer slot's set: the pairing this member taps, plus
/// the two per-member facts the daemon keeps current.
///
/// `source_generation` is the daemon-assigned incarnation counter. It advances
/// only when this member's source changes incarnation (never on the source's own
/// peer transitions), and is the sole discriminator between old-B and new-B
/// messages, which are byte-identical on the wire. A change drops and redeclares
/// that member's wire subscription (buffer isolation) and invalidates any
/// in-flight tagged message from the previous generation.
///
/// `source_live` reports whether the member's source instance is currently in a
/// non-terminal state. It is informational (the observer keeps the subscription
/// declared whether or not the source is live), delivered so the state is
/// complete. A member whose source is down stays listed, at its position.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObservedMemberState {
    pub source: ObservedSource,
    pub source_generation: u64,
    pub source_live: bool,
}

/// Absolute observation state for one observer slot as delivered by the daemon:
/// the slot's complete ordered member set, in the order the plan listed it.
///
/// `sequence` orders `observation_update` deliveries so a delayed (stale) retry
/// can never roll the slot back; the listener rejects strictly-smaller sequences
/// and treats an equal sequence as an idempotent retry. Each delivery carries
/// the whole set and replaces it wholesale, so the members a delivery omits are
/// gone from the slot.
///
/// `members` carries the plan's membership from the node's first instruction:
/// the slot boots seeded at sequence zero, so the set is empty only where the
/// plan could write an empty one (`zero_or_one` vacant, `zero_or_more`
/// observing nothing). A member's position never moves once delivered: a
/// generation bump changes that member in place.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObservationState {
    pub sequence: u64,
    pub members: Vec<ObservedMemberState>,
}

impl ObservationState {
    /// A slot's boot state at sequence zero, carrying the membership the
    /// daemon stamped into the config's `observation_seeds` entry at spawn.
    /// The one construction path for a pre-delivery state, so a slot cannot
    /// boot through a spelling that skips the seed. Live
    /// `observation_update` deliveries replace it at strictly larger
    /// sequences.
    pub fn seeded(members: Vec<ObservedMemberState>) -> Self {
        Self {
            sequence: 0,
            members,
        }
    }

    /// The empty boot state: a missing seed counts as empty, which constructs
    /// only where the plan could have written an empty set (`zero_or_one`
    /// vacant, `zero_or_more` observing nothing), so this is the boot state of
    /// those slots and of embedders that manage observation state themselves.
    pub fn unregistered() -> Self {
        Self::seeded(Vec::new())
    }
}

/// Narrows a member to the identity alone, dropping the daemon-kept
/// `source_generation` and `source_live`, which are the observer runtime's
/// business and not the consumer's.
impl From<&ObservedMemberState> for ObservedSource {
    fn from(member: &ObservedMemberState) -> Self {
        member.source.clone()
    }
}

/// One boot-config seed member as the runtime's wire-state type. Field for
/// field: the seed is the daemon's [`ObservedMemberState`] stamping carried by
/// the config instead of the wire, so the first live delivery normally repeats
/// it exactly and no subscription redeclares. Sits beside the narrowing
/// conversion above so both seed/wire translations live in one place.
impl From<&config::runtime::ObservationSeedMember> for ObservedMemberState {
    fn from(seed: &config::runtime::ObservationSeedMember) -> Self {
        Self {
            source: ObservedSource {
                producer: seed.source.clone(),
                source_link_id: seed.source_link_id.clone(),
                peer: seed.peer.as_ref().map(PeerInfo::from),
            },
            source_generation: seed.source_generation,
            source_live: seed.source_live,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sources() -> Vec<ObservedSource> {
        vec![
            ObservedSource {
                producer: ProducerRef::new("core-1234", "left_arm"),
                source_link_id: "joint_states".to_string(),
                peer: None,
            },
            ObservedSource {
                producer: ProducerRef::new("core-1234", "right_arm"),
                source_link_id: "joint_states".to_string(),
                peer: None,
            },
        ]
    }

    /// Two observations of one source slot pinned to different pairs are two
    /// members, and neither is the observation of the whole slot: the peer is
    /// part of the identity a demux map keys on.
    #[test]
    fn the_peer_is_part_of_a_members_identity() {
        let whole_slot = sources().remove(0);
        let pinned_to = |instance: &str| ObservedSource {
            peer: Some(PeerInfo {
                producer: ProducerRef::new("core-1234", instance),
                peer_link_id: "left_arm_link".to_string(),
            }),
            ..whole_slot.clone()
        };
        assert_ne!(pinned_to("alpha_backbone"), pinned_to("bravo_backbone"));
        assert_ne!(pinned_to("alpha_backbone"), whole_slot);
    }

    #[test]
    fn an_empty_vec_is_rejected_at_construction() {
        assert_eq!(NonEmptyObservedSources::new(Vec::new()), None);
    }

    #[test]
    fn first_iter_len_and_as_slice_preserve_plan_order() {
        let sources = sources();
        let set = NonEmptyObservedSources::new(sources.clone()).expect("two members are non-empty");

        assert_eq!(set.first(), &sources[0], "first() is the plan's head");
        assert_eq!(set.len(), 2);
        assert_eq!(set.as_slice(), &sources[..]);
        assert_eq!(
            set.iter().collect::<Vec<_>>(),
            sources.iter().collect::<Vec<_>>(),
            "iteration follows plan order"
        );
    }

    #[test]
    fn for_loops_work_by_reference_and_by_value() {
        let set = NonEmptyObservedSources::new(sources()).expect("two members are non-empty");

        let mut seen = Vec::new();
        for member in &set {
            seen.push(member.producer.instance_id.clone());
        }
        // The set owns its members rather than borrowing them, so unlike
        // `NonEmptyProducers` it is not `Copy` and the by-value loop consumes
        // it. It therefore has to come last.
        for member in set {
            seen.push(member.producer.instance_id);
        }
        assert_eq!(seen, ["left_arm", "right_arm", "left_arm", "right_arm"]);
    }
}
