//! Plan-phase validation for the launcher's per-instance `bindings`
//! field. Runs after node configs are loaded so the validator can
//! cross-reference each consumer's `depends_on` against the running
//! stack snapshot's `instance_id → (name, tag)` lookup.
//!
//! Every binding entry maps a declared slot to its application-selected
//! producer set: the KEY must equal a `depends_on.{nodes,contracts}`
//! `link_id`, the value's shape must mirror the slot's declared
//! cardinality (a scalar for `one`, an array for `one_or_more` /
//! `zero_or_more`; repeated `--bind` flags are checked by count instead),
//! and every target must deploy the slot's node (node slots) or implement
//! the slot's contract (contract slots); conformance runs per bound
//! instance. Every declared slot must resolve: `one` to exactly one
//! producer, `one_or_more` to at least one, and `zero_or_more` to zero or
//! more (an omitted binding and an empty array are both its valid empty
//! set). There is no wildcard fallback and no free-form key.
//!
//! The validator emits both errors and the resolved per-slot producer
//! set per consumer instance, which the caller serializes into
//! [`config::runtime::NodeInstanceConfig::slot_bindings`].

use crate::error::{
    BindingContractNotImplemented, BindingSlotUnfulfilled, BindingTargetMismatch,
    DuplicateInstanceIdAcrossStack, ParsingError, SlotKind,
};
use config::node::{Cardinality, DependsOn, ImplementsEntry};
use config::runtime::{BoundProducers, ProducerRef, SlotBindings};
use std::collections::BTreeMap;

use super::types::{DeploymentInstance, LinkValue};

/// Minimal view of one planned deployment needed for binding
/// validation. Built by the launcher with borrowed references to avoid
/// cloning the full planned-deployment graph; consumed by
/// [`validate_bindings`].
pub struct BindingValidationItem<'a> {
    pub node_name: &'a str,
    pub node_tag: &'a str,
    pub instances: &'a [DeploymentInstance],
    pub depends_on: Option<&'a DependsOn>,
    /// Producer's `manifest.implements` list, borrowed as a slice.
    /// Empty when the node implements no contract. Used by the validator
    /// to decide whether this node can satisfy a consumer's contract
    /// slot.
    pub implements: &'a [ImplementsEntry],
}

/// Per-slot metadata extracted from `depends_on` during validation.
/// Carrying `kind` and `cardinality` inline lets the shape and
/// target-matching paths pick the right error without re-scanning
/// `depends_on` per binding.
#[derive(Clone, Copy)]
struct SlotMeta<'a> {
    name: &'a str,
    tag: &'a str,
    kind: SlotKind,
    cardinality: Cardinality,
}

