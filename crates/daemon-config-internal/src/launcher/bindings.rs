//! Plan-phase validation for the launcher's per-instance `bindings`
//! field. Runs after node configs are loaded so the validator can
//! cross-reference each consumer's `depends_on` against the running
//! stack snapshot's `instance_id → (name, tag)` lookup.
//!
//! Every binding entry maps a declared slot to its producers: the KEY
//! must equal a `depends_on.{nodes,contracts}` `link_id` and each
//! target must deploy the slot's node (node slots) or conform to the
//! slot's contract (contract slots). A declared slot with no binding
//! entry stays unbound — the consumer's subscription for it is silent;
//! there is no wildcard fallback and no free-form key.
//!
//! The validator emits both errors and the resolved per-slot producer
//! lists per consumer instance, which the caller serializes into
//! [`config::runtime::NodeInstanceConfig::slot_bindings`].

use crate::error::{
    BindingContractNotConformed, BindingTargetMismatch, BindingUnknownSlot,
    DuplicateInstanceIdAcrossStack, ParsingError, SlotKind,
};
use config::node::{ConformsToItem, DependsOn};
use config::runtime::ProducerRef;
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
    /// to decide whether this node can satisfy a consumer's contract
    /// slot.
    pub conforms_to: &'a [ConformsToItem],
}

/// Per-slot metadata extracted from `depends_on` during validation.
/// Carrying `kind` inline lets the target-matching path pick the right
/// error (node mismatch vs contract not conformed) without re-scanning
/// `depends_on` per binding.
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
    /// `consumer_instance_id → link_id → bound producers`. Every declared
    /// slot of an instance appears, an unbound slot with an empty list.
    pub slot_bindings: BTreeMap<String, BTreeMap<String, Vec<ProducerRef>>>,
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
/// 1. Every binding `KEY` equals a declared `depends_on.{nodes,
///    contracts}` `link_id`. A key naming a pairing slot gets the
///    targeted [`ParsingError::BindingKeyIsPairingSlot`]; any other
///    unknown key is [`ParsingError::BindingUnknownSlot`].
/// 2. Every target exists in the snapshot
///    ([`ParsingError::UnknownInstanceId`] otherwise) and satisfies the
///    slot: node slots match the target's `(name, tag)` identity
///    ([`ParsingError::BindingTargetMismatch`] otherwise), contract
///    slots match the target's `conforms_to`
///    ([`ParsingError::BindingContractNotConformed`] otherwise).
/// 3. `--bind KEY` uniqueness within one invocation is enforced by the
///    CLI parser and the deserializer; this validator surfaces any
///    residual duplicates as [`ParsingError::BindingDuplicateKey`]
///    (defensive; should not fire in practice).
/// 4. Stack-wide `instance_id` uniqueness across every entry in
///    `items.instances` is enforced; collisions emit
///    [`ParsingError::DuplicateInstanceIdAcrossStack`].
///
/// A declared slot with no binding entry is legal: it resolves to an
/// empty producer list and the consumer's subscription for it stays
/// silent.
pub fn validate_bindings(
    items: &[BindingValidationItem<'_>],
    producer_core_node: &str,
) -> ValidatedBindings {
    let mut out = ValidatedBindings::default();

    check_stack_wide_instance_id_uniqueness(items, &mut out.errors);

    let instance_to_item = build_instance_lookup(items);

    for item in items {
        let declared_slots = collect_declared_slots(item.depends_on);
        let declared_csv = format_declared_keys(&declared_slots);
        // Pairing slots are never binding slots: `collect_declared_slots`
        // reads only `depends_on.{nodes,contracts}`, so a pairing link_id
        // can never match a binding key. This set exists solely to turn a
        // `--bind` on a pairing slot into a targeted "use --pair" error
        // instead of a generic `BindingUnknownSlot`.
        let pairing_link_ids: std::collections::BTreeSet<&str> = item
            .depends_on
            .map(|d| d.pairings.iter().map(|p| p.link_id.as_str()).collect())
            .unwrap_or_default();

        for instance in item.instances {
            let mut resolved: BTreeMap<String, Vec<ProducerRef>> = BTreeMap::new();
            let mut seen_keys: std::collections::BTreeSet<&str> = std::collections::BTreeSet::new();

            for (binding_key, target_ids) in &instance.bindings {
                // Rule 3 defensive check.
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

                // Rule 1: KEY must name a declared slot.
                let Some(slot) = declared_slots.get(binding_key.as_str()).copied() else {
                    out.errors.push(ParsingError::BindingUnknownSlot(Box::new(
                        BindingUnknownSlot {
                            owner_instance_id: instance.instance_id.to_string(),
                            binding: binding_key.clone(),
                            declared_link_ids: declared_csv.clone(),
                        },
                    )));
                    continue;
                };

                // Rule 2: every target exists and satisfies the slot.
                let mut producers = Vec::with_capacity(target_ids.len());
                let mut target_errors = false;
                for target_id in target_ids {
                    let Some(target_item) = instance_to_item.get(target_id.as_str()) else {
                        out.errors.push(ParsingError::UnknownInstanceId {
                            owner_instance_id: instance.instance_id.to_string(),
                            binding: binding_key.clone(),
                            instance_id: target_id.clone(),
                        });
                        target_errors = true;
                        continue;
                    };
                    if !slot_matches_producer(&slot, target_item) {
                        out.errors.push(match slot.kind {
                            SlotKind::Node => ParsingError::BindingTargetMismatch(Box::new(
                                BindingTargetMismatch {
                                    owner_instance_id: instance.instance_id.to_string(),
                                    binding: binding_key.clone(),
                                    target_instance_id: target_id.clone(),
                                    expected_name: slot.name.to_string(),
                                    expected_tag: slot.tag.to_string(),
                                    actual_name: target_item.node_name.to_string(),
                                    actual_tag: target_item.node_tag.to_string(),
                                },
                            )),
                            SlotKind::Contract => ParsingError::BindingContractNotConformed(
                                Box::new(BindingContractNotConformed {
                                    owner_instance_id: instance.instance_id.to_string(),
                                    binding: binding_key.clone(),
                                    target_instance_id: target_id.clone(),
                                    contract_name: slot.name.to_string(),
                                    contract_tag: slot.tag.to_string(),
                                    producer_name: target_item.node_name.to_string(),
                                    producer_tag: target_item.node_tag.to_string(),
                                }),
                            ),
                        });
                        target_errors = true;
                        continue;
                    }
                    producers.push(ProducerRef::new(producer_core_node, target_id.clone()));
                }
                if !target_errors {
                    resolved.insert(binding_key.clone(), producers);
                }
            }

            // Materialize every declared slot the bindings left out as
            // explicitly unbound (empty producer list), so the boot config
            // carries the complete slot picture and the runtime keeps the
            // slot silent.
            for slot_link_id in declared_slots.keys() {
                resolved.entry((*slot_link_id).to_string()).or_default();
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
/// entry keyed by `link_id`, with the dep's `(name, tag, kind)` so the
/// target-matching path can branch on node-vs-contract without
/// re-scanning the manifest.
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
                },
            );
        }
    }
    slots
}

