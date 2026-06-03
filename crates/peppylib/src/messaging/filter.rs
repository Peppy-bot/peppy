//! Consumer-side per-slot filter applied by the messaging layer to decide
//! which producer messages reach which `depends_on` slot. The validator
//! (in `config-internal::launcher::bindings`) pre-resolves each consumer
//! instance's launcher / CLI binding map into per-slot
//! [`config::runtime::SlotBinding`] entries; at startup, the runtime
//! [`crate::runtime::Processor`] reads each declared `link_id` and
//! synthesizes a [`ConsumerFilter`] for the subscribe / poll / send_goal
//! call.
//!
//! The four variants map directly to the spec's invariants:
//! - [`ConsumerFilter::Pin`] — wire-layer `from_instance_id` pin to a
//!   single producer. Used for pinned slots and from_any slots bound to
//!   exactly one producer.
//! - [`ConsumerFilter::OnlyFrom`] — wire wildcards; an in-process
//!   acceptance set filters incoming messages by source `instance_id`.
//!   Used for from_any slots bound to multiple producers.
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConsumerFilter {
    /// Wire-layer pin: subscribe with `from_instance_id = Some(id)`. No
    /// in-process filtering required.
    Pin(String),
    /// Wire wildcards; accept only messages whose source `instance_id`
    /// is in the set. The empty set is legal (and means "this slot
    /// receives nothing" — e.g. every bound producer was preempted by
    /// a pinned sibling).
    OnlyFrom(Vec<String>),
    /// Wire wildcards; drop messages whose source `instance_id` is in
    /// the set. The empty set degenerates to [`ConsumerFilter::Any`].
    AnyExcept(Vec<String>),
    /// Pure wildcard at the wire layer.
    Any,
}

impl ConsumerFilter {
    /// Service / action call sites use a single `target_instance_id`
    /// per call. Returns `Some(id)` when the filter targets exactly one
    /// producer ([`ConsumerFilter::Pin`]); otherwise `None`, in which
    /// case the call site falls back to wildcard discovery
    /// (services' discover-then-pin path) or fails up-front (actions
    /// require an explicit target).
    pub fn pinned_target(&self) -> Option<&str> {
        match self {
            ConsumerFilter::Pin(id) => Some(id.as_str()),
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
        // A deferred slot pins to its target exactly like `Pinned`: routing
        // bakes in the target `instance_id` and the transport delivers once
        // the producer appears. The only difference from `Pinned` lives in
        // validation/observability, not here.
        SlotBinding::Pinned {
            producer_instance_id,
        }
        | SlotBinding::Deferred {
            producer_instance_id,
        } => ConsumerFilter::Pin(producer_instance_id.clone()),
        SlotBinding::FromAnyBound {
            producer_instance_ids,
        } => {
            let pinned_claimed =
                pinned_claims_for_name_tag(slot_name_tag, slot_bindings, depends_on);
            let effective: Vec<String> = producer_instance_ids
                .iter()
                .filter(|id| !pinned_claimed.contains(id.as_str()))
                .cloned()
                .collect();
            // Degenerate `OnlyFrom([single])` → Pin for efficiency; the
            // wire layer can pin instead of paying the wildcard +
            // in-process filter cost.
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

/// All producer `instance_id`s claimed by pinned sibling slots on the
/// same `(name, tag)`.
fn pinned_claims_for_name_tag<'a>(
    name_tag: Option<(&str, &str)>,
    slot_bindings: &'a BTreeMap<String, SlotBinding>,
    depends_on: Option<&DependsOn>,
) -> BTreeSet<&'a str> {
    let mut out = BTreeSet::new();
    let Some((name, tag)) = name_tag else {
        return out;
    };
    for (dep_name, dep_tag, dep_link_id, from_any) in iter_deps(depends_on) {
        if from_any || dep_name != name || dep_tag != tag {
            continue;
        }
        // A deferred sibling claims its target for preemption purposes just
        // like a pinned one — it routes as a pin.
        if let Some(
            SlotBinding::Pinned {
                producer_instance_id,
            }
            | SlotBinding::Deferred {
                producer_instance_id,
            },
        ) = slot_bindings.get(dep_link_id)
        {
            out.insert(producer_instance_id.as_str());
        }
    }
    out
}

/// Every producer `instance_id` named by any sibling binding (pinned or
/// from_any explicit) on the same `(name, tag)`. Used to populate the
/// reject set for an unbound `from_any` slot.
fn all_sibling_claims_for_name_tag(
    name_tag: Option<(&str, &str)>,
    slot_bindings: &BTreeMap<String, SlotBinding>,
    depends_on: Option<&DependsOn>,
) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    let Some((name, tag)) = name_tag else {
        return out;
    };
    for (dep_name, dep_tag, dep_link_id, _from_any) in iter_deps(depends_on) {
        if dep_name != name || dep_tag != tag {
            continue;
        }
        match slot_bindings.get(dep_link_id) {
            // A deferred sibling claims its target like a pinned one.
            Some(SlotBinding::Pinned {
                producer_instance_id,
            })
            | Some(SlotBinding::Deferred {
                producer_instance_id,
            }) => {
                out.insert(producer_instance_id.clone());
            }
            Some(SlotBinding::FromAnyBound {
                producer_instance_ids,
            }) => {
                for id in producer_instance_ids {
                    out.insert(id.clone());
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
                producer_instance_id: "cam1".to_string(),
            },
        )]);
        let filter = resolve_consumer_filter("main", &bindings, Some(&depends_on));
        assert_eq!(filter, ConsumerFilter::Pin("cam1".to_string()));
    }