/// Outcome of [`validate_bindings`]. `errors` aggregates every validator
/// rule violation; `slot_bindings` carries the resolved per-slot view
/// for every consumer instance whose bindings parsed cleanly. The caller
/// must check `errors.is_empty()` before consuming the resolution.
#[derive(Debug, Default)]
pub struct ValidatedBindings {
    pub errors: Vec<ParsingError>,
    /// `consumer_instance_id → link_id → ordered bound producer set`.
    /// When `errors` is empty, every declared slot of an instance
    /// appears, sized per its cardinality; a `zero_or_more` slot with no
    /// binding appears with an explicit empty set so the resolution is
    /// self-describing.
    pub slot_bindings: BTreeMap<String, SlotBindings>,
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
/// This validator owns only the producer-binding slots
/// (`depends_on.{nodes,contracts}`). A `links` key naming a pairing/observer
/// slot, or naming no slot at all, is skipped here; pairing/observer
/// resolution and the unified unknown-key report (`validate_link_slots`) own
/// those.
///
/// Rules enforced:
/// 1. Only `links` keys equal to a declared `depends_on.{nodes,contracts}`
///    `link_id` are processed; every other key is skipped.
/// 2. The binding value matches the slot's declared cardinality. Launch
///    files carry shape: a `one` slot takes a scalar only (an array,
///    single-element and empty included, is
///    [`ParsingError::BindingArrayOnOneSlot`]), a multi slot takes an
///    array only (a scalar is
///    [`ParsingError::BindingScalarOnMultiSlot`]), and an empty array
///    meets only `zero_or_more`
///    ([`ParsingError::BindingCardinalityUnmet`] on `one_or_more`).
///    CLI flag occurrences carry no shape and are checked by count:
///    more than one on a `one` slot is
///    [`ParsingError::BindingSingleSlotMultipleTargets`].
/// 3. Every target in the slot's set exists in the snapshot
///    ([`ParsingError::UnknownInstanceId`] otherwise) and satisfies the
///    slot, checked per bound instance: node slots match the target's
///    `(name, tag)` identity ([`ParsingError::BindingTargetMismatch`]
///    otherwise), contract slots match the target's
///    `manifest.implements`
///    ([`ParsingError::BindingContractNotImplemented`] otherwise).
///    Duplicate targets within one slot are unrepresentable
///    ([`super::types::LinkTargets`] rejects them at construction);
///    declaration order is preserved into the resolution.
/// 4. Stack-wide `instance_id` uniqueness across every entry in
///    `items.instances` is enforced; collisions emit
///    [`ParsingError::DuplicateInstanceIdAcrossStack`].
/// 5. Every declared slot resolves. A `one` / `one_or_more` slot with no
///    binding entry emits one [`ParsingError::BindingSlotUnfulfilled`]
///    per slot (in link_id order); a `zero_or_more` slot with no entry
///    resolves to an explicit empty set.
pub fn validate_bindings(
    items: &[BindingValidationItem<'_>],
    producer_core_node: &str,
) -> ValidatedBindings {
    let mut out = ValidatedBindings::default();

    check_stack_wide_instance_id_uniqueness(items, &mut out.errors);

    let instance_to_item = build_instance_lookup(items);

    for item in items {
        let declared_slots = collect_declared_slots(item.depends_on);

        for instance in item.instances {
            let mut resolved: SlotBindings = BTreeMap::new();

            for (binding_key, value) in &instance.links {
                // Only producer-binding keys are ours. A key naming a
                // pairing/observer slot is handled by its own validator, and a
                // key naming no slot at all is reported once by
                // `validate_link_slots`; both are skipped here rather than
                // re-reported per mechanism.
                let Some(slot) = declared_slots.get(binding_key.as_str()).copied() else {
                    continue;
                };

                // Rule 2: the value's shape (or flag count) must match the
                // slot's declared cardinality.
                if let Err(shape_error) =
                    check_value_matches_cardinality(&slot, value, binding_key, instance)
                {
                    out.errors.push(shape_error);
                    continue;
                }

                // Rule 3: every target exists and satisfies the slot
                // (uniqueness holds by `LinkTargets` construction).
                // All-or-nothing: a slot with any bad target reports each
                // offender and resolves nothing.
                let mut producers: Vec<ProducerRef> = Vec::with_capacity(value.targets().len());
                let mut slot_failed = false;
                for target_id in value.targets() {
                    let Some(target_item) = instance_to_item.get(target_id.as_str()) else {
                        out.errors.push(ParsingError::UnknownInstanceId {
                            owner_instance_id: instance.instance_id.to_string(),
                            link: binding_key.clone(),
                            instance_id: target_id.clone(),
                        });
                        slot_failed = true;
                        continue;
                    };
                    if !slot_matches_producer(&slot, target_item) {
                        out.errors.push(target_conformance_error(
                            &slot,
                            target_item,
                            binding_key,
                            target_id,
                            instance,
                        ));
                        slot_failed = true;
                        continue;
                    }
                    producers.push(ProducerRef::new(producer_core_node, target_id.clone()));
                }
                if slot_failed {
                    continue;
                }

                let bound = BoundProducers::try_from(producers)
                    .expect("targets are duplicate-free by LinkTargets construction");
                resolved.insert(binding_key.clone(), bound);
            }

            // Rule 5: every declared slot must resolve. Keyed on the
            // binding entries, not `resolved`, so a slot whose entry
            // failed shape or target validation reports only that error,
            // not a bogus "add a binding" too. A `zero_or_more` slot with
            // no entry resolves to an explicit empty set.
            for (slot_link_id, slot) in &declared_slots {
                if instance.links.contains_key(*slot_link_id) {
                    continue;
                }
                if slot.cardinality.allows_empty() {
                    resolved.insert((*slot_link_id).to_string(), BoundProducers::default());
                } else {
                    out.errors
                        .push(ParsingError::BindingSlotUnfulfilled(Box::new(
                            BindingSlotUnfulfilled {
                                owner_instance_id: instance.instance_id.to_string(),
                                link_id: (*slot_link_id).to_string(),
                                slot_kind: slot.kind,
                                slot_name: slot.name.to_string(),
                                slot_tag: slot.tag.to_string(),
                                cardinality: slot.cardinality,
                            },
                        )));
                }
            }

            if !resolved.is_empty() {
                out.slot_bindings
                    .insert(instance.instance_id.to_string(), resolved);
            }
        }
    }

    out
}

/// Rule 2: does the binding value's shape (launch file) or occurrence
/// count (CLI flags) satisfy the slot's declared cardinality? Launch-file
/// shapes are strict (a scalar is only valid on a `one` slot and an array
/// only on a multi slot), while flag occurrences carry no shape and are
/// checked by count alone. CLI-built `Flags` is non-empty (zero occurrences
/// is an omitted binding, handled by rule 5), but the validator still rejects
/// an empty programmatic value on `one_or_more`.
fn check_value_matches_cardinality(
    slot: &SlotMeta<'_>,
    value: &LinkValue,
    binding_key: &str,
    instance: &DeploymentInstance,
) -> Result<(), ParsingError> {
    match (slot.cardinality, value) {
        (Cardinality::One, LinkValue::Scalar(_)) => Ok(()),
        (Cardinality::One, LinkValue::Array(_)) => Err(ParsingError::BindingArrayOnOneSlot {
            owner_instance_id: instance.instance_id.to_string(),
            binding: binding_key.to_string(),
        }),
        (Cardinality::One, LinkValue::Flags(targets)) => {
            if targets.len() == 1 {
                Ok(())
            } else {
                Err(ParsingError::BindingSingleSlotMultipleTargets {
                    owner_instance_id: instance.instance_id.to_string(),
                    binding: binding_key.to_string(),
                    target_count: targets.len(),
                })
            }
        }
        (Cardinality::OneOrMore | Cardinality::ZeroOrMore, LinkValue::Scalar(_)) => {
            Err(ParsingError::BindingScalarOnMultiSlot {
                owner_instance_id: instance.instance_id.to_string(),
                binding: binding_key.to_string(),
                cardinality: slot.cardinality,
            })
        }
        (Cardinality::OneOrMore, LinkValue::Array(targets) | LinkValue::Flags(targets))
            if targets.is_empty() =>
        {
            Err(ParsingError::BindingCardinalityUnmet {
                owner_instance_id: instance.instance_id.to_string(),
                binding: binding_key.to_string(),
            })
        }
        (
            Cardinality::OneOrMore | Cardinality::ZeroOrMore,
            LinkValue::Array(_) | LinkValue::Flags(_),
        ) => Ok(()),
    }
}

/// Rule 3 conformance error for one bound target, picked by slot kind:
/// node slots report an identity mismatch, contract slots a missing
/// `manifest.implements` claim.
fn target_conformance_error(
    slot: &SlotMeta<'_>,
    target_item: &BindingValidationItem<'_>,
    binding_key: &str,
    target_id: &str,
    instance: &DeploymentInstance,
) -> ParsingError {
    match slot.kind {
        SlotKind::Node => ParsingError::BindingTargetMismatch(Box::new(BindingTargetMismatch {
            owner_instance_id: instance.instance_id.to_string(),
            binding: binding_key.to_string(),
            target_instance_id: target_id.to_string(),
            expected_name: slot.name.to_string(),
            expected_tag: slot.tag.to_string(),
            actual_name: target_item.node_name.to_string(),
            actual_tag: target_item.node_tag.to_string(),
        })),
        SlotKind::Contract => {
            ParsingError::BindingContractNotImplemented(Box::new(BindingContractNotImplemented {
                owner_instance_id: instance.instance_id.to_string(),
                binding: binding_key.to_string(),
                target_instance_id: target_id.to_string(),
                contract_name: slot.name.to_string(),
                contract_tag: slot.tag.to_string(),
                producer_name: target_item.node_name.to_string(),
                producer_tag: target_item.node_tag.to_string(),
            }))
        }
    }
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

/// Stack-wide `instance_id` uniqueness (rule 4). Two entries anywhere
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

/// The declared binding slots: every `depends_on.{nodes,contracts}`
/// entry keyed by `link_id`, with the dep's `(name, tag, kind,
/// cardinality)` so the shape and target-matching paths can branch
/// without re-scanning the manifest. Cardinality applies uniformly to
/// both entry kinds.
fn collect_declared_slots(depends_on: Option<&DependsOn>) -> DeclaredSlots<'_> {
    let mut slots = BTreeMap::new();
    if let Some(deps) = depends_on {
        for dep in &deps.nodes {
            slots.insert(
                dep.link_id.as_str(),
                SlotMeta {
                    name: dep.name.as_str(),
                    tag: dep.tag.as_str(),
                    kind: SlotKind::Node,
                    cardinality: dep.cardinality,
                },
            );
        }
        for dep in &deps.contracts {
            slots.insert(
                dep.link_id.as_str(),
                SlotMeta {
                    name: dep.name.as_str(),
                    tag: dep.tag.as_str(),
                    kind: SlotKind::Contract,
                    cardinality: dep.cardinality,
                },
            );
        }
    }
    slots
}

/// Does a producer satisfy a declared slot? Node slots match by
/// `(name, tag)` identity; contract slots match against the producer's
/// `manifest.implements`. sha256 is not cross-checked here; each side
/// independently verifies its own declared sha256 against the on-disk
/// contract document at cache resolution time.
fn slot_matches_producer(slot: &SlotMeta<'_>, producer: &BindingValidationItem<'_>) -> bool {
    match slot.kind {
        SlotKind::Node => producer.node_name == slot.name && producer.node_tag == slot.tag,
        SlotKind::Contract => producer
            .implements
            .iter()
            .any(|item| item.name.as_str() == slot.name && item.tag.as_str() == slot.tag),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use config::runtime::Name;

    /// The launching daemon's core_node stamped into every resolved
    /// binding by these tests.
    const TEST_CORE: &str = "core_a";

    fn parse_instances(json5: &str) -> Vec<DeploymentInstance> {
        serde_json5::from_str(json5).expect("instances fixture should parse")
    }

    fn parse_depends_on(json5: &str) -> DependsOn {
        serde_json5::from_str(json5).expect("depends_on fixture should parse")
    }

    fn parse_implements(json5: &str) -> Vec<ImplementsEntry> {
        serde_json5::from_str(json5).expect("implements fixture should parse")
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
            implements: &[],
        }
    }

    /// Like `item` but also threads an `implements` slice, for tests
    /// that exercise contract-implementation matching.
    fn item_with_implements<'a>(
        node_name: &'a str,
        node_tag: &'a str,
        instances: &'a [DeploymentInstance],
        depends_on: Option<&'a DependsOn>,
        implements: &'a [ImplementsEntry],
    ) -> BindingValidationItem<'a> {
        BindingValidationItem {
            node_name,
            node_tag,
            instances,
            depends_on,
            implements,
        }
    }

