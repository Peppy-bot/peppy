//! The daemon's incarnation ledger: one monotonic counter per source
//! instance, allocated at spawn by the daemon that owns the instance and
//! mirrored verbatim by daemons that only relate to it.
//!
//! The number is load-bearing on the wire: every topic publish a node emits
//! carries its own incarnation as the trailing keyexpr segment, and pairing /
//! observer subscriptions pin that segment, so a subscription addresses
//! exactly one run of a source. That only works if everyone agrees on the
//! number, which is why the ledger distinguishes its two write paths:
//!
//! * [`IncarnationLedger::allocate_local`] is the ONE place a new number is
//!   minted, on the spawn path of the daemon that owns the instance, strictly
//!   before the boot config is serialized. The spawned process publishes
//!   under this number for its whole life.
//! * [`IncarnationLedger::record_reported`] mirrors a number another daemon
//!   allocated and reported (a relationship notification, a pair commit, or
//!   an incarnation query answer). It never increments: a duplicate or
//!   reordered report converges on the highest value seen, which is the
//!   newest incarnation, because allocation is monotonic on the owner.
//!
//! Entries are retained for the daemon's lifetime, across `stack reset`
//! included: reusing a number for a fresh run of the same instance id would
//! let a subscription pinned to the old run match the new one, which is the
//! exact confusion the wire segment exists to prevent. The ledger holds one
//! `u64` per distinct source address ever seen, so retention is not a
//! growth concern.

use peppylib::messaging::ProducerRef;
use std::collections::BTreeMap;
use std::num::NonZeroU64;
use std::sync::Mutex;

/// A source instance's full address. Two daemons can host same-named
/// instances, so the pair is the identity: keying the ledger on the instance
/// id alone would let a local `wrist_cam_inst` and a remote one share an
/// incarnation counter.
///
/// Deliberately coarser than an observed pin, which also carries the
/// producer-side link_id: an incarnation belongs to the source instance, so
/// an instance observed through two of its own pairing slots has one number,
/// not two.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct SourceKey {
    pub(crate) core_node: String,
    pub(crate) instance_id: String,
}

impl SourceKey {
    pub(crate) fn new(core_node: impl Into<String>, instance_id: impl Into<String>) -> Self {
        Self {
            core_node: core_node.into(),
            instance_id: instance_id.into(),
        }
    }

    pub(crate) fn from_producer(producer: &ProducerRef) -> Self {
        Self::new(&producer.core_node, &producer.instance_id)
    }
}

#[derive(Default)]
pub(crate) struct IncarnationLedger {
    per_source: Mutex<BTreeMap<SourceKey, u64>>,
}

impl IncarnationLedger {
    /// Mints the next incarnation for a LOCAL instance about to spawn. The
    /// sole allocation site; the returned number goes into the boot config
    /// the process publishes under. A spawn that later fails leaves a gap in
    /// the sequence, which is harmless: uniqueness per run is the contract,
    /// density is not.
    pub(crate) fn allocate_local(&self, core_node: &str, instance_id: &str) -> NonZeroU64 {
        let mut per_source = self.per_source.lock().unwrap();
        let counter = per_source
            .entry(SourceKey::new(core_node, instance_id))
            .or_insert(0);
        *counter += 1;
        NonZeroU64::new(*counter).expect("a just-incremented counter is positive")
    }

    /// Mirrors an incarnation another daemon allocated and reported.
    /// Converges on the maximum so a delayed or duplicated report can never
    /// roll the mirror back behind a newer one.
    pub(crate) fn record_reported(&self, source: &SourceKey, incarnation: u64) {
        let mut per_source = self.per_source.lock().unwrap();
        let current = per_source.entry(source.clone()).or_insert(0);
        *current = (*current).max(incarnation);
    }

    /// The current incarnation of `source` as this daemon knows it. Zero
    /// means "never seen run": authoritative for a local source (this daemon
    /// allocates every local number), best-known for a remote one.
    pub(crate) fn current(&self, source: &SourceKey) -> u64 {
        self.per_source
            .lock()
            .unwrap()
            .get(source)
            .copied()
            .unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(instance_id: &str) -> SourceKey {
        SourceKey::new("core_a", instance_id)
    }

    #[test]
    fn allocation_is_monotonic_per_source() {
        let ledger = IncarnationLedger::default();
        assert_eq!(ledger.allocate_local("core_a", "arm_1").get(), 1);
        assert_eq!(ledger.allocate_local("core_a", "arm_1").get(), 2);
        assert_eq!(
            ledger.allocate_local("core_a", "arm_2").get(),
            1,
            "each source counts alone"
        );
        assert_eq!(ledger.current(&key("arm_1")), 2);
    }

    #[test]
    fn same_instance_id_on_two_daemons_counts_separately() {
        let ledger = IncarnationLedger::default();
        ledger.allocate_local("core_a", "arm_1");
        ledger.allocate_local("core_a", "arm_1");
        assert_eq!(ledger.current(&SourceKey::new("core_b", "arm_1")), 0);
    }

    #[test]
    fn reported_incarnations_converge_on_the_maximum() {
        let ledger = IncarnationLedger::default();
        let source = key("remote_arm");
        ledger.record_reported(&source, 4);
        assert_eq!(ledger.current(&source), 4);
        // A delayed older report never rolls back.
        ledger.record_reported(&source, 2);
        assert_eq!(ledger.current(&source), 4);
        // A duplicate is idempotent, not an increment.
        ledger.record_reported(&source, 4);
        assert_eq!(ledger.current(&source), 4);
        ledger.record_reported(&source, 5);
        assert_eq!(ledger.current(&source), 5);
    }

    #[test]
    fn a_never_seen_source_reads_zero() {
        let ledger = IncarnationLedger::default();
        assert_eq!(ledger.current(&key("ghost")), 0);
    }
}