/// Does a producer satisfy a declared slot? Node slots match by
/// `(name, tag)` identity; contract slots match against the producer's
/// `conforms_to`. sha256 is not cross-checked here; each side
/// independently verifies its own declared sha256 against the on-disk
/// contract document at cache resolution time.
fn slot_matches_producer(slot: &SlotMeta<'_>, producer: &BindingValidationItem<'_>) -> bool {
    match slot.kind {
        SlotKind::Node => producer.node_name == slot.name && producer.node_tag == slot.tag,
        SlotKind::Contract => producer
            .conforms_to
            .iter()
            .any(|item| item.name.as_str() == slot.name && item.tag.as_str() == slot.tag),
    }
}

fn format_declared_keys(slots: &DeclaredSlots<'_>) -> String {
    let keys: Vec<&str> = slots.keys().copied().collect();
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
    /// that exercise contract-conformance matching.
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

    fn slot_binding(
        out: &ValidatedBindings,
        instance: &str,
        link_id: &str,
    ) -> Option<Vec<ProducerRef>> {
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

    /// Rule 1: a binding whose KEY names no declared slot is rejected.
    #[test]
    fn rule1_rejects_unknown_slot_key() {
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
        let ParsingError::BindingUnknownSlot(info) = &out.errors[0] else {
            panic!("expected BindingUnknownSlot, got {:?}", out.errors[0]);
        };
        assert_eq!(info.owner_instance_id, "cons1");
        assert_eq!(info.binding, "stale_slot");
        assert_eq!(info.declared_link_ids, "main");
    }

    /// A declared slot with no binding entry is legal and resolves to an
    /// empty producer list: the consumer's subscription stays silent.
    #[test]
    fn unbound_slot_resolves_to_empty_producer_list() {
        let instances = parse_instances(r#"[{ instance_id: "cons1" }]"#);
        let depends_on = parse_depends_on(
            r#"{
                nodes: [{ name: "camera", tag: "v1", link_id: "main" }]
            }"#,
        );
        let items = vec![item("cons", "v1", &instances, Some(&depends_on))];
        let out = validate_bindings(&items, TEST_CORE);
        assert!(out.errors.is_empty(), "unexpected errors: {:?}", out.errors);
        assert_eq!(slot_binding(&out, "cons1", "main"), Some(Vec::new()));
    }

    /// Rule 2 (happy path): a single-target binding resolves the slot to
    /// that producer.
    #[test]
    fn rule2_single_target_binding_resolves() {
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
            Some(vec![ProducerRef::new(TEST_CORE, "prod1")])
        );
    }

    /// Rule 2 (happy path): an array binding resolves the slot to every
    /// listed producer, in binding order.
    #[test]
    fn rule2_array_binding_resolves_all_targets_in_order() {
        let cons_instances = parse_instances(
            r#"[{
                instance_id: "cons1",
                bindings: { states: ["prod2", "prod1"] }
            }]"#,
        );
        let depends_on = parse_depends_on(
            r#"{
                nodes: [{ name: "camera", tag: "v1", link_id: "states" }]
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
            slot_binding(&out, "cons1", "states"),
            Some(vec![
                ProducerRef::new(TEST_CORE, "prod2"),
                ProducerRef::new(TEST_CORE, "prod1"),
            ])
        );
    }

    /// Rule 2: every target of an array binding must satisfy the slot; a
    /// single bad target rejects the binding while the good target's
    /// sibling error stands alone.
    #[test]
    fn rule2_array_binding_rejects_any_mismatched_target() {
        let cons_instances = parse_instances(
            r#"[{
                instance_id: "cons1",
                bindings: { states: ["prod1", "actually_lidar"] }
            }]"#,
        );
        let depends_on = parse_depends_on(
            r#"{
                nodes: [{ name: "camera", tag: "v1", link_id: "states" }]
            }"#,
        );
        let cam_instances = parse_instances(r#"[{ instance_id: "prod1" }]"#);
        let lidar_instances = parse_instances(r#"[{ instance_id: "actually_lidar" }]"#);
        let items = vec![
            item("cons", "v1", &cons_instances, Some(&depends_on)),
            item("camera", "v1", &cam_instances, None),
            item("lidar", "v1", &lidar_instances, None),
        ];
        let out = validate_bindings(&items, TEST_CORE);
        assert_eq!(out.errors.len(), 1, "errors: {:?}", out.errors);
        let ParsingError::BindingTargetMismatch(info) = &out.errors[0] else {
            panic!("expected BindingTargetMismatch, got {:?}", out.errors[0]);
        };
        assert_eq!(info.target_instance_id, "actually_lidar");
    }

    /// Rule 5: pinned binding whose target deploys the wrong node.
    #[test]
    fn rejects_target_node_mismatch() {
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

    /// Contract bindings check the producer's `conforms_to`
    /// (not just node identity). A producer with no matching
    /// `conforms_to` entry is rejected with
    /// `BindingContractNotConformed`.
    #[test]
    fn contract_binding_rejects_non_conforming_producer() {
        let cons_instances = parse_instances(
            r#"[{
                instance_id: "cons1",
                bindings: { depth: "any_producer" }
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
        let ParsingError::BindingContractNotConformed(info) = &out.errors[0] else {
            panic!(
                "expected BindingContractNotConformed, got {:?}",
                out.errors[0]
            );
        };
        assert_eq!(info.binding, "depth");
        assert_eq!(info.contract_name, "depth_camera");
        assert_eq!(info.contract_tag, "v1");
        assert_eq!(info.producer_name, "whatever");
        assert_eq!(info.producer_tag, "v1");
    }

    /// Contract dep targets a producer whose `conforms_to` includes the
    /// requested contract: accepted. The producer's node name is
    /// intentionally different from the contract name so this test
    /// exercises the conformance path rather than a coincidental
    /// identity match.
    #[test]
    fn contract_binding_accepts_conforming_producer() {
        let cons_instances = parse_instances(
            r#"[{
                instance_id: "cons1",
                bindings: { depth: "webcam_inst_1" }
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
        let producer_conforms = parse_conforms_to(r#"[{ name: "depth_camera", tag: "v1" }]"#);
        let items = vec![
            item("cons", "v1", &cons_instances, Some(&depends_on)),
            item_with_conforms_to("webcam", "v1", &prod_instances, None, &producer_conforms),
        ];
        let out = validate_bindings(&items, TEST_CORE);
        assert!(out.errors.is_empty(), "unexpected errors: {:?}", out.errors);
        assert_eq!(
            slot_binding(&out, "cons1", "depth"),
            Some(vec![ProducerRef::new(TEST_CORE, "webcam_inst_1")])
        );
    }

    /// A well-formed launcher (matching the openarm backbone shape: two
    /// bound contract slots and one left unbound) passes with no errors
    /// and every declared slot resolves.
    #[test]
    fn openarm_style_manifest_resolves_all_slots() {
        let cons_instances = parse_instances(
            r#"[{
                instance_id: "backbone_inst_1",
                bindings: {
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
                    { name: "depth_camera", tag: "v1", link_id: "wrist_right_camera" },
                    { name: "depth_camera", tag: "v1", link_id: "extra_cam" }
                ]
            }"#,
        );
        let prod_instances = parse_instances(r#"[{ instance_id: "depth_cam_inst1" }]"#);
        // Producer's node name coincidentally matches the contract
        // name+tag, but the validator only honors explicit `conforms_to`
        // claims; node-identity matching never satisfies an contract
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
            Some(vec![ProducerRef::new(TEST_CORE, "depth_cam_inst1")])
        );
        assert_eq!(
            slot_binding(&out, "backbone_inst_1", "wrist_right_camera"),
            Some(vec![ProducerRef::new(TEST_CORE, "depth_cam_inst1")])
        );
        assert_eq!(
            slot_binding(&out, "backbone_inst_1", "extra_cam"),
            Some(Vec::new()),
            "the unbound slot must materialize as an empty producer list"
        );
    }

    /// An "inert" item (`depends_on: None`) contributes no slots of its
    /// own; it represents a node whose bindings were already resolved at
    /// spawn time. Its instances and `conforms_to` must still feed the
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
                bindings: { cam: "node_prod_inst", depth: "iface_prod_inst" }
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

        // Inert contract producer: same shape, plus a `conforms_to`
        // entry that should still match the consumer's contract dep.
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
            Some(vec![ProducerRef::new(TEST_CORE, "node_prod_inst")])
        );
        assert_eq!(
            slot_binding(&out, "cons1", "depth"),
            Some(vec![ProducerRef::new(TEST_CORE, "iface_prod_inst")])
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

    /// Errors aggregate (no short-circuit): an unknown slot key and an
    /// unknown target instance surface together.
    #[test]
    fn aggregates_multiple_errors() {
        let cons_instances = parse_instances(
            r#"[{
                instance_id: "cons1",
                bindings: { unknown_slot: "prod1", main: "ghost" }
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
        // Bindings iterate in key order: `main` errors first, then
        // `unknown_slot`.
        assert!(matches!(
            out.errors[0],
            ParsingError::UnknownInstanceId { .. }
        ));
        assert!(matches!(out.errors[1], ParsingError::BindingUnknownSlot(_)));
    }

    /// `conforms_to` matching is strict on `(name, tag)`: a producer
    /// declaring a different tag for the same contract name is
    /// rejected.
    #[test]
    fn contract_dep_with_wrong_tag_in_conforms_to_is_rejected() {
        let cons_instances = parse_instances(
            r#"[{
                instance_id: "cons1",
                bindings: { depth: "webcam_inst_1" }
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
        let producer_conforms = parse_conforms_to(r#"[{ name: "depth_camera", tag: "v2" }]"#);
        let items = vec![
            item("cons", "v1", &cons_instances, Some(&depends_on)),
            item_with_conforms_to("webcam", "v1", &prod_instances, None, &producer_conforms),
        ];
        let out = validate_bindings(&items, TEST_CORE);
        assert_eq!(out.errors.len(), 1, "errors: {:?}", out.errors);
        let ParsingError::BindingContractNotConformed(info) = &out.errors[0] else {
            panic!(
                "expected BindingContractNotConformed, got {:?}",
                out.errors[0]
            );
        };
        assert_eq!(info.contract_tag, "v1");
        assert_eq!(info.producer_name, "webcam");
    }

    /// A producer declaring multiple `conforms_to` entries can satisfy
    /// any of them. Two consumers (each asking for a different
    /// contract) both successfully bind to the same producer.
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
                bindings: { feed: "multi_prod" }
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
            Some(vec![ProducerRef::new(TEST_CORE, "multi_prod")])
        );
        assert_eq!(
            slot_binding(&out, "uvc_cons", "feed"),
            Some(vec![ProducerRef::new(TEST_CORE, "multi_prod")])
        );
    }

    /// A `--bind` whose KEY names a pairing slot gets the targeted
    /// "use --pair" error, not `BindingUnknownSlot`. Pairing slots are
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
                bindings: { main: "prod1", extra: ["prod2"] }
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
        let producers: Vec<&ProducerRef> = resolved.values().flatten().collect();
        assert_eq!(producers.len(), 2, "both bound producers must be stamped");
        for producer in producers {
            assert_eq!(producer.core_node, "daemon_west");
        }
    }
}