    /// The resolved producer set for one slot, as a plain `Vec` for
    /// assertion ergonomics. `None` when the instance or slot resolved
    /// nothing at all.
    fn slot_binding(
        out: &ValidatedBindings,
        instance: &str,
        link_id: &str,
    ) -> Option<Vec<ProducerRef>> {
        out.slot_bindings
            .get(instance)
            .and_then(|m| m.get(link_id))
            .map(|bound| bound.iter().cloned().collect())
    }

    /// Shorthand for the common single-producer expectation.
    fn single(core_node: &str, instance_id: &str) -> Option<Vec<ProducerRef>> {
        Some(vec![ProducerRef::new(core_node, instance_id)])
    }

    /// Test shorthand: a `Flags` binding value from unique literals, as
    /// the CLI's flag accumulation would build it.
    fn flags(targets: &[&str]) -> LinkValue {
        LinkValue::Flags(
            super::super::types::LinkTargets::new(
                targets.iter().map(|t| t.to_string()).collect(),
            )
            .expect("test targets are unique"),
        )
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

    /// A link whose KEY names no producer slot is silently skipped by the
    /// binding validator (unknown-key reporting is `validate_link_slots`'s
    /// job); a valid sibling binding still resolves.
    #[test]
    fn unknown_key_is_skipped_here_not_reported() {
        let instances = parse_instances(
            r#"[{
                instance_id: "cons1",
                links: { main: "prod1", stale_slot: "prod1" }
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
        assert!(out.errors.is_empty(), "unexpected errors: {:?}", out.errors);
        assert_eq!(
            slot_binding(&out, "cons1", "main"),
            single(TEST_CORE, "prod1")
        );
    }

    /// Rule 5: a declared slot with no binding entry is rejected — there
    /// is no unbound state.
    #[test]
    fn unbound_slot_is_rejected() {
        let instances = parse_instances(r#"[{ instance_id: "cons1" }]"#);
        let depends_on = parse_depends_on(
            r#"{
                nodes: [{ name: "camera", tag: "v1", link_id: "main" }]
            }"#,
        );
        let items = vec![item("cons", "v1", &instances, Some(&depends_on))];
        let out = validate_bindings(&items, TEST_CORE);
        assert_eq!(
            out.errors.len(),
            1,
            "expected one error, got {:?}",
            out.errors
        );
        let ParsingError::BindingSlotUnfulfilled(info) = &out.errors[0] else {
            panic!("expected BindingSlotUnfulfilled, got {:?}", out.errors[0]);
        };
        assert_eq!(info.owner_instance_id, "cons1");
        assert_eq!(info.link_id, "main");
        assert_eq!(info.slot_kind, SlotKind::Node);
        assert_eq!(info.slot_name, "camera");
        assert_eq!(info.slot_tag, "v1");
        assert!(
            !out.slot_bindings.contains_key("cons1"),
            "an instance with no resolvable bindings must not appear in the resolution"
        );
    }

    /// Rule 5: every unfulfilled slot on an instance gets its own error,
    /// in link_id order, and a slot WITH a binding entry never
    /// double-reports. `cons1` declares three slots and binds only
    /// `middle`.
    #[test]
    fn rule5_reports_each_unfulfilled_slot_in_link_id_order() {
        let cons_instances = parse_instances(
            r#"[{
                instance_id: "cons1",
                links: { middle: "prod1" }
            }]"#,
        );
        let depends_on = parse_depends_on(
            r#"{
                nodes: [
                    { name: "camera", tag: "v1", link_id: "zeta" },
                    { name: "camera", tag: "v1", link_id: "middle" },
                    { name: "camera", tag: "v1", link_id: "alpha" }
                ]
            }"#,
        );
        let prod_instances = parse_instances(r#"[{ instance_id: "prod1" }]"#);
        let items = vec![
            item("cons", "v1", &cons_instances, Some(&depends_on)),
            item("camera", "v1", &prod_instances, None),
        ];
        let out = validate_bindings(&items, TEST_CORE);
        let unfulfilled: Vec<&str> = out
            .errors
            .iter()
            .map(|e| match e {
                ParsingError::BindingSlotUnfulfilled(info) => info.link_id.as_str(),
                other => panic!("expected only BindingSlotUnfulfilled, got {other:?}"),
            })
            .collect();
        assert_eq!(
            unfulfilled,
            ["alpha", "zeta"],
            "one error per unfulfilled slot, in link_id order; bound `middle` must not report"
        );
        // The bound slot still resolves (errors gate consumption at the
        // caller, but the resolution itself is complete for bound slots).
        assert_eq!(
            slot_binding(&out, "cons1", "middle"),
            single(TEST_CORE, "prod1")
        );
    }

