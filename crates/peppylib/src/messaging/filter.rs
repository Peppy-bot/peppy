//! Consumer-side per-slot filter applied by the messaging layer to decide
//! which producer messages reach which `depends_on` slot. The validator
//! (in `config-internal::launcher::bindings`) pre-resolves each consumer
//! instance's launcher / CLI binding map into per-slot
//! [`config::runtime::SlotBinding`] entries — each stamped with the
//! producer's full `(core_node, instance_id)` wire address; at startup,
//! the runtime [`crate::runtime::Processor`] reads each declared
//! `link_id` and synthesizes a [`ConsumerFilter`] for the subscribe /
//! poll / send_goal call.
//!
//! Every producer reference below the validator is a [`ProducerRef`]: the
//! wire addresses producers by the pair (instance_id alone is only unique
//! within one stack), so a half-address is unrepresentable here by
//! construction.
//!
//! The four variants map directly to the spec's invariants:
//! - [`ConsumerFilter::Pin`] — wire-layer pin of both `from_core_node`
//!   and `from_instance_id` to a single producer. Used for pinned slots
//!   and from_any slots bound to exactly one producer.
//! - [`ConsumerFilter::OnlyFrom`] — wire wildcards; an in-process
//!   acceptance set filters incoming messages by source
//!   `(core_node, instance_id)`. Used for from_any slots bound to
//!   multiple producers.
//! - [`ConsumerFilter::AnyExcept`] — wire wildcards; a reject set drops
//!   messages from producers claimed by sibling slots. Used for
//!   from_any slots with no bindings on consumers that *do* have
//!   sibling bindings claiming some producers for this `(name, tag)`.
//! - [`ConsumerFilter::Any`] — pure wildcard. Used for from_any slots
//!   with no bindings on consumers with no sibling claims for this
//!   `(name, tag)`.

use config::node::DependsOn;
use config::runtime::SlotBinding;
use std::collections::{BTreeMap, BTreeSet};

pub use config::runtime::ProducerRef;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConsumerFilter {
    /// Wire-layer pin: subscribe / poll / send_goal address exactly this
    /// producer's `(core_node, instance_id)`. No in-process filtering and
    /// no discovery required.
    Pin(ProducerRef),
    /// Wire wildcards; accept only messages whose source
    /// `(core_node, instance_id)` is in the set. The empty set is legal
    /// (and means "this slot receives nothing" — e.g. every bound
    /// producer was preempted by a pinned sibling).
    OnlyFrom(Vec<ProducerRef>),
    /// Wire wildcards; drop messages whose source
    /// `(core_node, instance_id)` is in the set. The empty set
    /// degenerates to [`ConsumerFilter::Any`].
    AnyExcept(Vec<ProducerRef>),
    /// Pure wildcard at the wire layer.
    Any,
}

impl ConsumerFilter {
    /// Service / action call sites use a single fully-pinned target per
    /// call. Returns `Some(producer)` when the filter targets exactly one
    /// producer ([`ConsumerFilter::Pin`]) — the call site then addresses
    /// it directly and skips discovery entirely; otherwise `None`, in
    /// which case the call site falls back to wildcard discovery
    /// (discover-then-pin).
    pub fn pinned_target(&self) -> Option<&ProducerRef> {
        match self {
            ConsumerFilter::Pin(producer) => Some(producer),
            _ => None,
        }
    }
}

