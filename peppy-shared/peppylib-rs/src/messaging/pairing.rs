//! Peer-set state for pairing slots. "Pairing" names the mechanism, contract,
//! and slot; "peer" names the other end of an established pair. A node's
//! runtime holds one [`tokio::sync::watch`] channel of [`PeerSetState`] per
//! declared pairing slot (see `runtime::Processor::pairing_slots`); the
//! daemon replaces it live over the `peer_update` service and the slot's
//! [`crate::runtime::PeerSubscription`], [`crate::runtime::PeerSlot`] and
//! [`crate::runtime::PeerSlotSet`] observe it.

use super::ProducerRef;

/// One peer paired on a slot: the peer instance's full `(core_node,
/// instance_id)` address plus the link_id of the peer's own complementary
/// slot. The triple pins the peer's publishes exactly (core, instance,
/// producer-side link_id segment), and names the peer a publish is for.
///
/// Returned by `NodeRunner::peer(link_id).paired()` / `wait_paired()` and
/// listed by `NodeRunner::peer_set(link_id).peers()`, surfaced by the
/// generated per-slot helpers, and tagged onto every message a peer
/// subscription yields. Hashable and ordered so consumers key maps on it.
///
/// Its derived `PartialEq` is the follow key: the slot's wire subscription is
/// redeclared when a pin changes, and a buffered message is dropped at
/// delivery once the slot has moved off the pin it was tagged with. The
/// peer's copy names the peer for presentation and lives beside it in
/// [`PeerMember`].
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PeerInfo {
    /// The peer instance's full wire address.
    pub producer: ProducerRef,
    /// The link_id of the peer's complementary pairing slot.
    pub peer_link_id: String,
}

/// `PeerInfo` is documented as a map key, so the bounds that needs are pinned
/// here rather than left to the derive list. `Ord` has no other user in the
/// crate and would otherwise be droppable without a failure.
const _: fn() = || {
    fn assert_map_key<T: std::hash::Hash + Eq + Ord>() {}
    assert_map_key::<PeerInfo>();
};

/// The far end of a pinned observation, as the boot config and the plan carry
/// it, as the runtime's peer identity.
impl From<&config::runtime::ObservedPeer> for PeerInfo {
    fn from(peer: &config::runtime::ObservedPeer) -> Self {
        Self {
            producer: peer.peer.clone(),
            peer_link_id: peer.peer_link_id.clone(),
        }
    }
}

/// The inverse, for a standalone seed written from runtime identities.
impl From<&PeerInfo> for config::runtime::ObservedPeer {
    fn from(peer: &PeerInfo) -> Self {
        Self {
            peer: peer.producer.clone(),
            peer_link_id: peer.peer_link_id.clone(),
        }
    }
}

/// One pair a slot holds, as the daemon delivers it: the peer's identity and
/// the copy the peer's instance belongs to (`None` for an instance run
/// outside a copy). A node holding pairs from several copies groups them by
/// `copy`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PeerMember {
    pub info: PeerInfo,
    pub copy: Option<String>,
}

/// The boot-config seed of one pair as the runtime's wire-state type.
impl From<&config::runtime::PairedPeer> for PeerMember {
    fn from(seed: &config::runtime::PairedPeer) -> Self {
        Self {
            info: PeerInfo {
                producer: ProducerRef::new(
                    seed.peer.core_node.clone(),
                    seed.peer.instance_id.clone(),
                ),
                peer_link_id: seed.peer_link_id.clone(),
            },
            copy: seed.copy.as_ref().map(|copy| copy.as_str().to_string()),
        }
    }
}

/// Absolute state of one pairing slot as delivered by the daemon: every pair
/// the slot holds, in establishment order. A scalar slot holds zero or one
/// member; a multi slot holds what its cardinality admits.
///
/// `sequence` orders `peer_update` deliveries so a delayed (stale) retry can
/// never roll the slot back; the listener rejects strictly-smaller sequences
/// and treats an equal sequence as an idempotent retry. Each delivery carries
/// the whole set and replaces it wholesale, so the members a delivery omits
/// are gone from the slot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerSetState {
    pub sequence: u64,
    pub members: Vec<PeerMember>,
}

impl PeerSetState {
    /// A slot's boot state at sequence zero, carrying the pairs the
    /// daemon stamped into the config's `pairing_slots` entry at spawn. Every
    /// daemon-delivered update carries a strictly larger sequence.
    pub fn seeded(members: Vec<PeerMember>) -> Self {
        Self {
            sequence: 0,
            members,
        }
    }

    /// The empty boot state: a slot with no pair yet.
    pub fn empty() -> Self {
        Self::seeded(Vec::new())
    }

    /// The identity of every peer the slot holds, in establishment order.
    pub fn peers(&self) -> impl Iterator<Item = &PeerInfo> {
        self.members.iter().map(|member| &member.info)
    }
}