    /// Rule 2 (happy path): a single-target binding resolves the slot to
    /// that producer.
    #[test]
    fn rule2_single_target_binding_resolves() {
        let cons_instances = parse_instances(
            r#"[{
                instance_id: "cons1",
                links: { main: "prod1" }
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
            single(TEST_CORE, "prod1")
        );
    }

    /// Depends-on fixture with one slot per cardinality, all expecting
    /// `camera:v1` nodes: `main` (one, omitted), `cameras` (one_or_more),
    /// `spare_cameras` (zero_or_more).
    fn all_cardinalities_depends_on() -> DependsOn {
        parse_depends_on(
            r#"{
                nodes: [
                    { name: "camera", tag: "v1", link_id: "main" },
                    { name: "camera", tag: "v1", link_id: "cameras", cardinality: "one_or_more" },
                    { name: "camera", tag: "v1", link_id: "spare_cameras", cardinality: "zero_or_more" }
                ]
            }"#,
        )
    }

    /// Happy path across all three cardinalities: a scalar on the `one`
    /// slot, an array on the `one_or_more` slot (resolved in declaration
    /// order), and an omitted `zero_or_more` slot resolving to an explicit
    /// empty set.
    #[test]
    fn cardinalities_resolve_scalar_array_and_omitted_empty_set() {
        let cons_instances = parse_instances(
            r#"[{
                instance_id: "cons1",
                links: {
                    main: "prod1",
                    cameras: ["prod2", "prod1"]
                }
            }]"#,
        );
        let depends_on = all_cardinalities_depends_on();
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
            slot_binding(&out, "cons1", "main"),
            single(TEST_CORE, "prod1")
        );
        assert_eq!(
            slot_binding(&out, "cons1", "cameras"),
            Some(vec![
                ProducerRef::new(TEST_CORE, "prod2"),
                ProducerRef::new(TEST_CORE, "prod1"),
            ]),
            "array declaration order must be preserved, not sorted"
        );
        assert_eq!(
            slot_binding(&out, "cons1", "spare_cameras"),
            Some(Vec::new()),
            "an omitted zero_or_more binding resolves to an explicit empty set"
        );
    }

    /// A `zero_or_more` slot bound to an explicit empty array is the same
    /// valid empty set as omitting the binding entirely.
    #[test]
    fn zero_or_more_empty_array_is_a_valid_empty_set() {
        let cons_instances = parse_instances(
            r#"[{
                instance_id: "cons1",
                links: {
                    main: "prod1",
                    cameras: ["prod1"],
                    spare_cameras: []
                }
            }]"#,
        );
        let depends_on = all_cardinalities_depends_on();
        let prod_instances = parse_instances(r#"[{ instance_id: "prod1" }]"#);
        let items = vec![
            item("cons", "v1", &cons_instances, Some(&depends_on)),
            item("camera", "v1", &prod_instances, None),
        ];
        let out = validate_bindings(&items, TEST_CORE);
        assert!(out.errors.is_empty(), "unexpected errors: {:?}", out.errors);
        assert_eq!(
            slot_binding(&out, "cons1", "spare_cameras"),
            Some(Vec::new())
        );
        assert_eq!(
            slot_binding(&out, "cons1", "cameras"),
            single(TEST_CORE, "prod1"),
            "a single-element array is valid on a multi slot"
        );
    }

    /// Rule 2: an array on a `one` slot is rejected, single-element and
    /// empty arrays included; the value shape must mirror the cardinality.
    #[test]
    fn rule2_array_on_a_one_slot_is_rejected() {
        for producers in [r#"["prod1", "prod2"]"#, r#"["prod1"]"#, r#"[]"#] {
            let cons_json = format!(
                r#"[{{
                    instance_id: "cons1",
                    links: {{ main: {producers} }}
                }}]"#
            );
            let cons_instances = parse_instances(&cons_json);
            let depends_on =
                parse_depends_on(r#"{ nodes: [{ name: "camera", tag: "v1", link_id: "main" }] }"#);
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
            assert_eq!(out.errors.len(), 1, "value {producers}: {:?}", out.errors);
            let ParsingError::BindingArrayOnOneSlot {
                owner_instance_id,
                binding,
            } = &out.errors[0]
            else {
                panic!(
                    "expected BindingArrayOnOneSlot for {producers}, got {:?}",
                    out.errors[0]
                );
            };
            assert_eq!(owner_instance_id, "cons1");
            assert_eq!(binding, "main");
            assert!(
                !out.slot_bindings.contains_key("cons1"),
                "a shape-rejected slot must not resolve"
            );
        }
    }

    /// Rule 2: a scalar on a multi slot is rejected for both multi
    /// cardinalities; the error names the slot's cardinality.
    #[test]
    fn rule2_scalar_on_a_multi_slot_is_rejected() {
        // Per case: the slot under test bound as a scalar, the other two
        // slots validly bound.
        for (link_id, cardinality, cons_json) in [
            (
                "cameras",
                Cardinality::OneOrMore,
                r#"[{
                    instance_id: "cons1",
                    links: { main: "prod1", cameras: "prod1", spare_cameras: [] }
                }]"#,
            ),
            (
                "spare_cameras",
                Cardinality::ZeroOrMore,
                r#"[{
                    instance_id: "cons1",
                    links: { main: "prod1", cameras: ["prod1"], spare_cameras: "prod1" }
                }]"#,
            ),
        ] {
            let cons_instances = parse_instances(cons_json);
            let depends_on = all_cardinalities_depends_on();
            let prod_instances = parse_instances(r#"[{ instance_id: "prod1" }]"#);
            let items = vec![
                item("cons", "v1", &cons_instances, Some(&depends_on)),
                item("camera", "v1", &prod_instances, None),
            ];
            let out = validate_bindings(&items, TEST_CORE);
            assert_eq!(out.errors.len(), 1, "slot {link_id}: {:?}", out.errors);
            let ParsingError::BindingScalarOnMultiSlot {
                owner_instance_id,
                binding,
                cardinality: actual_cardinality,
            } = &out.errors[0]
            else {
                panic!(
                    "expected BindingScalarOnMultiSlot for {link_id}, got {:?}",
                    out.errors[0]
                );
            };
            assert_eq!(owner_instance_id, "cons1");
            assert_eq!(binding, link_id);
            assert_eq!(*actual_cardinality, cardinality);
        }
    }

    /// Rule 2: an empty array on a `one_or_more` slot is a binding entry
    /// whose set misses the minimum of one producer.
    #[test]
    fn rule2_empty_array_on_one_or_more_is_cardinality_unmet() {
        let cons_instances = parse_instances(
            r#"[{
                instance_id: "cons1",
                links: { main: "prod1", cameras: [], spare_cameras: [] }
            }]"#,
        );
        let depends_on = all_cardinalities_depends_on();
        let prod_instances = parse_instances(r#"[{ instance_id: "prod1" }]"#);
        let items = vec![
            item("cons", "v1", &cons_instances, Some(&depends_on)),
            item("camera", "v1", &prod_instances, None),
        ];
        let out = validate_bindings(&items, TEST_CORE);
        assert_eq!(out.errors.len(), 1, "errors: {:?}", out.errors);
        let ParsingError::BindingCardinalityUnmet {
            owner_instance_id,
            binding,
        } = &out.errors[0]
        else {
            panic!("expected BindingCardinalityUnmet, got {:?}", out.errors[0]);
        };
        assert_eq!(owner_instance_id, "cons1");
        assert_eq!(binding, "cameras");
    }

    /// An omitted binding is an error for BOTH `one` and `one_or_more`
    /// (the cardinality table's "omitted binding" column), each reported
    /// with the slot's cardinality; only `zero_or_more` tolerates it.
    #[test]
    fn omitted_bindings_error_per_cardinality() {
        let cons_instances = parse_instances(r#"[{ instance_id: "cons1" }]"#);
        let depends_on = all_cardinalities_depends_on();
        let items = vec![item("cons", "v1", &cons_instances, Some(&depends_on))];
        let out = validate_bindings(&items, TEST_CORE);
        let unfulfilled: Vec<(&str, Cardinality)> = out
            .errors
            .iter()
            .map(|e| match e {
                ParsingError::BindingSlotUnfulfilled(info) => {
                    (info.link_id.as_str(), info.cardinality)
                }
                other => panic!("expected only BindingSlotUnfulfilled, got {other:?}"),
            })
            .collect();
        assert_eq!(
            unfulfilled,
            [
                ("cameras", Cardinality::OneOrMore),
                ("main", Cardinality::One)
            ],
            "one error per non-zero_or_more slot, in link_id order"
        );
        assert_eq!(
            slot_binding(&out, "cons1", "spare_cameras"),
            Some(Vec::new()),
            "the zero_or_more slot still resolves to its empty set"
        );
    }

    /// CLI flag occurrences (shape-less `LinkValue::Flags`) accumulate
    /// on a multi slot in flag order and stay a hard error on a `one`
    /// slot; a single occurrence is valid everywhere it meets the minimum.
    #[test]
    fn flag_occurrences_check_count_not_shape() {
        let depends_on = all_cardinalities_depends_on();
        let prod_instances = parse_instances(
            r#"[
                { instance_id: "prod1" },
                { instance_id: "prod2" }
            ]"#,
        );

        // Accumulated flags on the multi slot, one flag on the one slot.
        let mut valid = DeploymentInstance::empty(Name::new("cons1").unwrap());
        valid.links.insert("main".to_string(), flags(&["prod1"]));
        valid
            .links
            .insert("cameras".to_string(), flags(&["prod2", "prod1"]));
        let valid_instances = vec![valid];
        let items = vec![
            item("cons", "v1", &valid_instances, Some(&depends_on)),
            item("camera", "v1", &prod_instances, None),
        ];
        let out = validate_bindings(&items, TEST_CORE);
        assert!(out.errors.is_empty(), "unexpected errors: {:?}", out.errors);
        assert_eq!(
            slot_binding(&out, "cons1", "main"),
            single(TEST_CORE, "prod1")
        );
        assert_eq!(
            slot_binding(&out, "cons1", "cameras"),
            Some(vec![
                ProducerRef::new(TEST_CORE, "prod2"),
                ProducerRef::new(TEST_CORE, "prod1"),
            ]),
            "flag occurrence order must be preserved"
        );

        // Two flags on the `one` slot: hard error naming the count.
        let mut repeated = DeploymentInstance::empty(Name::new("cons1").unwrap());
        repeated
            .links
            .insert("main".to_string(), flags(&["prod1", "prod2"]));
        repeated
            .links
            .insert("cameras".to_string(), flags(&["prod1"]));
        let repeated_instances = vec![repeated];
        let items = vec![
            item("cons", "v1", &repeated_instances, Some(&depends_on)),
            item("camera", "v1", &prod_instances, None),
        ];
        let out = validate_bindings(&items, TEST_CORE);
        assert_eq!(out.errors.len(), 1, "errors: {:?}", out.errors);
        let ParsingError::BindingSingleSlotMultipleTargets {
            owner_instance_id,
            binding,
            target_count,
        } = &out.errors[0]
        else {
            panic!(
                "expected BindingSingleSlotMultipleTargets, got {:?}",
                out.errors[0]
            );
        };
        assert_eq!(owner_instance_id, "cons1");
        assert_eq!(binding, "main");
        assert_eq!(*target_count, 2);

        // The CLI never constructs an empty Flags value, but programmatic
        // callers still go through the same cardinality check.
        let mut empty = DeploymentInstance::empty(Name::new("cons1").unwrap());
        empty.links.insert("main".to_string(), flags(&["prod1"]));
        empty.links.insert("cameras".to_string(), flags(&[]));
        let empty_instances = vec![empty];
        let items = vec![
            item("cons", "v1", &empty_instances, Some(&depends_on)),
            item("camera", "v1", &prod_instances, None),
        ];
        let out = validate_bindings(&items, TEST_CORE);
        assert!(matches!(
            out.errors.as_slice(),
            [ParsingError::BindingCardinalityUnmet { binding, .. }] if binding == "cameras"
        ));
    }

    /// Rule 3 runs per bound instance on a multi slot: one bad target
    /// among good ones reports exactly that target, and the slot resolves
    /// nothing (all-or-nothing) while other slots still resolve.
    #[test]
    fn rule3_checks_each_target_of_a_multi_slot() {
        let cons_instances = parse_instances(
            r#"[{
                instance_id: "cons1",
                links: {
                    main: "prod1",
                    cameras: ["prod1", "actually_lidar", "ghost"]
                }
            }]"#,
        );
        let depends_on = all_cardinalities_depends_on();
        let prod_instances = parse_instances(r#"[{ instance_id: "prod1" }]"#);
        let lidar_instances = parse_instances(r#"[{ instance_id: "actually_lidar" }]"#);
        let items = vec![
            item("cons", "v1", &cons_instances, Some(&depends_on)),
            item("camera", "v1", &prod_instances, None),
            item("lidar", "v1", &lidar_instances, None),
        ];
        let out = validate_bindings(&items, TEST_CORE);
        assert_eq!(out.errors.len(), 2, "errors: {:?}", out.errors);
        let ParsingError::BindingTargetMismatch(mismatch) = &out.errors[0] else {
            panic!(
                "expected BindingTargetMismatch first, got {:?}",
                out.errors[0]
            );
        };
        assert_eq!(mismatch.target_instance_id, "actually_lidar");
        assert_eq!(mismatch.actual_name, "lidar");
        let ParsingError::UnknownInstanceId { instance_id, .. } = &out.errors[1] else {
            panic!("expected UnknownInstanceId second, got {:?}", out.errors[1]);
        };
        assert_eq!(instance_id, "ghost");
        let resolved = out.slot_bindings.get("cons1").expect("cons1 resolution");
        assert!(
            !resolved.contains_key("cameras"),
            "a slot with any bad target must resolve nothing"
        );
        assert!(
            resolved.contains_key("main"),
            "unrelated slots still resolve"
        );
    }

    /// Contract-slot conformance also runs per bound instance: every
    /// member of a multi slot must implement the contract.
    #[test]
    fn contract_multi_slot_checks_implements_per_target() {
        let cons_instances = parse_instances(
            r#"[{
                instance_id: "cons1",
                links: { cameras: ["webcam_1", "not_a_camera"] }
            }]"#,
        );
        let depends_on = parse_depends_on(
            r#"{
                contracts: [{
                    name: "uvc_camera",
                    tag: "v1",
                    link_id: "cameras",
                    cardinality: "one_or_more"
                }]
            }"#,
        );
        let webcam_instances = parse_instances(r#"[{ instance_id: "webcam_1" }]"#);
        let webcam_implements =
            parse_implements(r#"[{ name: "uvc_camera", tag: "v1", link_id: "cam" }]"#);
        let other_instances = parse_instances(r#"[{ instance_id: "not_a_camera" }]"#);
        let items = vec![
            item("cons", "v1", &cons_instances, Some(&depends_on)),
            item_with_implements("webcam", "v1", &webcam_instances, None, &webcam_implements),
            item("other", "v1", &other_instances, None),
        ];
        let out = validate_bindings(&items, TEST_CORE);
        assert_eq!(out.errors.len(), 1, "errors: {:?}", out.errors);
        let ParsingError::BindingContractNotImplemented(info) = &out.errors[0] else {
            panic!(
                "expected BindingContractNotImplemented, got {:?}",
                out.errors[0]
            );
        };
        assert_eq!(info.target_instance_id, "not_a_camera");
        assert_eq!(info.contract_name, "uvc_camera");
    }

    /// Rule 5: pinned binding whose target deploys the wrong node.
    #[test]
    fn rejects_target_node_mismatch() {
        let cons_instances = parse_instances(
            r#"[{
                instance_id: "cons1",
                links: { main: "actually_lidar" }
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

    /// Contract bindings check the producer's `manifest.implements`
    /// (not just node identity). A producer with no matching
    /// `implements` entry is rejected with
    /// `BindingContractNotImplemented`.
    #[test]
    fn contract_binding_rejects_non_implementing_producer() {
        let cons_instances = parse_instances(
            r#"[{
                instance_id: "cons1",
                links: { depth: "any_producer" }
            }]"#,
        );
        let depends_on = parse_depends_on(
            r#"{
                nodes: [],
                contracts: [{
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
        let ParsingError::BindingContractNotImplemented(info) = &out.errors[0] else {
            panic!(
                "expected BindingContractNotImplemented, got {:?}",
                out.errors[0]
            );
        };
        assert_eq!(info.binding, "depth");
        assert_eq!(info.contract_name, "depth_camera");
        assert_eq!(info.contract_tag, "v1");
        assert_eq!(info.producer_name, "whatever");
        assert_eq!(info.producer_tag, "v1");
    }

    /// Contract dep targets a producer whose `manifest.implements` includes the
    /// requested contract: accepted. The producer's node name is
    /// intentionally different from the contract name so this test
    /// exercises the implements path rather than a coincidental
    /// identity match.
    #[test]
    fn contract_binding_accepts_implementing_producer() {
        let cons_instances = parse_instances(
            r#"[{
                instance_id: "cons1",
                links: { depth: "webcam_inst_1" }
            }]"#,
        );
        let depends_on = parse_depends_on(
            r#"{
                nodes: [],
                contracts: [{
                    name: "depth_camera",
                    tag: "v1",
                    link_id: "depth"
                }]
            }"#,
        );
        let prod_instances = parse_instances(r#"[{ instance_id: "webcam_inst_1" }]"#);
        let producer_implements =
            parse_implements(r#"[{ name: "depth_camera", tag: "v1", link_id: "cam" }]"#);
        let items = vec![
            item("cons", "v1", &cons_instances, Some(&depends_on)),
            item_with_implements("webcam", "v1", &prod_instances, None, &producer_implements),
        ];
        let out = validate_bindings(&items, TEST_CORE);
        assert!(out.errors.is_empty(), "unexpected errors: {:?}", out.errors);
        assert_eq!(
            slot_binding(&out, "cons1", "depth"),
            single(TEST_CORE, "webcam_inst_1")
        );
    }

    /// A well-formed launcher (matching the openarm backbone shape:
    /// every declared contract slot bound) passes with no errors and
    /// every declared slot resolves.
    #[test]
    fn openarm_style_manifest_resolves_all_slots() {
        let cons_instances = parse_instances(
            r#"[{
                instance_id: "backbone_inst_1",
                links: {
                    wrist_left_camera: "depth_cam_inst1",
                    wrist_right_camera: "depth_cam_inst1"
                }
            }]"#,
        );
        let depends_on = parse_depends_on(
            r#"{
                nodes: [],
                contracts: [
                    { name: "depth_camera", tag: "v1", link_id: "wrist_left_camera" },
                    { name: "depth_camera", tag: "v1", link_id: "wrist_right_camera" }
                ]
            }"#,
        );
        let prod_instances = parse_instances(r#"[{ instance_id: "depth_cam_inst1" }]"#);
        // Producer's node name coincidentally matches the contract
        // name+tag, but the validator only honors explicit `manifest.implements`
        // claims; node-identity matching never satisfies an contract
        // slot.
        let producer_implements =
            parse_implements(r#"[{ name: "depth_camera", tag: "v1", link_id: "cam" }]"#);
        let items = vec![
            item(
                "openarm01_backbone",
                "v1",
                &cons_instances,
                Some(&depends_on),
            ),
            item_with_implements(
                "depth_camera",
                "v1",
                &prod_instances,
                None,
                &producer_implements,
            ),
        ];
        let out = validate_bindings(&items, TEST_CORE);
        assert!(out.errors.is_empty(), "unexpected errors: {:?}", out.errors);
        for link_id in ["wrist_left_camera", "wrist_right_camera"] {
            assert_eq!(
                slot_binding(&out, "backbone_inst_1", link_id),
                single(TEST_CORE, "depth_cam_inst1")
            );
        }
    }

    /// The same openarm backbone shape with `wrist_right_camera` left out
    /// of the bindings map is rejected by rule 5, naming the slot's
    /// contract.
    #[test]
    fn openarm_style_manifest_rejects_unbound_slot() {
        let cons_instances = parse_instances(
            r#"[{
                instance_id: "backbone_inst_1",
                links: {
                    wrist_left_camera: "depth_cam_inst1"
                }
            }]"#,
        );
        let depends_on = parse_depends_on(
            r#"{
                nodes: [],
                contracts: [
                    { name: "depth_camera", tag: "v1", link_id: "wrist_left_camera" },
                    { name: "depth_camera", tag: "v1", link_id: "wrist_right_camera" }
                ]
            }"#,
        );
        let prod_instances = parse_instances(r#"[{ instance_id: "depth_cam_inst1" }]"#);
        let producer_implements =
            parse_implements(r#"[{ name: "depth_camera", tag: "v1", link_id: "cam" }]"#);
        let items = vec![
            item(
                "openarm01_backbone",
                "v1",
                &cons_instances,
                Some(&depends_on),
            ),
            item_with_implements(
                "depth_camera",
                "v1",
                &prod_instances,
                None,
                &producer_implements,
            ),
        ];
        let out = validate_bindings(&items, TEST_CORE);
        assert_eq!(
            out.errors.len(),
            1,
            "expected one error, got {:?}",
            out.errors
        );
        let ParsingError::BindingSlotUnfulfilled(info) = &out.errors[0] else {
            panic!("expected BindingSlotUnfulfilled, got {:?}", out.errors[0]);
        };
        assert_eq!(info.owner_instance_id, "backbone_inst_1");
        assert_eq!(info.link_id, "wrist_right_camera");
        assert_eq!(info.slot_kind, SlotKind::Contract);
        assert_eq!(info.slot_name, "depth_camera");
        assert_eq!(info.slot_tag, "v1");
    }

    /// An "inert" item (`depends_on: None`) contributes no slots of its
    /// own; it represents a node whose bindings were already resolved at
    /// spawn time. Its instances and `implements` must still feed the
    /// producer-lookup index so a live consumer in the same
    /// `validate_bindings` call can satisfy node / contract deps against
    /// them.
    ///
    /// This locks in the contract that
    /// `peppy::commands::node::run::validate_binds_against_stack`
    /// relies on when it folds already-running stack nodes into the
    /// validator snapshot without re-checking their bindings.
    #[test]
    fn inert_item_with_no_depends_on_remains_a_producer() {
        // Live consumer with a node dep + an contract dep.
        let cons_instances = parse_instances(
            r#"[{
                instance_id: "cons1",
                links: { cam: "node_prod_inst", depth: "contract_prod_inst" }
            }]"#,
        );
        let cons_depends_on = parse_depends_on(
            r#"{
                nodes: [{ name: "camera", tag: "v1", link_id: "cam" }],
                contracts: [{ name: "depth_camera", tag: "v1", link_id: "depth" }]
            }"#,
        );

        // Inert node producer: depends_on omitted entirely, even though
        // it WOULD declare deps in real life.
        let node_prod_instances = parse_instances(r#"[{ instance_id: "node_prod_inst" }]"#);

        // Inert contract producer: same shape, plus an `implements`
        // entry that should still match the consumer's contract dep.
        let contract_prod_instances = parse_instances(r#"[{ instance_id: "contract_prod_inst" }]"#);
        let contract_prod_implements =
            parse_implements(r#"[{ name: "depth_camera", tag: "v1", link_id: "cam" }]"#);

        let items = vec![
            item("cons", "v1", &cons_instances, Some(&cons_depends_on)),
            // Inert items: depends_on intentionally `None`.
            item("camera", "v1", &node_prod_instances, None),
            item_with_implements(
                "webcam",
                "v1",
                &contract_prod_instances,
                None,
                &contract_prod_implements,
            ),
        ];
        let out = validate_bindings(&items, TEST_CORE);
        assert!(out.errors.is_empty(), "unexpected errors: {:?}", out.errors);
        assert_eq!(
            slot_binding(&out, "cons1", "cam"),
            single(TEST_CORE, "node_prod_inst")
        );
        assert_eq!(
            slot_binding(&out, "cons1", "depth"),
            single(TEST_CORE, "contract_prod_inst")
        );
    }

    /// Defensive: an instance lookup miss for a binding target.
    #[test]
    fn rejects_binding_whose_target_is_unknown_to_planner() {
        let cons_instances = parse_instances(
            r#"[{
                instance_id: "cons1",
                links: { main: "ghost_producer" }
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
            link,
            instance_id,
        } = &out.errors[0]
        else {
            panic!("expected UnknownInstanceId, got {:?}", out.errors[0]);
        };
        assert_eq!(owner_instance_id, "cons1");
        assert_eq!(link, "main");
        assert_eq!(instance_id, "ghost_producer");
    }

    /// Rule 4: stack-wide instance_id duplicate across different
    /// (node_name, node_tag).
    #[test]
    fn rule4_rejects_stack_wide_duplicate_instance_id() {
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
    fn rule4_reports_duplicate_instance_id_even_for_same_name_tag() {
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

    /// Errors aggregate (no short-circuit) within `validate_bindings`: a bad
    /// target on one binding slot does not stop a sibling slot from being
    /// checked. An unknown-slot KEY is not this validator's concern (a link
    /// key naming no producer slot is skipped here and reported once by
    /// `validate_link_slots`), so only the target error surfaces.
    #[test]
    fn aggregates_multiple_errors() {
        let cons_instances = parse_instances(
            r#"[{
                instance_id: "cons1",
                links: { extra: "ghost", main: "also_ghost" }
            }]"#,
        );
        let depends_on = parse_depends_on(
            r#"{
                nodes: [
                    { name: "camera", tag: "v1", link_id: "main" },
                    { name: "camera", tag: "v1", link_id: "extra" }
                ]
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
            "expected two UnknownInstanceId errors, got {:?}",
            out.errors
        );
        assert!(
            out.errors
                .iter()
                .all(|e| matches!(e, ParsingError::UnknownInstanceId { .. })),
            "both slots' bad targets should surface: {:?}",
            out.errors
        );
    }

    /// `implements` matching is strict on `(name, tag)`: a producer
    /// declaring a different tag for the same contract name is
    /// rejected.
    #[test]
    fn contract_dep_with_wrong_tag_in_implements_is_rejected() {
        let cons_instances = parse_instances(
            r#"[{
                instance_id: "cons1",
                links: { depth: "webcam_inst_1" }
            }]"#,
        );
        let depends_on = parse_depends_on(
            r#"{
                nodes: [],
                contracts: [{
                    name: "depth_camera",
                    tag: "v1",
                    link_id: "depth"
                }]
            }"#,
        );
        let prod_instances = parse_instances(r#"[{ instance_id: "webcam_inst_1" }]"#);
        let producer_implements =
            parse_implements(r#"[{ name: "depth_camera", tag: "v2", link_id: "cam" }]"#);
        let items = vec![
            item("cons", "v1", &cons_instances, Some(&depends_on)),
            item_with_implements("webcam", "v1", &prod_instances, None, &producer_implements),
        ];
        let out = validate_bindings(&items, TEST_CORE);
        assert_eq!(out.errors.len(), 1, "errors: {:?}", out.errors);
        let ParsingError::BindingContractNotImplemented(info) = &out.errors[0] else {
            panic!(
                "expected BindingContractNotImplemented, got {:?}",
                out.errors[0]
            );
        };
        assert_eq!(info.contract_tag, "v1");
        assert_eq!(info.producer_name, "webcam");
    }

    /// A producer declaring multiple `implements` entries can satisfy
    /// any of them. Two consumers (each asking for a different
    /// contract) both successfully bind to the same producer.
    #[test]
    fn producer_with_multiple_implements_can_satisfy() {
        let depth_consumer = parse_instances(
            r#"[{
                instance_id: "depth_cons",
                links: { feed: "multi_prod" }
            }]"#,
        );
        let depth_deps = parse_depends_on(
            r#"{
                nodes: [],
                contracts: [{
                    name: "depth_camera",
                    tag: "v1",
                    link_id: "feed"
                }]
            }"#,
        );
        let uvc_consumer = parse_instances(
            r#"[{
                instance_id: "uvc_cons",
                links: { feed: "multi_prod" }
            }]"#,
        );
        let uvc_deps = parse_depends_on(
            r#"{
                nodes: [],
                contracts: [{
                    name: "uvc_camera",
                    tag: "v1",
                    link_id: "feed"
                }]
            }"#,
        );
        let prod_instances = parse_instances(r#"[{ instance_id: "multi_prod" }]"#);
        let producer_implements = parse_implements(
            r#"[
                { name: "depth_camera", tag: "v1", link_id: "depth" },
                { name: "uvc_camera", tag: "v1", link_id: "uvc" }
            ]"#,
        );
        let items = vec![
            item("depth_cons_node", "v1", &depth_consumer, Some(&depth_deps)),
            item("uvc_cons_node", "v1", &uvc_consumer, Some(&uvc_deps)),
            item_with_implements(
                "multi_camera",
                "v1",
                &prod_instances,
                None,
                &producer_implements,
            ),
        ];
        let out = validate_bindings(&items, TEST_CORE);
        assert!(out.errors.is_empty(), "unexpected errors: {:?}", out.errors);
        assert_eq!(
            slot_binding(&out, "depth_cons", "feed"),
            single(TEST_CORE, "multi_prod")
        );
        assert_eq!(
            slot_binding(&out, "uvc_cons", "feed"),
            single(TEST_CORE, "multi_prod")
        );
    }

    /// Pairing slots are not binding slots: a required pairing slot with
    /// no binding produces no error and no slot entry here (the pairing
    /// validator owns that surface).
    #[test]
    fn pairing_slots_are_invisible_to_binding_validation() {
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

    /// Stamping: every producer reference the validator emits carries
    /// exactly the `producer_core_node` passed by the caller (the
    /// launching daemon). This is the single point where the
    /// instance-only `--bind` syntax becomes a wire-complete address.
    #[test]
    fn every_resolved_binding_is_stamped_with_the_launching_core_node() {
        let cons_instances = parse_instances(
            r#"[{
                instance_id: "cons1",
                links: { main: "prod1", extra: "prod2" }
            }]"#,
        );
        let depends_on = parse_depends_on(
            r#"{
                nodes: [
                    { name: "camera", tag: "v1", link_id: "main" },
                    { name: "camera", tag: "v1", link_id: "extra" }
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
        assert_eq!(resolved.len(), 2, "both slots must resolve");
        for producer in resolved.values().flat_map(|bound| bound.iter()) {
            assert_eq!(producer.core_node, "daemon_west");
        }
    }
}
