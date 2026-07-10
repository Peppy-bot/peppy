//! Plan-phase validation for the launcher's per-instance `bindings`
//! field. Runs after node configs are loaded so the validator can
//! cross-reference each consumer's `depends_on` against the running
//! stack snapshot's `instance_id → (name, tag)` lookup.
//!
//! Every binding KEY is a `link_id` the consumer declares in
//! `depends_on.{nodes,interfaces}` — nothing else:
//!
//! - `KEY` matches a declared pinned (`from_any: false`) `link_id`: the
//!   value must name exactly one producer, which pins that slot.
//! - `KEY` matches a declared `from_any: true` `link_id`: the value's
//!   producers (one, or an array) become the slot's explicit bound set —
//!   the node receives from all of them and only them. An omitted key or
//!   an empty array leaves the slot deliberately **unbound**: valid, and
//!   silent at runtime (no subscription, no traffic).
//! - Any other `KEY` is dead (rejected). There is no free-form
//!   producer-matched key form.
//!
//! The validator emits both errors and the resolved per-slot
//! `SlotBinding` map per consumer instance — one entry for **every**
//! declared slot (this is what lets the node runtime treat a missing
//! entry as "standalone mode") — which the caller serializes into
//! [`config::runtime::NodeInstanceConfig::slot_bindings`].

use crate::error::{
    BindingInterfaceNotConformed, BindingMissingForPinnedDep, BindingTargetMismatch,
    DuplicateInstanceIdAcrossStack, ParsingError, SlotKind,
};
use config::node::{ConformsToItem, DependsOn};
use config::runtime::{ProducerRef, SlotBinding};
use std::collections::BTreeMap;

use super::types::DeploymentInstance;

/// Minimal view of one planned deployment needed for binding
/// validation. Built by the launcher with borrowed references to avoid
/// cloning the full planned-deployment graph; consumed by
/// [`validate_bindings`].
pub struct BindingValidationItem<'a> {
    pub node_name: &'a str,
    pub node_tag: &'a str,
    pub instances: &'a [DeploymentInstance],
    pub depends_on: Option<&'a DependsOn>,
    /// Producer's `interfaces.conforms_to` list, borrowed as a slice.
    /// Empty when the node declares no conformance. Used by the validator
    /// to decide whether this node can satisfy a consumer's interface
    /// slot.
    pub conforms_to: &'a [ConformsToItem],
}

/// Per-slot metadata extracted from `depends_on` during validation.
/// Carrying `kind` inline lets both pinned and from_any paths branch
/// without re-scanning `depends_on` per binding.
#[derive(Clone, Copy)]
struct SlotMeta<'a> {
    name: &'a str,
    tag: &'a str,
    kind: SlotKind,
}

/// Outcome of [`validate_bindings`]. `errors` aggregates every validator
/// rule violation; `slot_bindings` carries the resolved per-slot view
/// for every consumer instance whose bindings parsed cleanly. The caller
/// must check `errors.is_empty()` before consuming the resolution.
#[derive(Debug, Default)]
pub struct ValidatedBindings {
    pub errors: Vec<ParsingError>,
    /// `consumer_instance_id → link_id → SlotBinding`.
    pub slot_bindings: BTreeMap<String, BTreeMap<String, SlotBinding>>,
}

/// Run all binding validator rules over the snapshot. Returns
/// aggregated errors (ordering is deterministic across runs) plus the
/// resolved per-slot bindings for each consumer instance.
///
/// `producer_core_node` is the core_node of the daemon this stack
/// deploys under. The raw `--bind KEY@instance_id` syntax names
/// producers by `instance_id` alone (unique within one stack); the wire
/// addresses producers by the `(core_node, instance_id)` pair, so this
/// validator is the single point where every resolved binding is
/// stamped with the full [`ProducerRef`]. Stacks are daemon-scoped, so
/// every producer in the snapshot lives on the launching daemon. If
/// cross-daemon stacks ever land, the launcher knows each instance's
/// target daemon and the stamp generalizes to a per-instance input.
///
/// Rules enforced:
/// 1. Every pinned `depends_on` slot has a binding whose `KEY` equals
///    the slot's `link_id`, naming exactly one producer. Unbound →
///    [`ParsingError::BindingMissingForPinnedDep`]; zero or several
///    producers → [`ParsingError::BindingPinnedTakesOneTarget`]. A
///    `from_any` slot has **no** required-binding rule: unbound (or
///    bound to an empty array) is valid and resolves to
///    [`SlotBinding::FromAnyUnbound`] — a silent slot.
/// 2. Every binding `KEY` must be a declared `link_id` (pinned or
///    `from_any`); anything else is [`ParsingError::BindingDeadKey`].
///    Keys naming pairing slots get the targeted
///    [`ParsingError::BindingKeyIsPairingSlot`] instead.
/// 3. Every bound producer must satisfy the slot: node slots match the
///    producer's `(name, tag)` identity, interface slots match its
///    `conforms_to` — checked **per element** for `from_any` arrays.
///    Violations emit [`ParsingError::BindingTargetMismatch`] /
///    [`ParsingError::BindingInterfaceNotConformed`]; unknown
///    instance_ids emit [`ParsingError::UnknownInstanceId`].
/// 4. Binding keys are unique per instance — enforced by the CLI
///    accumulator and the deserializer; residual duplicates surface as
///    [`ParsingError::BindingDuplicateKey`] (defensive).
/// 5. Stack-wide `instance_id` uniqueness across every entry in
///    `items.instances` is enforced; collisions emit
///    [`ParsingError::DuplicateInstanceIdAcrossStack`].
pub fn validate_bindings(
    items: &[BindingValidationItem<'_>],
    producer_core_node: &str,
) -> ValidatedBindings {
    let mut out = ValidatedBindings::default();

    check_stack_wide_instance_id_uniqueness(items, &mut out.errors);

    let instance_to_item = build_instance_lookup(items);

    for item in items {
        let (declared_pinned, declared_from_any) = collect_declared_slots(item.depends_on);
        let declared_csv = format_declared_keys(&declared_pinned, &declared_from_any);
        // Pairing slots are never binding slots: `collect_declared_slots`
        // reads only `depends_on.{nodes,interfaces}`, so a pairing link_id
        // can never satisfy rule 1 or match a pinned KEY. This set exists
        // solely to turn a `--bind` on a pairing slot into a targeted
        // "use --pair" error instead of a generic `BindingDeadKey`.
        let pairing_link_ids: std::collections::BTreeSet<&str> = item
            .depends_on
            .map(|d| d.pairings.iter().map(|p| p.link_id.as_str()).collect())
            .unwrap_or_default();

        for instance in item.instances {
            let mut resolved: BTreeMap<String, SlotBinding> = BTreeMap::new();
            let mut seen_keys: std::collections::BTreeSet<&str> = std::collections::BTreeSet::new();
            // Pinned link_ids whose KEY appeared in the binding map
            // (even if the resolution errored). Used to skip rule 1's
            // pinned-unbound report so we don't double-emit on top of a
            // BindingTargetMismatch / UnknownInstanceId for the same
            // slot.
            let mut pinned_keys_seen: std::collections::BTreeSet<&str> =
                std::collections::BTreeSet::new();

            for (binding_key, targets) in &instance.bindings {
                // Rule 4 defensive check.
                if !seen_keys.insert(binding_key.as_str()) {
                    out.errors.push(ParsingError::BindingDuplicateKey {
                        owner_instance_id: instance.instance_id.to_string(),
                        binding: binding_key.clone(),
                    });
                    continue;
                }

                if pairing_link_ids.contains(binding_key.as_str()) {
                    out.errors.push(ParsingError::BindingKeyIsPairingSlot {
                        owner_instance_id: instance.instance_id.to_string(),
                        binding: binding_key.clone(),
                    });
                    continue;
                }

                if let Some(slot) = declared_pinned.get(binding_key.as_str()).copied() {
                    // KEY matches a declared pinned link_id: exactly one
                    // producer, satisfying the slot's identity.
                    pinned_keys_seen.insert(binding_key.as_str());
                    let [target_id] = targets.targets() else {
                        out.errors.push(ParsingError::BindingPinnedTakesOneTarget {
                            owner_instance_id: instance.instance_id.to_string(),
                            binding: binding_key.clone(),
                            target_count: targets.targets().len(),
                        });
                        continue;
                    };
                    if let Err(err) = check_bound_target(
                        &slot,
                        instance.instance_id.as_str(),
                        binding_key,
                        target_id,
                        &instance_to_item,
                    ) {
                        out.errors.push(err);
                        continue;
                    }
                    resolved.insert(
                        binding_key.clone(),
                        SlotBinding::Pinned {
                            producer: ProducerRef::new(producer_core_node, target_id.clone()),
                        },
                    );
                    continue;
                }

                if let Some(slot) = declared_from_any.get(binding_key.as_str()).copied() {
                    // KEY matches a declared from_any link_id: every
                    // element of the (possibly empty) target set must
                    // exist and satisfy the slot, checked per element by
                    // rule 3. Valid elements become the slot's explicit
                    // bound set; an empty or fully-rejected set falls
                    // through to the unbound materialization below, so
                    // `FromAnyBound { [] }` is never produced.
                    let mut producers = Vec::new();
                    for target_id in targets.targets() {
                        match check_bound_target(
                            &slot,
                            instance.instance_id.as_str(),
                            binding_key,
                            target_id,
                            &instance_to_item,
                        ) {
                            Ok(()) => producers
                                .push(ProducerRef::new(producer_core_node, target_id.clone())),
                            Err(err) => out.errors.push(err),
                        }
                    }
                    if !producers.is_empty() {
                        resolved
                            .insert(binding_key.clone(), SlotBinding::FromAnyBound { producers });
                    }
                    continue;
                }

                // Rule 2: KEY is not a declared link_id of any kind.
                out.errors.push(ParsingError::BindingDeadKey {
                    owner_instance_id: instance.instance_id.to_string(),
                    binding: binding_key.clone(),
                    declared_link_ids: declared_csv.clone(),
                });
            }

            // Every declared from_any slot the loop above left without a
            // bound set materializes as deliberately unbound (silent).
            // Per-key element uniqueness is a parse-time rule
            // (`BindingTargets`), so no dedupe here.
            for slot_link_id in declared_from_any.keys() {
                resolved
                    .entry((*slot_link_id).to_string())
                    .or_insert(SlotBinding::FromAnyUnbound);
            }

            // Rule 1: every pinned slot must be bound. Suppress when
            // the slot's KEY was present in the binding map but
            // errored elsewhere; surfacing both
            // `BindingTargetMismatch` and `BindingMissingForPinnedDep`
            // for the same slot is double-reporting one root cause.
            for (slot_link_id, slot) in &declared_pinned {
                if resolved.contains_key(*slot_link_id) {
                    continue;
                }
                if pinned_keys_seen.contains(*slot_link_id) {
                    continue;
                }
                out.errors
                    .push(ParsingError::BindingMissingForPinnedDep(Box::new(
                        BindingMissingForPinnedDep {
                            owner_instance_id: instance.instance_id.to_string(),
                            link_id: (*slot_link_id).to_string(),
                            kind: slot.kind,
                            expected_name: slot.name.to_string(),
                            expected_tag: slot.tag.to_string(),
                        },
                    )));
            }

            if !resolved.is_empty() {
                out.slot_bindings
                    .insert(instance.instance_id.to_string(), resolved);
            }
        }
    }

    out
}

/// Build `instance_id → BindingValidationItem` lookup. Duplicate IDs
/// across `items` are surfaced separately by
/// [`check_stack_wide_instance_id_uniqueness`]; this builder uses
/// insertion-wins (alphabetical first occurrence) so subsequent checks
/// still have a usable lookup even when a duplicate exists.
fn build_instance_lookup<'a>(
    items: &'a [BindingValidationItem<'a>],
) -> BTreeMap<&'a str, &'a BindingValidationItem<'a>> {
    let mut lookup = BTreeMap::new();
    for item in items {
        for instance in item.instances {
            lookup.entry(instance.instance_id.as_str()).or_insert(item);
        }
    }
    lookup
}

