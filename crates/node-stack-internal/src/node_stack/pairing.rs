//! The daemon's source of truth for established pairs. Lives inside
//! `NodeStackInner` (same `RwLock` as the graph) so pair mutations and
//! stack-state scans validate atomically. In-memory only, like the stack
//! itself: a daemon restart loses pairing state.
//!
//! The registry records WHICH slots are paired; delivery of that state to
//! the endpoint nodes (over the `peer_update` service) is owned by the
//! `PairingCoordinator` in core-node-internal, which serializes all pairing
//! operations and calls into this registry to commit.

/// Address of one pairing slot: instance × link_id. A pair is strictly 1:1
/// between two complementary slots, exclusive until cleared.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SlotAddr {
    pub instance_id: String,
    pub link_id: String,
}

impl SlotAddr {
    pub fn new(instance_id: impl Into<String>, link_id: impl Into<String>) -> Self {
        Self {
            instance_id: instance_id.into(),
            link_id: link_id.into(),
        }
    }
}

impl std::fmt::Display for SlotAddr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}", self.instance_id, self.link_id)
    }
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

    pub fn involves_instance(&self, instance_id: &str) -> bool {
        self.a.slot.instance_id == instance_id || self.b.slot.instance_id == instance_id
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

    pub(crate) fn remove_for_instance(&mut self, instance_id: &str) -> Vec<Pairing> {
        let (dissolved, kept): (Vec<_>, Vec<_>) = std::mem::take(&mut self.pairs)
            .into_iter()
            .partition(|p| p.involves_instance(instance_id));
        self.pairs = kept;
        dissolved
    }

    /// Drops every pair for which `is_live` rejects either endpoint's
    /// instance. The lazy half of cleanup: the process-exit watcher has no
    /// stack back-reference, so registry reads prune dead pairs on the fly
    /// instead of trusting eager dissolution alone.
    pub(crate) fn prune_dead(&mut self, is_live: impl Fn(&str) -> bool) {
        self.pairs
            .retain(|p| is_live(&p.a.slot.instance_id) && is_live(&p.b.slot.instance_id));
    }

    pub(crate) fn pairs(&self) -> &[Pairing] {
        &self.pairs
    }

    pub(crate) fn clear(&mut self) {
        self.pairs.clear();
    }
}