/// Compute the [`ConsumerFilter`] for `link_id` from the daemon-supplied
/// per-slot bindings and the consumer's manifest `depends_on`.
///
/// The algorithm applies the spec's invariants in one pass:
/// 1. A pinned slot is `Pin(producer)`.
/// 2. A `FromAnyBound` slot's effective producer set excludes those
///    already claimed by a pinned sibling on the same `(name, tag)` —
///    that's the "pinned-bound preempts from_any" rule.
/// 3. A `FromAnyUnbound` slot drops every producer claimed by *any*
///    sibling binding (pinned or from_any-explicit) on the same `(name,
///    tag)` — that's the "explicit bindings replace the wildcard
///    fallback" rule.
///
/// Claims are keyed on the full `(core_node, instance_id)` pair: two
/// producers sharing an instance_id on different core_nodes are distinct
/// producers and never preempt each other.
///
/// Slots not present in `slot_bindings` (e.g. consumers with no
/// `depends_on` at all) resolve to [`ConsumerFilter::Any`]. This is a
/// defensive fallback — the validator should have populated every
/// declared slot.
pub fn resolve_consumer_filter(
    link_id: &str,
    slot_bindings: &BTreeMap<String, SlotBinding>,
    depends_on: Option<&DependsOn>,
) -> ConsumerFilter {
    let Some(slot) = slot_bindings.get(link_id) else {
        return ConsumerFilter::Any;
    };

    // Map every slot's link_id to its (name, tag) and kind, so we can
    // collect sibling claims on the same (name, tag).
    let slot_name_tag = lookup_slot_name_tag(link_id, depends_on);

    match slot {
        SlotBinding::Pinned { producer } => ConsumerFilter::Pin(producer.clone()),
        SlotBinding::FromAnyBound { producers } => {
            let pinned_claimed =
                pinned_claims_for_name_tag(slot_name_tag, slot_bindings, depends_on);
            let effective: Vec<ProducerRef> = producers
                .iter()
                .filter(|producer| !pinned_claimed.contains(producer))
                .cloned()
                .collect();
            // Degenerate `OnlyFrom([single])` → Pin: the slot resolves to
            // exactly one wire-complete producer, so the call sites can
            // pin both wire slots and skip discovery, instead of paying
            // the wildcard + in-process filter cost.
            if effective.len() == 1 {
                ConsumerFilter::Pin(effective.into_iter().next().unwrap())
            } else {
                ConsumerFilter::OnlyFrom(effective)
            }
        }
        SlotBinding::FromAnyUnbound => {
            let claimed = all_sibling_claims_for_name_tag(slot_name_tag, slot_bindings, depends_on);
            if claimed.is_empty() {
                ConsumerFilter::Any
            } else {
                ConsumerFilter::AnyExcept(claimed.into_iter().collect())
            }
        }
    }
}

/// Normalize each `DependsOn` entry to a `(name, tag, link_id,
/// from_any)` tuple so node and interface dep lists can be walked
/// uniformly.
fn iter_deps(depends_on: Option<&DependsOn>) -> Vec<(&str, &str, &str, bool)> {
    let Some(deps) = depends_on else {
        return Vec::new();
    };
    let mut out: Vec<(&str, &str, &str, bool)> =
        Vec::with_capacity(deps.nodes.len() + deps.interfaces.len());
    for dep in &deps.nodes {
        out.push((
            dep.name.as_str(),
            dep.tag.as_str(),
            dep.link_id.as_str(),
            dep.from_any,
        ));
    }
    for dep in &deps.interfaces {
        out.push((
            dep.name.as_str(),
            dep.tag.as_str(),
            dep.link_id.as_str(),
            dep.from_any,
        ));
    }
    out
}

/// `(name, tag)` of the `depends_on` entry declaring `link_id`, or
/// `None` if no such entry exists (defensive — validator should have
/// caught this).
fn lookup_slot_name_tag<'a>(
    link_id: &str,
    depends_on: Option<&'a DependsOn>,
) -> Option<(&'a str, &'a str)> {
    iter_deps(depends_on)
        .into_iter()
        .find(|(_, _, lid, _)| *lid == link_id)
        .map(|(name, tag, _, _)| (name, tag))
}

/// All producers claimed by pinned sibling slots on the same
/// `(name, tag)`, keyed on the full `(core_node, instance_id)` pair.
fn pinned_claims_for_name_tag<'a>(
    name_tag: Option<(&str, &str)>,
    slot_bindings: &'a BTreeMap<String, SlotBinding>,
    depends_on: Option<&DependsOn>,
) -> BTreeSet<&'a ProducerRef> {
    let mut out = BTreeSet::new();
    let Some((name, tag)) = name_tag else {
        return out;
    };
    for (dep_name, dep_tag, dep_link_id, from_any) in iter_deps(depends_on) {
        if from_any || dep_name != name || dep_tag != tag {
            continue;
        }
        if let Some(SlotBinding::Pinned { producer }) = slot_bindings.get(dep_link_id) {
            out.insert(producer);
        }
    }
    out
}