    /// A `Deferred` slot pins to its target exactly like `Pinned`: the wire
    /// layer bakes in the `instance_id` and the transport delivers once the
    /// producer appears.
    #[test]
    fn deferred_slot_resolves_to_pin() {
        let depends_on = deps(vec![("camera", "v1", "main", false)]);
        let bindings = slot_map(vec![(
            "main",
            SlotBinding::Deferred {
                producer_instance_id: "cam1".to_string(),
            },
        )]);
        let filter = resolve_consumer_filter("main", &bindings, Some(&depends_on));
        assert_eq!(filter, ConsumerFilter::Pin("cam1".to_string()));
    }

    /// A deferred sibling claims its target for from_any preemption just like
    /// a pinned one, so an unbound from_any slot on the same `(name, tag)`
    /// excludes the deferred-claimed producer.
    #[test]
    fn from_any_unbound_excludes_deferred_claimed_sibling() {
        let depends_on = deps(vec![
            ("camera", "v1", "wrist_left", false),
            ("camera", "v1", "extra", true),
        ]);
        let bindings = slot_map(vec![
            (
                "wrist_left",
                SlotBinding::Deferred {
                    producer_instance_id: "cam1".to_string(),
                },
            ),
            ("extra", SlotBinding::FromAnyUnbound),
        ]);
        let filter = resolve_consumer_filter("extra", &bindings, Some(&depends_on));
        assert_eq!(filter, ConsumerFilter::AnyExcept(vec!["cam1".to_string()]));
    }

    #[test]
    fn from_any_bound_to_single_producer_collapses_to_pin() {
        let depends_on = deps(vec![("camera", "v1", "extra", true)]);
        let bindings = slot_map(vec![(
            "extra",
            SlotBinding::FromAnyBound {
                producer_instance_ids: vec!["cam1".to_string()],
            },
        )]);
        let filter = resolve_consumer_filter("extra", &bindings, Some(&depends_on));
        assert_eq!(filter, ConsumerFilter::Pin("cam1".to_string()));
    }

    #[test]
    fn from_any_bound_to_multiple_producers_resolves_to_only_from() {
        let depends_on = deps(vec![("camera", "v1", "extra", true)]);
        let bindings = slot_map(vec![(
            "extra",
            SlotBinding::FromAnyBound {
                producer_instance_ids: vec!["cam1".to_string(), "cam2".to_string()],
            },
        )]);
        let filter = resolve_consumer_filter("extra", &bindings, Some(&depends_on));
        assert_eq!(
            filter,
            ConsumerFilter::OnlyFrom(vec!["cam1".to_string(), "cam2".to_string()])
        );
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
                    producer_instance_id: "cam1".to_string(),
                },
            ),
            (
                "extra",
                SlotBinding::FromAnyBound {
                    producer_instance_ids: vec!["cam1".to_string()],
                },
            ),
        ]);
        let filter = resolve_consumer_filter("extra", &bindings, Some(&depends_on));
        assert_eq!(filter, ConsumerFilter::OnlyFrom(vec![]));
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
                    producer_instance_id: "cam1".to_string(),
                },
            ),
            ("extra", SlotBinding::FromAnyUnbound),
        ]);
        let filter = resolve_consumer_filter("extra", &bindings, Some(&depends_on));
        assert_eq!(filter, ConsumerFilter::AnyExcept(vec!["cam1".to_string()]));
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
                    producer_instance_ids: vec!["cam_a".to_string(), "cam_b".to_string()],
                },
            ),
            ("extra", SlotBinding::FromAnyUnbound),
        ]);
        let filter = resolve_consumer_filter("extra", &bindings, Some(&depends_on));
        // BTreeSet → sorted iteration.
        assert_eq!(
            filter,
            ConsumerFilter::AnyExcept(vec!["cam_a".to_string(), "cam_b".to_string()])
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
                    producer_instance_id: "cam1".to_string(),
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