/// Stack-wide `instance_id` uniqueness (rule 7). Two entries anywhere
/// in `items.instances` (across any `(node_name, node_tag)`) sharing
/// an `instance_id` is a hard error: `--bind KEY@id` would be
/// ambiguous.
fn check_stack_wide_instance_id_uniqueness(
    items: &[BindingValidationItem<'_>],
    errors: &mut Vec<ParsingError>,
) {
    // (name, tag) of the first occurrence of each instance_id.
    let mut seen: BTreeMap<&str, (&str, &str)> = BTreeMap::new();
    for item in items {
        for instance in item.instances {
            let id = instance.instance_id.as_str();
            if let Some((name_a, tag_a)) = seen.get(id) {
                // Report every cross-item duplicate, even when the two
                // colliding entries share the same `(node_name,
                // node_tag)`. `deserialize_instances` only dedupes
                // within a single deployment's `instances` array, so two
                // SEPARATE planned items carrying the same `(name, tag)`
                // can each hold the same `instance_id` and slip past that
                // check. If we skipped them here, `build_instance_lookup`
                // would silently resolve the collision by first
                // insertion, making `--bind KEY@id` ambiguous.
                errors.push(ParsingError::DuplicateInstanceIdAcrossStack(Box::new(
                    DuplicateInstanceIdAcrossStack {
                        instance_id: id.to_string(),
                        name_a: (*name_a).to_string(),
                        tag_a: (*tag_a).to_string(),
                        name_b: item.node_name.to_string(),
                        tag_b: item.node_tag.to_string(),
                    },
                )));
            } else {
                seen.insert(id, (item.node_name, item.node_tag));
            }
        }
    }
}

type DeclaredSlots<'a> = BTreeMap<&'a str, SlotMeta<'a>>;

/// Split declared `depends_on` entries into pinned (`from_any: false`)
/// and `from_any: true` slots. Each map is keyed by `link_id` and
/// values carry the dep's `(name, tag, kind)` so the matching paths can
/// branch on node-vs-interface without re-scanning the manifest.
fn collect_declared_slots(
    depends_on: Option<&DependsOn>,
) -> (DeclaredSlots<'_>, DeclaredSlots<'_>) {
    let mut pinned = BTreeMap::new();
    let mut from_any = BTreeMap::new();
    if let Some(deps) = depends_on {
        for dep in &deps.nodes {
            let meta = SlotMeta {
                name: dep.name.as_str(),
                tag: dep.tag.as_str(),
                kind: SlotKind::Node,
            };
            if dep.from_any {
                from_any.insert(dep.link_id.as_str(), meta);
            } else {
                pinned.insert(dep.link_id.as_str(), meta);
            }
        }
        for dep in &deps.interfaces {
            let meta = SlotMeta {
                name: dep.name.as_str(),
                tag: dep.tag.as_str(),
                kind: SlotKind::Interface,
            };
            if dep.from_any {
                from_any.insert(dep.link_id.as_str(), meta);
            } else {
                pinned.insert(dep.link_id.as_str(), meta);
            }
        }
    }
    (pinned, from_any)
}

/// Does a producer satisfy a declared slot? Node slots match by
/// `(name, tag)` identity; interface slots match against the producer's
/// `conforms_to`. sha256 is not cross-checked here; each side
/// independently verifies its own declared sha256 against the on-disk
/// interface document at cache resolution time.
fn slot_matches_producer(slot: &SlotMeta<'_>, producer: &BindingValidationItem<'_>) -> bool {
    match slot.kind {
        SlotKind::Node => producer.node_name == slot.name && producer.node_tag == slot.tag,
        SlotKind::Interface => producer
            .conforms_to
            .iter()
            .any(|item| item.name.as_str() == slot.name && item.tag.as_str() == slot.tag),
    }
}

/// Rule 3 for one bound producer, shared by the pinned path and the
/// per-element from_any path so both enforce the same per-producer
/// contract: the target must name a known instance (else
/// [`ParsingError::UnknownInstanceId`]) and satisfy the slot via
/// [`slot_matches_producer`] (else the [`identity_mismatch_error`] for
/// the slot's kind). The caller decides what a success binds: the
/// pinned producer, or one element of a from_any bound set.
fn check_bound_target(
    slot: &SlotMeta<'_>,
    owner_instance_id: &str,
    binding_key: &str,
    target_id: &str,
    instance_to_item: &BTreeMap<&str, &BindingValidationItem<'_>>,
) -> Result<(), ParsingError> {
    let Some(target_item) = instance_to_item.get(target_id) else {
        return Err(ParsingError::UnknownInstanceId {
            owner_instance_id: owner_instance_id.to_string(),
            binding: binding_key.to_string(),
            instance_id: target_id.to_string(),
        });
    };
    if !slot_matches_producer(slot, target_item) {
        return Err(identity_mismatch_error(
            slot,
            owner_instance_id,
            binding_key,
            target_id,
            target_item,
        ));
    }
    Ok(())
}

/// The identity-mismatch error for a bound producer that does not satisfy
/// `slot` (rule 3): node slots report the deployed `(name, tag)` mismatch,
/// interface slots the missing `conforms_to` claim.
fn identity_mismatch_error(
    slot: &SlotMeta<'_>,
    owner_instance_id: &str,
    binding_key: &str,
    target_id: &str,
    target_item: &BindingValidationItem<'_>,
) -> ParsingError {
    match slot.kind {
        SlotKind::Node => ParsingError::BindingTargetMismatch(Box::new(BindingTargetMismatch {
            owner_instance_id: owner_instance_id.to_string(),
            binding: binding_key.to_string(),
            target_instance_id: target_id.to_string(),
            expected_name: slot.name.to_string(),
            expected_tag: slot.tag.to_string(),
            actual_name: target_item.node_name.to_string(),
            actual_tag: target_item.node_tag.to_string(),
        })),
        SlotKind::Interface => {
            ParsingError::BindingInterfaceNotConformed(Box::new(BindingInterfaceNotConformed {
                owner_instance_id: owner_instance_id.to_string(),
                binding: binding_key.to_string(),
                target_instance_id: target_id.to_string(),
                interface_name: slot.name.to_string(),
                interface_tag: slot.tag.to_string(),
                producer_name: target_item.node_name.to_string(),
                producer_tag: target_item.node_tag.to_string(),
            }))
        }
    }
}

fn format_declared_keys(pinned: &DeclaredSlots<'_>, from_any: &DeclaredSlots<'_>) -> String {
    let mut keys: Vec<&str> = pinned.keys().chain(from_any.keys()).copied().collect();
    keys.sort();
    keys.dedup();
    keys.join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The launching daemon's core_node stamped into every resolved
    /// binding by these tests.
    const TEST_CORE: &str = "core_a";

    fn parse_instances(json5: &str) -> Vec<DeploymentInstance> {
        serde_json5::from_str(json5).expect("instances fixture should parse")
    }

    fn parse_depends_on(json5: &str) -> DependsOn {
        serde_json5::from_str(json5).expect("depends_on fixture should parse")
    }

    fn parse_conforms_to(json5: &str) -> Vec<ConformsToItem> {
        serde_json5::from_str(json5).expect("conforms_to fixture should parse")
    }

    /// Convenience: build a `BindingValidationItem` whose lifetimes
    /// stay tethered to the caller's locals.
    fn item<'a>(
        node_name: &'a str,
        node_tag: &'a str,
        instances: &'a [DeploymentInstance],
        depends_on: Option<&'a DependsOn>,
    ) -> BindingValidationItem<'a> {
        BindingValidationItem {
            node_name,
            node_tag,
            instances,
            depends_on,
            conforms_to: &[],
        }
    }

    /// Like `item` but also threads a `conforms_to` slice, for tests
    /// that exercise interface-conformance matching.
    fn item_with_conforms_to<'a>(
        node_name: &'a str,
        node_tag: &'a str,
        instances: &'a [DeploymentInstance],
        depends_on: Option<&'a DependsOn>,
        conforms_to: &'a [ConformsToItem],
    ) -> BindingValidationItem<'a> {
        BindingValidationItem {
            node_name,
            node_tag,
            instances,
            depends_on,
            conforms_to,
        }
    }

    fn slot_binding(out: &ValidatedBindings, instance: &str, link_id: &str) -> Option<SlotBinding> {
        out.slot_bindings
            .get(instance)
            .and_then(|m| m.get(link_id))
            .cloned()
    }

    #[test]
    fn empty_planned_set_returns_no_errors() {
        let out = validate_bindings(&[], TEST_CORE);
        assert!(out.errors.is_empty());
        assert!(out.slot_bindings.is_empty());
    }

    /// A consumer with no `depends_on` and no `bindings` is trivially
    /// valid.
    #[test]
    fn consumer_without_depends_on_and_without_bindings_is_valid() {
        let instances = parse_instances(r#"[{ instance_id: "cons1" }]"#);
        let items = vec![item("cons", "v1", &instances, None)];
        let out = validate_bindings(&items, TEST_CORE);
        assert!(out.errors.is_empty(), "unexpected errors: {:?}", out.errors);
        assert!(out.slot_bindings.is_empty());
    }

    /// Rule 2: a binding whose KEY is not a declared link_id is dead —
    /// regardless of what its value targets.
    #[test]
    fn rule2_rejects_dead_binding_key() {
        let instances = parse_instances(
            r#"[{
                instance_id: "cons1",
                bindings: { main: "prod1", stale_slot: "prod1" }
            }]"#,
        );
        let depends_on = parse_depends_on(
            r#"{
                nodes: [{ name: "camera", tag: "v1", link_id: "main" }]
            }"#,
        );
        let prod_instances = parse_instances(r#"[{ instance_id: "prod1" }]"#);
        let items = vec![
            item("cons", "v1", &instances, Some(&depends_on)),
            item("camera", "v1", &prod_instances, None),
        ];
        let out = validate_bindings(&items, TEST_CORE);
        assert_eq!(
            out.errors.len(),
            1,
            "expected one error, got {:?}",
            out.errors
        );
        let ParsingError::BindingDeadKey {
            owner_instance_id,
            binding,
            declared_link_ids,
        } = &out.errors[0]
        else {
            panic!("expected BindingDeadKey, got {:?}", out.errors[0]);
        };
        assert_eq!(owner_instance_id, "cons1");
        assert_eq!(binding, "stale_slot");
        assert_eq!(declared_link_ids, "main");
    }

    /// Rule 1: pinned-unbound is a hard error.
    #[test]
    fn rule1_rejects_pinned_unbound() {
        let instances = parse_instances(r#"[{ instance_id: "cons1" }]"#);
        let depends_on = parse_depends_on(
            r#"{
                nodes: [{ name: "camera", tag: "v1", link_id: "main" }]
            }"#,
        );
        let items = vec![item("cons", "v1", &instances, Some(&depends_on))];
        let out = validate_bindings(&items, TEST_CORE);
        assert_eq!(out.errors.len(), 1);
        let ParsingError::BindingMissingForPinnedDep(info) = &out.errors[0] else {
            panic!(
                "expected BindingMissingForPinnedDep, got {:?}",
                out.errors[0]
            );
        };
        assert_eq!(info.owner_instance_id, "cons1");
        assert_eq!(info.link_id, "main");
        assert_eq!(info.kind, SlotKind::Node);
        assert_eq!(info.expected_name, "camera");
        assert_eq!(info.expected_tag, "v1");
        let msg = info.to_string();
        assert!(
            msg.contains("slot `main` is unbound"),
            "unexpected error message: {msg}"
        );
        assert!(
            msg.contains("expected node `camera:v1`"),
            "unexpected error message: {msg}"
        );
    }

    /// Rule 2 (happy path): pinned binding resolves to
    /// `SlotBinding::Pinned`.
    #[test]
    fn rule2_pinned_binding_resolves_to_pinned() {
        let cons_instances = parse_instances(
            r#"[{
                instance_id: "cons1",
                bindings: { main: "prod1" }
            }]"#,
        );
        let depends_on = parse_depends_on(
            r#"{
                nodes: [{ name: "camera", tag: "v1", link_id: "main" }]
            }"#,
        );
        let prod_instances = parse_instances(r#"[{ instance_id: "prod1" }]"#);
        let items = vec![
            item("cons", "v1", &cons_instances, Some(&depends_on)),
            item("camera", "v1", &prod_instances, None),
        ];
        let out = validate_bindings(&items, TEST_CORE);
        assert!(out.errors.is_empty(), "unexpected errors: {:?}", out.errors);
        assert_eq!(
            slot_binding(&out, "cons1", "main"),
            Some(SlotBinding::Pinned {
                producer: ProducerRef::new(TEST_CORE, "prod1")
            })
        );
    }

    /// Rule 3 (happy path): a from_any slot binds by its own link_id;
    /// the scalar form names a single producer.
    #[test]
    fn from_any_slot_binds_by_link_id_scalar() {
        let cons_instances = parse_instances(
            r#"[{
                instance_id: "cons1",
                bindings: { extra: "prod1" }
            }]"#,
        );
        let depends_on = parse_depends_on(
            r#"{
                nodes: [{ name: "camera", tag: "v1", link_id: "extra", from_any: true }]
            }"#,
        );
        let prod_instances = parse_instances(r#"[{ instance_id: "prod1" }]"#);
        let items = vec![
            item("cons", "v1", &cons_instances, Some(&depends_on)),
            item("camera", "v1", &prod_instances, None),
        ];
        let out = validate_bindings(&items, TEST_CORE);
        assert!(out.errors.is_empty(), "unexpected errors: {:?}", out.errors);
        assert_eq!(
            slot_binding(&out, "cons1", "extra"),
            Some(SlotBinding::FromAnyBound {
                producers: vec![ProducerRef::new(TEST_CORE, "prod1")]
            })
        );
    }

    /// Rule 3: an array value binds every listed producer to the slot,
    /// preserving declaration order.
    #[test]
    fn from_any_slot_binds_array_of_producers_in_order() {
        let cons_instances = parse_instances(
            r#"[{
                instance_id: "cons1",
                bindings: { extra: ["prod2", "prod1"] }
            }]"#,
        );
        let depends_on = parse_depends_on(
            r#"{
                nodes: [{ name: "camera", tag: "v1", link_id: "extra", from_any: true }]
            }"#,
        );
        let prod_instances = parse_instances(
            r#"[
                { instance_id: "prod1" },
                { instance_id: "prod2" }
            ]"#,
        );
        let items = vec![
            item("cons", "v1", &cons_instances, Some(&depends_on)),
            item("camera", "v1", &prod_instances, None),
        ];
        let out = validate_bindings(&items, TEST_CORE);
        assert!(out.errors.is_empty(), "unexpected errors: {:?}", out.errors);
        assert_eq!(
            slot_binding(&out, "cons1", "extra"),
            Some(SlotBinding::FromAnyBound {
                producers: vec![
                    ProducerRef::new(TEST_CORE, "prod2"),
                    ProducerRef::new(TEST_CORE, "prod1"),
                ]
            })
        );
    }

    /// An explicit empty array is valid and identical to omitting the
    /// key: the slot materializes as deliberately unbound, never as
    /// `FromAnyBound { [] }`.
    #[test]
    fn from_any_empty_array_resolves_to_unbound() {
        let cons_instances = parse_instances(
            r#"[{
                instance_id: "cons1",
                bindings: { extra: [] }
            }]"#,
        );
        let depends_on = parse_depends_on(
            r#"{
                nodes: [{ name: "camera", tag: "v1", link_id: "extra", from_any: true }]
            }"#,
        );
        let items = vec![item("cons", "v1", &cons_instances, Some(&depends_on))];
        let out = validate_bindings(&items, TEST_CORE);
        assert!(out.errors.is_empty(), "unexpected errors: {:?}", out.errors);
        assert_eq!(
            slot_binding(&out, "cons1", "extra"),
            Some(SlotBinding::FromAnyUnbound)
        );
    }

    /// Rule 1: a pinned slot binds exactly one producer — an array of
    /// two (or an empty array) on a pinned link_id is rejected, and
    /// rule 1's unbound report is suppressed (one root cause, one
    /// error).
    #[test]
    fn pinned_slot_rejects_array_binding() {
        let cons_instances = parse_instances(
            r#"[{
                instance_id: "cons1",
                bindings: { main: ["prod1", "prod2"] }
            }]"#,
        );
        let depends_on = parse_depends_on(
            r#"{
                nodes: [{ name: "camera", tag: "v1", link_id: "main" }]
            }"#,
        );
        let prod_instances = parse_instances(
            r#"[
                { instance_id: "prod1" },
                { instance_id: "prod2" }
            ]"#,
        );
        let items = vec![
            item("cons", "v1", &cons_instances, Some(&depends_on)),
            item("camera", "v1", &prod_instances, None),
        ];
        let out = validate_bindings(&items, TEST_CORE);
        assert_eq!(out.errors.len(), 1, "errors: {:?}", out.errors);
        let ParsingError::BindingPinnedTakesOneTarget {
            owner_instance_id,
            binding,
            target_count,
        } = &out.errors[0]
        else {
            panic!(
                "expected BindingPinnedTakesOneTarget, got {:?}",
                out.errors[0]
            );
        };
        assert_eq!(owner_instance_id, "cons1");
        assert_eq!(binding, "main");
        assert_eq!(*target_count, 2);
    }

    /// Rule 2: a key that is not a declared link_id is dead even when a
    /// from_any slot whose type matches the target exists — the retired
    /// free-form producer-matched form must NOT resurface.
    #[test]
    fn non_link_id_key_is_dead_even_when_type_matching_from_any_exists() {
        let cons_instances = parse_instances(
            r#"[{
                instance_id: "cons1",
                bindings: { the_extra: "prod1" }
            }]"#,
        );
        let depends_on = parse_depends_on(
            r#"{
                nodes: [{ name: "camera", tag: "v1", link_id: "extra", from_any: true }]
            }"#,
        );
        // The target IS a camera:v1 — under the retired free-form
        // mechanism this binding would have attached to `extra`.
        let prod_instances = parse_instances(r#"[{ instance_id: "prod1" }]"#);
        let items = vec![
            item("cons", "v1", &cons_instances, Some(&depends_on)),
            item("camera", "v1", &prod_instances, None),
        ];
        let out = validate_bindings(&items, TEST_CORE);
        assert_eq!(out.errors.len(), 1, "errors: {:?}", out.errors);
        assert!(matches!(out.errors[0], ParsingError::BindingDeadKey { .. }));
        // The unmatched slot stays deliberately unbound.
        assert_eq!(
            slot_binding(&out, "cons1", "extra"),
            Some(SlotBinding::FromAnyUnbound)
        );
    }

    /// Rule 3 runs per element of an array: a good element binds, a
    /// wrong-identity element errors, an unknown element errors — all in
    /// one pass.
    #[test]
    fn from_any_array_validates_each_element() {
        let cons_instances = parse_instances(
            r#"[{
                instance_id: "cons1",
                bindings: { extra: ["cam_ok", "actually_lidar", "ghost"] }
            }]"#,
        );
        let depends_on = parse_depends_on(
            r#"{
                nodes: [{ name: "camera", tag: "v1", link_id: "extra", from_any: true }]
            }"#,
        );
        let cam_instances = parse_instances(r#"[{ instance_id: "cam_ok" }]"#);
        let lidar_instances = parse_instances(r#"[{ instance_id: "actually_lidar" }]"#);
        let items = vec![
            item("cons", "v1", &cons_instances, Some(&depends_on)),
            item("camera", "v1", &cam_instances, None),
            item("lidar", "v1", &lidar_instances, None),
        ];
        let out = validate_bindings(&items, TEST_CORE);
        assert_eq!(out.errors.len(), 2, "errors: {:?}", out.errors);
        let ParsingError::BindingTargetMismatch(mismatch) = &out.errors[0] else {
            panic!("expected BindingTargetMismatch, got {:?}", out.errors[0]);
        };
        assert_eq!(mismatch.target_instance_id, "actually_lidar");
        assert_eq!(mismatch.binding, "extra");
        let ParsingError::UnknownInstanceId { instance_id, .. } = &out.errors[1] else {
            panic!("expected UnknownInstanceId, got {:?}", out.errors[1]);
        };
        assert_eq!(instance_id, "ghost");
        // The valid element still binds.
        assert_eq!(
            slot_binding(&out, "cons1", "extra"),
            Some(SlotBinding::FromAnyBound {
                producers: vec![ProducerRef::new(TEST_CORE, "cam_ok")]
            })
        );
    }

    /// A `from_any` slot with no bindings resolves to
    /// `SlotBinding::FromAnyUnbound`.
    #[test]
    fn from_any_without_bindings_resolves_to_unbound() {
        let cons_instances = parse_instances(r#"[{ instance_id: "cons1" }]"#);
        let depends_on = parse_depends_on(
            r#"{
                nodes: [{ name: "camera", tag: "v1", link_id: "extra", from_any: true }]
            }"#,
        );
        let items = vec![item("cons", "v1", &cons_instances, Some(&depends_on))];
        let out = validate_bindings(&items, TEST_CORE);
        assert!(out.errors.is_empty(), "unexpected errors: {:?}", out.errors);
        assert_eq!(
            slot_binding(&out, "cons1", "extra"),
            Some(SlotBinding::FromAnyUnbound)
        );
    }

    /// Rule 1 (interface variant): pinned interface dep without
    /// binding fails the same way.
    #[test]
    fn rule1_rejects_missing_binding_for_pinned_interface_dep() {
        let instances = parse_instances(r#"[{ instance_id: "cons1" }]"#);
        let depends_on = parse_depends_on(
            r#"{
                nodes: [],
                interfaces: [{
                    name: "depth_camera",
                    tag: "v1",
                    link_id: "depth"
                }]
            }"#,
        );
        let items = vec![item("cons", "v1", &instances, Some(&depends_on))];
        let out = validate_bindings(&items, TEST_CORE);
        assert_eq!(out.errors.len(), 1);
        let ParsingError::BindingMissingForPinnedDep(info) = &out.errors[0] else {
            panic!(
                "expected BindingMissingForPinnedDep, got {:?}",
                out.errors[0]
            );
        };
        assert_eq!(info.kind, SlotKind::Interface);
        assert_eq!(info.link_id, "depth");
    }

    /// Rule 5: pinned binding whose target deploys the wrong node.
    #[test]
    fn rule5_rejects_target_node_mismatch() {
        let cons_instances = parse_instances(
            r#"[{
                instance_id: "cons1",
                bindings: { main: "actually_lidar" }
            }]"#,
        );
        let depends_on = parse_depends_on(
            r#"{
                nodes: [{ name: "camera", tag: "v1", link_id: "main" }]
            }"#,
        );
        let prod_instances = parse_instances(r#"[{ instance_id: "actually_lidar" }]"#);
        let items = vec![
            item("cons", "v1", &cons_instances, Some(&depends_on)),
            item("lidar", "v1", &prod_instances, None),
        ];
        let out = validate_bindings(&items, TEST_CORE);
        assert_eq!(out.errors.len(), 1);
        let ParsingError::BindingTargetMismatch(info) = &out.errors[0] else {
            panic!("expected BindingTargetMismatch, got {:?}", out.errors[0]);
        };
        assert_eq!(info.owner_instance_id, "cons1");
        assert_eq!(info.binding, "main");
        assert_eq!(info.target_instance_id, "actually_lidar");
    }

    /// Pinned interface bindings check the producer's `conforms_to`
    /// (not just node identity). A producer with no matching
    /// `conforms_to` entry is rejected with
    /// `BindingInterfaceNotConformed`.
    #[test]
    fn pinned_interface_binding_rejects_non_conforming_producer() {
        let cons_instances = parse_instances(
            r#"[{
                instance_id: "cons1",
                bindings: { depth: "any_producer" }
            }]"#,
        );
        let depends_on = parse_depends_on(
            r#"{
                nodes: [],
                interfaces: [{
                    name: "depth_camera",
                    tag: "v1",
                    link_id: "depth"
                }]
            }"#,
        );
        let prod_instances = parse_instances(r#"[{ instance_id: "any_producer" }]"#);
        let items = vec![
            item("cons", "v1", &cons_instances, Some(&depends_on)),
            item("whatever", "v1", &prod_instances, None),
        ];
        let out = validate_bindings(&items, TEST_CORE);
        assert_eq!(out.errors.len(), 1, "errors: {:?}", out.errors);
        let ParsingError::BindingInterfaceNotConformed(info) = &out.errors[0] else {
            panic!(
                "expected BindingInterfaceNotConformed, got {:?}",
                out.errors[0]
            );
        };
        assert_eq!(info.binding, "depth");
        assert_eq!(info.interface_name, "depth_camera");
        assert_eq!(info.interface_tag, "v1");
        assert_eq!(info.producer_name, "whatever");
        assert_eq!(info.producer_tag, "v1");
    }

    /// Pinned interface dep targets a producer whose `conforms_to`
    /// includes the requested interface: accepted as `SlotBinding::Pinned`.
    /// The producer's node name is intentionally different from the
    /// interface name so this test exercises the conformance path
    /// rather than a coincidental identity match.
    #[test]
    fn pinned_interface_binding_accepts_conforming_producer() {
        let cons_instances = parse_instances(
            r#"[{
                instance_id: "cons1",
                bindings: { depth: "webcam_inst_1" }
            }]"#,
        );
        let depends_on = parse_depends_on(
            r#"{
                nodes: [],
                interfaces: [{
                    name: "depth_camera",
                    tag: "v1",
                    link_id: "depth"
                }]
            }"#,
        );
        let prod_instances = parse_instances(r#"[{ instance_id: "webcam_inst_1" }]"#);
        let producer_conforms = parse_conforms_to(r#"[{ name: "depth_camera", tag: "v1" }]"#);
        let items = vec![
            item("cons", "v1", &cons_instances, Some(&depends_on)),
            item_with_conforms_to("webcam", "v1", &prod_instances, None, &producer_conforms),
        ];
        let out = validate_bindings(&items, TEST_CORE);
        assert!(out.errors.is_empty(), "unexpected errors: {:?}", out.errors);
        assert_eq!(
            slot_binding(&out, "cons1", "depth"),
            Some(SlotBinding::Pinned {
                producer: ProducerRef::new(TEST_CORE, "webcam_inst_1")
            })
        );
    }

    /// A well-formed launcher (matching the spec's openarm01_backbone
    /// example: two pinned slots and a from_any slot) passes with no
    /// errors and all slots resolve.
    #[test]
    fn openarm_style_manifest_resolves_all_slots() {
        let cons_instances = parse_instances(
            r#"[{
                instance_id: "backbone_inst_1",
                bindings: {
                    wrist_left_camera: "depth_cam_inst1",
                    wrist_right_camera: "depth_cam_inst1",
                    extra_cam: "depth_cam_inst1"
                }
            }]"#,
        );
        let depends_on = parse_depends_on(
            r#"{
                nodes: [],
                interfaces: [
                    { name: "depth_camera", tag: "v1", link_id: "wrist_left_camera" },
                    { name: "depth_camera", tag: "v1", link_id: "wrist_right_camera" },
                    { name: "depth_camera", tag: "v1", link_id: "extra_cam", from_any: true }
                ]
            }"#,
        );
        let prod_instances = parse_instances(r#"[{ instance_id: "depth_cam_inst1" }]"#);
        // Producer's node name coincidentally matches the interface
        // name+tag, but the validator only honors explicit `conforms_to`
        // claims; node-identity matching never satisfies an interface
        // slot.
        let producer_conforms = parse_conforms_to(r#"[{ name: "depth_camera", tag: "v1" }]"#);
        let items = vec![
            item(
                "openarm01_backbone",
                "v1",
                &cons_instances,
                Some(&depends_on),
            ),
            item_with_conforms_to(
                "depth_camera",
                "v1",
                &prod_instances,
                None,
                &producer_conforms,
            ),
        ];
        let out = validate_bindings(&items, TEST_CORE);
        assert!(out.errors.is_empty(), "unexpected errors: {:?}", out.errors);
        assert_eq!(
            slot_binding(&out, "backbone_inst_1", "wrist_left_camera"),
            Some(SlotBinding::Pinned {
                producer: ProducerRef::new(TEST_CORE, "depth_cam_inst1")
            })
        );
        assert_eq!(
            slot_binding(&out, "backbone_inst_1", "wrist_right_camera"),
            Some(SlotBinding::Pinned {
                producer: ProducerRef::new(TEST_CORE, "depth_cam_inst1")
            })
        );
        assert_eq!(
            slot_binding(&out, "backbone_inst_1", "extra_cam"),
            Some(SlotBinding::FromAnyBound {
                producers: vec![ProducerRef::new(TEST_CORE, "depth_cam_inst1")]
            })
        );
    }

    /// An "inert" item (`depends_on: None`) must NOT trigger Rule 1
    /// against the slots it would have declared if `depends_on` were
    /// populated; it represents a node whose bindings were already
    /// resolved at spawn time. At the same time, its instances and
    /// `conforms_to` must still feed the producer-lookup index so a
    /// live consumer in the same `validate_bindings` call can satisfy
    /// pinned node / interface deps against them.
    ///
    /// This locks in the contract that
    /// `peppy::commands::node::run::validate_binds_against_stack`
    /// relies on when it folds already-running stack nodes into the
    /// validator snapshot without re-checking their bindings.
    #[test]
    fn inert_item_with_no_depends_on_does_not_trigger_rule1_but_remains_a_producer() {
        // Live consumer with a pinned node dep + a pinned interface dep.
        let cons_instances = parse_instances(
            r#"[{
                instance_id: "cons1",
                bindings: { cam: "node_prod_inst", depth: "iface_prod_inst" }
            }]"#,
        );
        let cons_depends_on = parse_depends_on(
            r#"{
                nodes: [{ name: "camera", tag: "v1", link_id: "cam" }],
                interfaces: [{ name: "depth_camera", tag: "v1", link_id: "depth" }]
            }"#,
        );

        // Inert node producer: depends_on omitted entirely, even though
        // it WOULD declare deps in real life. If Rule 1 fired against
        // inert items, this is where the false positive would surface.
        let node_prod_instances = parse_instances(r#"[{ instance_id: "node_prod_inst" }]"#);

        // Inert interface producer: same shape, plus a `conforms_to`
        // entry that should still match the consumer's interface dep.
        let iface_prod_instances = parse_instances(r#"[{ instance_id: "iface_prod_inst" }]"#);
        let iface_prod_conforms = parse_conforms_to(r#"[{ name: "depth_camera", tag: "v1" }]"#);

        let items = vec![
            item("cons", "v1", &cons_instances, Some(&cons_depends_on)),
            // Inert items: depends_on intentionally `None`.
            item("camera", "v1", &node_prod_instances, None),
            item_with_conforms_to(
                "webcam",
                "v1",
                &iface_prod_instances,
                None,
                &iface_prod_conforms,
            ),
        ];
        let out = validate_bindings(&items, TEST_CORE);
        assert!(out.errors.is_empty(), "unexpected errors: {:?}", out.errors);
        assert_eq!(
            slot_binding(&out, "cons1", "cam"),
            Some(SlotBinding::Pinned {
                producer: ProducerRef::new(TEST_CORE, "node_prod_inst")
            })
        );
        assert_eq!(
            slot_binding(&out, "cons1", "depth"),
            Some(SlotBinding::Pinned {
                producer: ProducerRef::new(TEST_CORE, "iface_prod_inst")
            })
        );
    }

    /// Defensive: an instance lookup miss for a binding target.
    #[test]
    fn rejects_binding_whose_target_is_unknown_to_planner() {
        let cons_instances = parse_instances(
            r#"[{
                instance_id: "cons1",
                bindings: { main: "ghost_producer" }
            }]"#,
        );
        let depends_on = parse_depends_on(
            r#"{
                nodes: [{ name: "camera", tag: "v1", link_id: "main" }]
            }"#,
        );
        let items = vec![item("cons", "v1", &cons_instances, Some(&depends_on))];
        let out = validate_bindings(&items, TEST_CORE);
        assert_eq!(out.errors.len(), 1);
        let ParsingError::UnknownInstanceId {
            owner_instance_id,
            binding,
            instance_id,
        } = &out.errors[0]
        else {
            panic!("expected UnknownInstanceId, got {:?}", out.errors[0]);
        };
        assert_eq!(owner_instance_id, "cons1");
        assert_eq!(binding, "main");
        assert_eq!(instance_id, "ghost_producer");
    }

    /// Rule 7: stack-wide instance_id duplicate across different
    /// (node_name, node_tag).
    #[test]
    fn rule7_rejects_stack_wide_duplicate_instance_id() {
        let camera_instances = parse_instances(r#"[{ instance_id: "shared_inst" }]"#);
        let lidar_instances = parse_instances(r#"[{ instance_id: "shared_inst" }]"#);
        let items = vec![
            item("camera", "v1", &camera_instances, None),
            item("lidar", "v1", &lidar_instances, None),
        ];
        let out = validate_bindings(&items, TEST_CORE);
        assert_eq!(out.errors.len(), 1, "errors: {:?}", out.errors);
        let ParsingError::DuplicateInstanceIdAcrossStack(info) = &out.errors[0] else {
            panic!(
                "expected DuplicateInstanceIdAcrossStack, got {:?}",
                out.errors[0]
            );
        };
        assert_eq!(info.instance_id, "shared_inst");
        assert_eq!(info.name_a, "camera");
        assert_eq!(info.tag_a, "v1");
        assert_eq!(info.name_b, "lidar");
        assert_eq!(info.tag_b, "v1");
    }

    /// Stack-wide check reports duplicate `instance_id`s even when the
    /// colliding entries share the same `(name, tag)`. In real parsing
    /// the deserializer's `deserialize_instances` rejects intra-array
    /// duplicates before they reach the validator, but the validator
    /// cannot distinguish that case from two separate planned items that
    /// happen to share `(name, tag)`, so it reports defensively rather
    /// than silently letting `build_instance_lookup` resolve by first
    /// insertion.
    #[test]
    fn rule7_reports_duplicate_instance_id_even_for_same_name_tag() {
        let camera_instances = parse_instances(
            r#"[
                { instance_id: "shared_inst" },
                { instance_id: "shared_inst" }
            ]"#,
        );
        let items = vec![item("camera", "v1", &camera_instances, None)];
        let out = validate_bindings(&items, TEST_CORE);
        assert_eq!(out.errors.len(), 1, "errors: {:?}", out.errors);
        let ParsingError::DuplicateInstanceIdAcrossStack(info) = &out.errors[0] else {
            panic!(
                "expected DuplicateInstanceIdAcrossStack, got {:?}",
                out.errors[0]
            );
        };
        assert_eq!(info.instance_id, "shared_inst");
        assert_eq!(info.name_a, "camera");
        assert_eq!(info.tag_a, "v1");
        assert_eq!(info.name_b, "camera");
        assert_eq!(info.tag_b, "v1");
    }

    /// Pinned and from_any errors aggregate (no short-circuit).
    #[test]
    fn aggregates_multiple_errors() {
        let cons_instances = parse_instances(
            r#"[{
                instance_id: "cons1",
                bindings: { unknown_slot: "prod1" }
            }]"#,
        );
        let depends_on = parse_depends_on(
            r#"{
                nodes: [{ name: "camera", tag: "v1", link_id: "main" }]
            }"#,
        );
        let prod_instances = parse_instances(r#"[{ instance_id: "prod1" }]"#);
        let items = vec![
            item("cons", "v1", &cons_instances, Some(&depends_on)),
            item("camera", "v1", &prod_instances, None),
        ];
        let out = validate_bindings(&items, TEST_CORE);
        assert_eq!(
            out.errors.len(),
            2,
            "expected two errors, got {:?}",
            out.errors
        );
        // BindingDeadKey is emitted first (in iteration order),
        // BindingMissingForPinnedDep after.
        assert!(matches!(out.errors[0], ParsingError::BindingDeadKey { .. }));
        assert!(matches!(
            out.errors[1],
            ParsingError::BindingMissingForPinnedDep(_)
        ));
    }

    /// A `from_any` interface dep accepts a producer whose
    /// `interfaces.conforms_to` includes the requested interface, even
    /// when the producer's node name differs from the interface name.
    #[test]
    fn from_any_interface_dep_accepts_producer_via_conforms_to() {
        let cons_instances = parse_instances(
            r#"[{
                instance_id: "cons1",
                bindings: { extra_cam: "webcam_inst_1" }
            }]"#,
        );
        let depends_on = parse_depends_on(
            r#"{
                nodes: [],
                interfaces: [{
                    name: "depth_camera",
                    tag: "v1",
                    link_id: "extra_cam",
                    from_any: true
                }]
            }"#,
        );
        let prod_instances = parse_instances(r#"[{ instance_id: "webcam_inst_1" }]"#);
        let producer_conforms = parse_conforms_to(r#"[{ name: "depth_camera", tag: "v1" }]"#);
        let items = vec![
            item("cons", "v1", &cons_instances, Some(&depends_on)),
            item_with_conforms_to("webcam", "v1", &prod_instances, None, &producer_conforms),
        ];
        let out = validate_bindings(&items, TEST_CORE);
        assert!(out.errors.is_empty(), "unexpected errors: {:?}", out.errors);
        assert_eq!(
            slot_binding(&out, "cons1", "extra_cam"),
            Some(SlotBinding::FromAnyBound {
                producers: vec![ProducerRef::new(TEST_CORE, "webcam_inst_1")]
            })
        );
    }

    /// A `from_any` interface dep rejects a producer that lacks the
    /// matching `conforms_to`, even when its node name coincidentally
    /// equals the requested interface name+tag. Interface satisfaction
    /// is determined solely by `conforms_to`, never by node identity.
    #[test]
    fn from_any_interface_dep_rejects_producer_without_conforms_to() {
        let cons_instances = parse_instances(
            r#"[{
                instance_id: "cons1",
                bindings: { extra_cam: "depth_cam_inst_1" }
            }]"#,
        );
        let depends_on = parse_depends_on(
            r#"{
                nodes: [],
                interfaces: [{
                    name: "depth_camera",
                    tag: "v1",
                    link_id: "extra_cam",
                    from_any: true
                }]
            }"#,
        );
        let prod_instances = parse_instances(r#"[{ instance_id: "depth_cam_inst_1" }]"#);
        // Producer's node identity coincidentally matches the interface
        // name+tag, but it declares no `conforms_to`, so the element fails
        // the slot's per-element conformance check.
        let items = vec![
            item("cons", "v1", &cons_instances, Some(&depends_on)),
            item("depth_camera", "v1", &prod_instances, None),
        ];
        let out = validate_bindings(&items, TEST_CORE);
        assert_eq!(out.errors.len(), 1, "errors: {:?}", out.errors);
        let ParsingError::BindingInterfaceNotConformed(info) = &out.errors[0] else {
            panic!(
                "expected BindingInterfaceNotConformed, got {:?}",
                out.errors[0]
            );
        };
        assert_eq!(info.binding, "extra_cam");
        assert_eq!(info.target_instance_id, "depth_cam_inst_1");
        assert_eq!(info.producer_name, "depth_camera");
        assert_eq!(info.producer_tag, "v1");
        // The slot itself stays unbound (the only element errored).
        assert_eq!(
            slot_binding(&out, "cons1", "extra_cam"),
            Some(SlotBinding::FromAnyUnbound)
        );
    }

    /// `conforms_to` matching is strict on `(name, tag)`: a producer
    /// declaring a different tag for the same interface name is
    /// rejected.
    #[test]
    fn interface_dep_with_wrong_tag_in_conforms_to_is_rejected() {
        let cons_instances = parse_instances(
            r#"[{
                instance_id: "cons1",
                bindings: { depth: "webcam_inst_1" }
            }]"#,
        );
        let depends_on = parse_depends_on(
            r#"{
                nodes: [],
                interfaces: [{
                    name: "depth_camera",
                    tag: "v1",
                    link_id: "depth"
                }]
            }"#,
        );
        let prod_instances = parse_instances(r#"[{ instance_id: "webcam_inst_1" }]"#);
        let producer_conforms = parse_conforms_to(r#"[{ name: "depth_camera", tag: "v2" }]"#);
        let items = vec![
            item("cons", "v1", &cons_instances, Some(&depends_on)),
            item_with_conforms_to("webcam", "v1", &prod_instances, None, &producer_conforms),
        ];
        let out = validate_bindings(&items, TEST_CORE);
        assert_eq!(out.errors.len(), 1, "errors: {:?}", out.errors);
        let ParsingError::BindingInterfaceNotConformed(info) = &out.errors[0] else {
            panic!(
                "expected BindingInterfaceNotConformed, got {:?}",
                out.errors[0]
            );
        };
        assert_eq!(info.interface_tag, "v1");
        assert_eq!(info.producer_name, "webcam");
    }

    /// A producer declaring multiple `conforms_to` entries can satisfy
    /// any of them. Two consumers (each asking for a different
    /// interface) both successfully bind to the same producer.
    #[test]
    fn producer_with_multiple_conforms_to_can_satisfy() {
        let depth_consumer = parse_instances(
            r#"[{
                instance_id: "depth_cons",
                bindings: { feed: "multi_prod" }
            }]"#,
        );
        let depth_deps = parse_depends_on(
            r#"{
                nodes: [],
                interfaces: [{
                    name: "depth_camera",
                    tag: "v1",
                    link_id: "feed"
                }]
            }"#,
        );
        let uvc_consumer = parse_instances(
            r#"[{
                instance_id: "uvc_cons",
                bindings: { feed: "multi_prod" }
            }]"#,
        );
        let uvc_deps = parse_depends_on(
            r#"{
                nodes: [],
                interfaces: [{
                    name: "uvc_camera",
                    tag: "v1",
                    link_id: "feed"
                }]
            }"#,
        );
        let prod_instances = parse_instances(r#"[{ instance_id: "multi_prod" }]"#);
        let producer_conforms = parse_conforms_to(
            r#"[
                { name: "depth_camera", tag: "v1" },
                { name: "uvc_camera", tag: "v1" }
            ]"#,
        );
        let items = vec![
            item("depth_cons_node", "v1", &depth_consumer, Some(&depth_deps)),
            item("uvc_cons_node", "v1", &uvc_consumer, Some(&uvc_deps)),
            item_with_conforms_to(
                "multi_camera",
                "v1",
                &prod_instances,
                None,
                &producer_conforms,
            ),
        ];
        let out = validate_bindings(&items, TEST_CORE);
        assert!(out.errors.is_empty(), "unexpected errors: {:?}", out.errors);
        assert_eq!(
            slot_binding(&out, "depth_cons", "feed"),
            Some(SlotBinding::Pinned {
                producer: ProducerRef::new(TEST_CORE, "multi_prod")
            })
        );
        assert_eq!(
            slot_binding(&out, "uvc_cons", "feed"),
            Some(SlotBinding::Pinned {
                producer: ProducerRef::new(TEST_CORE, "multi_prod")
            })
        );
    }

    /// Binding is by link_id, never by type matching: a producer whose
    /// `conforms_to` satisfies BOTH from_any slots binds only to the
    /// slot whose link_id names it; the other stays deliberately
    /// unbound. (Under the retired free-form mechanism, the validator
    /// would have picked a slot by producer-type match — that shadowing
    /// is gone.)
    #[test]
    fn from_any_slots_bind_only_where_named() {
        let cons_instances = parse_instances(
            r#"[{
                instance_id: "cons1",
                bindings: { slot_b: "multi_prod" }
            }]"#,
        );
        let depends_on = parse_depends_on(
            r#"{
                nodes: [],
                interfaces: [
                    { name: "alpha_iface", tag: "v1", link_id: "slot_a", from_any: true },
                    { name: "beta_iface", tag: "v1", link_id: "slot_b", from_any: true }
                ]
            }"#,
        );
        let prod_instances = parse_instances(r#"[{ instance_id: "multi_prod" }]"#);
        let producer_conforms = parse_conforms_to(
            r#"[
                { name: "alpha_iface", tag: "v1" },
                { name: "beta_iface", tag: "v1" }
            ]"#,
        );
        let items = vec![
            item("cons", "v1", &cons_instances, Some(&depends_on)),
            item_with_conforms_to(
                "multi_iface_node",
                "v1",
                &prod_instances,
                None,
                &producer_conforms,
            ),
        ];
        let out = validate_bindings(&items, TEST_CORE);
        assert!(out.errors.is_empty(), "unexpected errors: {:?}", out.errors);
        assert_eq!(
            slot_binding(&out, "cons1", "slot_a"),
            Some(SlotBinding::FromAnyUnbound),
            "slot_a is not named by any binding and stays unbound \
             despite the producer conforming to its interface",
        );
        assert_eq!(
            slot_binding(&out, "cons1", "slot_b"),
            Some(SlotBinding::FromAnyBound {
                producers: vec![ProducerRef::new(TEST_CORE, "multi_prod")]
            })
        );
    }

    /// A `--bind` whose KEY names a pairing slot gets the targeted
    /// "use --pair" error, not `BindingDeadKey`. Pairing slots are
    /// established via `--pair`/`pairings:` only.
    #[test]
    fn binding_key_naming_a_pairing_slot_says_use_pair() {
        let cons_instances = parse_instances(
            r#"[{
                instance_id: "ctrl_1",
                bindings: { arm: "arm_inst" }
            }]"#,
        );
        let depends_on = parse_depends_on(
            r#"{
                pairings: [{ name: "arm_link", tag: "v1", role: "controller", link_id: "arm" }]
            }"#,
        );
        let prod_instances = parse_instances(r#"[{ instance_id: "arm_inst" }]"#);
        let items = vec![
            item("arm_controller", "v1", &cons_instances, Some(&depends_on)),
            item("robot_arm", "v1", &prod_instances, None),
        ];
        let out = validate_bindings(&items, TEST_CORE);
        assert_eq!(out.errors.len(), 1, "errors: {:?}", out.errors);
        let ParsingError::BindingKeyIsPairingSlot {
            owner_instance_id,
            binding,
        } = &out.errors[0]
        else {
            panic!("expected BindingKeyIsPairingSlot, got {:?}", out.errors[0]);
        };
        assert_eq!(owner_instance_id, "ctrl_1");
        assert_eq!(binding, "arm");
        assert!(
            out.errors[0].to_string().contains("--pair"),
            "message should point at --pair: {}",
            out.errors[0]
        );
    }

    /// Rule 1 must NOT fire for pairing slots: `collect_declared_slots`
    /// reads only nodes/interfaces, so a required pairing slot with no
    /// binding produces no `BindingMissingForPinnedDep`.
    #[test]
    fn pairing_slots_are_invisible_to_rule1() {
        let cons_instances = parse_instances(r#"[{ instance_id: "ctrl_1" }]"#);
        let depends_on = parse_depends_on(
            r#"{
                pairings: [{ name: "arm_link", tag: "v1", role: "controller", link_id: "arm" }]
            }"#,
        );
        let items = vec![item(
            "arm_controller",
            "v1",
            &cons_instances,
            Some(&depends_on),
        )];
        let out = validate_bindings(&items, TEST_CORE);
        assert!(out.errors.is_empty(), "unexpected errors: {:?}", out.errors);
        assert!(out.slot_bindings.is_empty());
    }

    /// Stamping: every producer reference the validator emits (pinned
    /// and from_any-bound alike) carries exactly the
    /// `producer_core_node` passed by the caller (the launching
    /// daemon). This is the single point where the instance-only
    /// `--bind` syntax becomes a wire-complete address.
    #[test]
    fn every_resolved_binding_is_stamped_with_the_launching_core_node() {
        let cons_instances = parse_instances(
            r#"[{
                instance_id: "cons1",
                bindings: { main: "prod1", extra: "prod2" }
            }]"#,
        );
        let depends_on = parse_depends_on(
            r#"{
                nodes: [
                    { name: "camera", tag: "v1", link_id: "main" },
                    { name: "camera", tag: "v1", link_id: "extra", from_any: true }
                ]
            }"#,
        );
        let prod_instances = parse_instances(
            r#"[
                { instance_id: "prod1" },
                { instance_id: "prod2" }
            ]"#,
        );
        let items = vec![
            item("cons", "v1", &cons_instances, Some(&depends_on)),
            item("camera", "v1", &prod_instances, None),
        ];
        let out = validate_bindings(&items, "daemon_west");
        assert!(out.errors.is_empty(), "unexpected errors: {:?}", out.errors);
        let resolved = out.slot_bindings.get("cons1").expect("cons1 bindings");
        let mut producer_count = 0;
        for binding in resolved.values() {
            match binding {
                SlotBinding::Pinned { producer } => {
                    producer_count += 1;
                    assert_eq!(producer.core_node, "daemon_west");
                }
                SlotBinding::FromAnyBound { producers } => {
                    for producer in producers {
                        producer_count += 1;
                        assert_eq!(producer.core_node, "daemon_west");
                    }
                }
                SlotBinding::FromAnyUnbound => {}
            }
        }
        assert_eq!(
            producer_count, 2,
            "both the pinned and the from_any producer must be stamped"
        );
    }
}
