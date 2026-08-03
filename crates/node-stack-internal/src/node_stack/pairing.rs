//! The daemon's source of truth for established pairs. Lives inside
//! `NodeStackInner` (same `RwLock` as the graph) so pair mutations and
//! stack-state scans validate atomically. In-memory only, like the stack
//! itself: a daemon restart loses pairing state.
//!
//! The registry records WHICH slots are paired; delivery of that state to
//! the endpoint nodes (over the `peer_update` service) is owned by the
//! `PairingCoordinator` in core-node-internal, which serializes all pairing
//! operations and calls into this registry to commit.

/// Address of one pairing slot: core node × instance × link_id. A pair is
/// strictly 1:1 between two complementary slots, exclusive until cleared.
///
/// The core node is part of the identity because two daemons can host
/// same-named instances: without it, a local `reflex_inst` and a remote one
/// would collide in this registry and each could silently claim the other's
/// slot.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SlotAddr {
    pub core_node: String,
    pub instance_id: String,
    pub link_id: String,
}

impl SlotAddr {
    pub fn new(
        core_node: impl Into<String>,
        instance_id: impl Into<String>,
        link_id: impl Into<String>,
    ) -> Self {
        Self {
            core_node: core_node.into(),
            instance_id: instance_id.into(),
            link_id: link_id.into(),
        }
    }

    /// Whether this slot lives on `core_node`, i.e. whether the daemon holding
    /// that name owns it and can read its manifest.
    pub fn is_on(&self, core_node: &str) -> bool {
        self.core_node == core_node
    }
}

impl std::fmt::Display for SlotAddr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}/{}:{}",
            self.core_node, self.instance_id, self.link_id
        )
    }
}

/// The already-validated metadata of a pairing slot that lives on ANOTHER
/// daemon.
///
/// A daemon can only read manifests for instances it hosts, so it cannot
/// derive these for a remote endpoint. It does not need to: the coordinator of
/// a federated launch holds every participant's manifests and validates the
/// whole plan (same pairing, complementary roles, matching sha pins) before
/// anything starts. This carries that verdict to the daemon that commits the
/// local half.
///
/// The coordinator is the serialization point precisely so daemons never have
/// to negotiate this among themselves.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteSlotMeta {
    pub pairing_name: String,
    pub pairing_tag: String,
    pub role: String,
}

/// One endpoint of an established pair: the slot plus the role its manifest
/// declares for it (recorded at pair time so readers don't re-derive it).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PairEndpoint {
    pub slot: SlotAddr,
    pub role: String,
}

/// One established pair.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pairing {
    pub pairing_name: String,
    pub pairing_tag: String,
    pub a: PairEndpoint,
    pub b: PairEndpoint,
}

impl Pairing {
    /// The endpoint at `slot`, if this pair contains it.
    pub fn endpoint(&self, slot: &SlotAddr) -> Option<&PairEndpoint> {
        if &self.a.slot == slot {
            Some(&self.a)
        } else if &self.b.slot == slot {
            Some(&self.b)
        } else {
            None
        }
    }

    /// The OTHER endpoint relative to `slot`, if this pair contains `slot`.
    pub fn peer_of(&self, slot: &SlotAddr) -> Option<&PairEndpoint> {
        if &self.a.slot == slot {
            Some(&self.b)
        } else if &self.b.slot == slot {
            Some(&self.a)
        } else {
            None
        }
    }

    /// Whether either endpoint is the given instance ON the given daemon.
    /// Both halves of the address matter: two daemons can host same-named
    /// instances, and a remote one dying must not dissolve the local one's
    /// pairs.
    pub fn involves(&self, core_node: &str, instance_id: &str) -> bool {
        [&self.a, &self.b].iter().any(|endpoint| {
            endpoint.slot.core_node == core_node && endpoint.slot.instance_id == instance_id
        })
    }
}

/// The pair store. Plain `Vec` — stacks hold at most a handful of pairs, and
/// every access already happens under the stack's `RwLock`.
#[derive(Debug, Default)]
pub(crate) struct PairingRegistry {
    pairs: Vec<Pairing>,
}

impl PairingRegistry {
    pub(crate) fn find_by_slot(&self, slot: &SlotAddr) -> Option<&Pairing> {
        self.pairs.iter().find(|p| p.endpoint(slot).is_some())
    }

    pub(crate) fn insert(&mut self, pairing: Pairing) {
        self.pairs.push(pairing);
    }

    pub(crate) fn remove_by_slot(&mut self, slot: &SlotAddr) -> Option<Pairing> {
        let idx = self.pairs.iter().position(|p| p.endpoint(slot).is_some())?;
        Some(self.pairs.remove(idx))
    }

    pub(crate) fn remove_for_instance(
        &mut self,
        core_node: &str,
        instance_id: &str,
    ) -> Vec<Pairing> {
        let (dissolved, kept): (Vec<_>, Vec<_>) = std::mem::take(&mut self.pairs)
            .into_iter()
            .partition(|p| p.involves(core_node, instance_id));
        self.pairs = kept;
        dissolved
    }

    /// Drops every pair for which `is_live` rejects a LOCAL endpoint's
    /// instance. The lazy half of cleanup: the process-exit watcher has no
    /// stack back-reference, so registry reads prune dead pairs on the fly
    /// instead of trusting eager dissolution alone.
    ///
    /// A remote endpoint is deliberately never pruned here. This daemon cannot
    /// see whether an instance on another machine is alive, and treating
    /// "cannot see" as "dead" would silently dissolve every cross-daemon pair
    /// on the next registry read. Remote death arrives as an explicit
    /// notification from the daemon that owns it, which stays authoritative.
    pub(crate) fn prune_dead(&mut self, local_core_node: &str, is_live: impl Fn(&str) -> bool) {
        self.pairs.retain(|p| {
            [&p.a, &p.b].into_iter().all(|endpoint| {
                !endpoint.slot.is_on(local_core_node) || is_live(&endpoint.slot.instance_id)
            })
        });
    }

    pub(crate) fn pairs(&self) -> &[Pairing] {
        &self.pairs
    }

    pub(crate) fn clear(&mut self) {
        self.pairs.clear();
    }
}