/// Every producer named by any sibling binding (pinned or from_any
/// explicit) on the same `(name, tag)`, keyed on the full pair. Used to
/// populate the reject set for an unbound `from_any` slot.
fn all_sibling_claims_for_name_tag(
    name_tag: Option<(&str, &str)>,
    slot_bindings: &BTreeMap<String, SlotBinding>,
    depends_on: Option<&DependsOn>,
) -> BTreeSet<ProducerRef> {
    let mut out = BTreeSet::new();
    let Some((name, tag)) = name_tag else {
        return out;
    };
    for (dep_name, dep_tag, dep_link_id, _from_any) in iter_deps(depends_on) {
        if dep_name != name || dep_tag != tag {
            continue;
        }
        match slot_bindings.get(dep_link_id) {
            Some(SlotBinding::Pinned { producer }) => {
                out.insert(producer.clone());
            }
            Some(SlotBinding::FromAnyBound { producers }) => {
                for producer in producers {
                    out.insert(producer.clone());
                }
            }
            Some(SlotBinding::FromAnyUnbound) | None => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use config::node::{Name, NodeDependency};

    /// Core_node used by these fixtures: bindings are stack-scoped, so
    /// every producer in one consumer's binding map shares the launching
    /// daemon's core_node.
    const CORE: &str = "core_a";

    fn pref(instance_id: &str) -> ProducerRef {
        ProducerRef::new(CORE, instance_id)
    }

    fn deps(entries: Vec<(&str, &str, &str, bool)>) -> DependsOn {
        DependsOn {
            nodes: entries
                .into_iter()
                .map(|(name, tag, link_id, from_any)| NodeDependency {
                    name: Name::new(name).unwrap(),
                    tag: tag.to_string(),
                    link_id: link_id.to_string(),
                    from_any,
                })
                .collect(),
            interfaces: vec![],
        }
    }

    fn slot_map(entries: Vec<(&str, SlotBinding)>) -> BTreeMap<String, SlotBinding> {
        entries
            .into_iter()
            .map(|(k, v)| (k.to_string(), v))
            .collect()
    }

    #[test]
    fn pinned_slot_resolves_to_pin() {
        let depends_on = deps(vec![("camera", "v1", "main", false)]);
        let bindings = slot_map(vec![(
            "main",
            SlotBinding::Pinned {
                producer: pref("cam1"),
            },
        )]);
        let filter = resolve_consumer_filter("main", &bindings, Some(&depends_on));
        assert_eq!(filter, ConsumerFilter::Pin(pref("cam1")));
        assert_eq!(filter.pinned_target(), Some(&pref("cam1")));
    }

    /// The single-producer collapse yields a full `ProducerRef`, which is
    /// what lets call sites skip discovery for from_any-bound-to-one
    /// slots exactly like explicitly pinned ones.
    #[test]
    fn from_any_bound_to_single_producer_collapses_to_pin() {
        let depends_on = deps(vec![("camera", "v1", "extra", true)]);
        let bindings = slot_map(vec![(
            "extra",
            SlotBinding::FromAnyBound {
                producers: vec![pref("cam1")],
            },
        )]);
        let filter = resolve_consumer_filter("extra", &bindings, Some(&depends_on));
        assert_eq!(filter, ConsumerFilter::Pin(pref("cam1")));
        assert_eq!(filter.pinned_target(), Some(&pref("cam1")));
    }

    #[test]
    fn from_any_bound_to_multiple_producers_resolves_to_only_from() {
        let depends_on = deps(vec![("camera", "v1", "extra", true)]);
        let bindings = slot_map(vec![(
            "extra",
            SlotBinding::FromAnyBound {
                producers: vec![pref("cam1"), pref("cam2")],
            },
        )]);
        let filter = resolve_consumer_filter("extra", &bindings, Some(&depends_on));
        assert_eq!(
            filter,
            ConsumerFilter::OnlyFrom(vec![pref("cam1"), pref("cam2")])
        );
        assert_eq!(filter.pinned_target(), None);
    }

    /// Statement 1 + precedence: pinned slot bound to a producer also
    /// named by a from_any sibling — the from_any slot's effective set
    /// excludes the pinned-claimed producer.
    #[test]
    fn from_any_bound_excludes_pinned_claimed_siblings() {
        let depends_on = deps(vec![
            ("camera", "v1", "wrist_left", false),
            ("camera", "v1", "extra", true),
        ]);
        let bindings = slot_map(vec![
            (
                "wrist_left",
                SlotBinding::Pinned {
                    producer: pref("cam1"),
                },
            ),
            (
                "extra",
                SlotBinding::FromAnyBound {
                    producers: vec![pref("cam1")],
                },
            ),
        ]);
        let filter = resolve_consumer_filter("extra", &bindings, Some(&depends_on));
        assert_eq!(filter, ConsumerFilter::OnlyFrom(vec![]));
    }

    /// Claims are keyed on the full pair: a pinned sibling claiming
    /// `(core_a, cam1)` does NOT preempt a producer sharing the
    /// instance_id on a different core_node — they are distinct
    /// producers on the wire.
    #[test]
    fn sibling_claims_distinguish_same_instance_id_on_different_core_nodes() {
        let depends_on = deps(vec![
            ("camera", "v1", "wrist_left", false),
            ("camera", "v1", "extra", true),
        ]);
        let other_core_cam1 = ProducerRef::new("core_b", "cam1");
        let bindings = slot_map(vec![
            (
                "wrist_left",
                SlotBinding::Pinned {
                    producer: pref("cam1"),
                },
            ),
            (
                "extra",
                SlotBinding::FromAnyBound {
                    producers: vec![other_core_cam1.clone()],
                },
            ),
        ]);
        let filter = resolve_consumer_filter("extra", &bindings, Some(&depends_on));
        // `(core_b, cam1)` survives the pinned `(core_a, cam1)` claim and,
        // as the single remaining producer, collapses to a full pin.
        assert_eq!(filter, ConsumerFilter::Pin(other_core_cam1));
    }

    /// Statement 3 (from_any-only manifest): unbound from_any with no
    /// sibling claims resolves to a pure wildcard.
    #[test]
    fn from_any_unbound_without_siblings_is_any() {
        let depends_on = deps(vec![("camera", "v1", "extra", true)]);
        let bindings = slot_map(vec![("extra", SlotBinding::FromAnyUnbound)]);
        let filter = resolve_consumer_filter("extra", &bindings, Some(&depends_on));
        assert_eq!(filter, ConsumerFilter::Any);
    }

    /// Statement 1 precedence on the unbound from_any side: pinned
    /// sibling bound to producer P claims P; the unbound from_any
    /// wildcards everyone except P.
    #[test]
    fn from_any_unbound_excludes_pinned_claimed() {
        let depends_on = deps(vec![
            ("camera", "v1", "wrist_left", false),
            ("camera", "v1", "extra", true),
        ]);
        let bindings = slot_map(vec![
            (
                "wrist_left",
                SlotBinding::Pinned {
                    producer: pref("cam1"),
                },
            ),
            ("extra", SlotBinding::FromAnyUnbound),
        ]);
        let filter = resolve_consumer_filter("extra", &bindings, Some(&depends_on));
        assert_eq!(filter, ConsumerFilter::AnyExcept(vec![pref("cam1")]));
    }

    /// "Explicit bindings replace the wildcard fallback" — a from_any
    /// slot bound to A and B, plus an unbound from_any sibling: the
    /// unbound slot's reject set includes A and B (so a third producer
    /// C reaches the unbound slot, but A and B don't).
    #[test]
    fn unbound_from_any_excludes_explicit_from_any_claims() {
        let depends_on = deps(vec![
            ("camera", "v1", "specific", true),
            ("camera", "v1", "extra", true),
        ]);
        let bindings = slot_map(vec![
            (
                "specific",
                SlotBinding::FromAnyBound {
                    producers: vec![pref("cam_a"), pref("cam_b")],
                },
            ),
            ("extra", SlotBinding::FromAnyUnbound),
        ]);
        let filter = resolve_consumer_filter("extra", &bindings, Some(&depends_on));
        // BTreeSet → sorted iteration.
        assert_eq!(
            filter,
            ConsumerFilter::AnyExcept(vec![pref("cam_a"), pref("cam_b")])
        );
    }

    /// Cross-(name, tag) bindings don't leak: a pinned camera dep
    /// doesn't claim a producer for an unrelated lidar from_any.
    #[test]
    fn sibling_claims_are_scoped_per_name_tag() {
        let depends_on = deps(vec![
            ("camera", "v1", "cam_slot", false),
            ("lidar", "v1", "lidar_slot", true),
        ]);
        let bindings = slot_map(vec![
            (
                "cam_slot",
                SlotBinding::Pinned {
                    producer: pref("cam1"),
                },
            ),
            ("lidar_slot", SlotBinding::FromAnyUnbound),
        ]);
        let filter = resolve_consumer_filter("lidar_slot", &bindings, Some(&depends_on));
        assert_eq!(filter, ConsumerFilter::Any);
    }

    /// Defensive: no slot binding entry for `link_id` → wildcard. The
    /// validator should never produce this state, but the resolver
    /// must not panic.
    #[test]
    fn missing_slot_binding_falls_back_to_any() {
        let bindings: BTreeMap<String, SlotBinding> = BTreeMap::new();
        let filter = resolve_consumer_filter("nope", &bindings, None);
        assert_eq!(filter, ConsumerFilter::Any);
    }
}
