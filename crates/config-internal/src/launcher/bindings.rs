//! Plan-phase validation for the launcher's per-instance `bindings`
//! field. Runs after node configs are loaded so the validator can
//! cross-reference each consumer's `depends_on` against the running
//! stack snapshot's `instance_id → (name, tag)` lookup.
//!
//! In the binding-driven dispatch model, every `(KEY, VALUE)` binding
//! resolves to one of the consumer's declared slots:
//!
//! - If `KEY` matches a declared pinned `link_id`, the binding pins
//!   that slot to producer `VALUE`.
//! - Else, if a `from_any: true` slot exists for `VALUE`'s `(name,
//!   tag)`, the binding attaches `VALUE` to that slot under the
//!   free-form label `KEY`. Multiple bindings on the same from_any
//!   slot accumulate.
//! - Else, the binding is dead (rejected).
//!
//! The validator emits both errors and the resolved per-slot
//! `SlotBinding` map per consumer instance, which the caller
//! serializes into [`crate::runtime::NodeInstanceConfig::slot_bindings`].

use crate::error::{
    BindingDeadKey, BindingInterfaceNotConformed, BindingMissingForPinnedDep,
    BindingTargetMismatch, DuplicateInstanceIdAcrossStack, ParsingError,
};
use crate::node::{ConformsToItem, DependsOn};
use crate::runtime::SlotBinding;
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

/// Whether a declared slot is a node dep (matched by `(name, tag)`
/// identity) or an interface dep (matched against the producer's
/// `conforms_to`).
#[derive(Clone, Copy)]
enum SlotKind {
    Node,
    Interface,
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
/// Rules enforced (numbered to match `BINDING_ROUTING.md`):
/// 1. Every pinned `depends_on` entry has a matching `--bind` whose
///    `KEY` equals the slot's `link_id`. Otherwise
///    [`ParsingError::BindingMissingForPinnedDep`].
/// 3. Free-form `--bind KEY@VALUE` where `KEY` doesn't match a pinned
///    `link_id` is accepted iff a `from_any: true` slot exists for
///    VALUE's `(name, tag)`. Multiple bindings on the same from_any
///    slot accumulate.
/// 4. A `--bind` whose `KEY` matches neither a pinned `link_id` nor a
///    `from_any` slot for VALUE's `(name, tag)` is
///    [`ParsingError::BindingDeadKey`].
/// 5. A pinned binding whose target instance deploys the wrong node
///    is [`ParsingError::BindingTargetMismatch`].
/// 6. `--bind KEY` uniqueness within one invocation is enforced by the
///    CLI parser and the deserializer; this validator surfaces any
///    residual duplicates as
///    [`ParsingError::BindingDuplicateKey`] (defensive — should not
///    fire in practice).
/// 7. Stack-wide `instance_id` uniqueness across every entry in
///    `items.instances` is enforced; collisions emit
///    [`ParsingError::DuplicateInstanceIdAcrossStack`].
pub fn validate_bindings(items: &[BindingValidationItem<'_>]) -> ValidatedBindings {
    let mut out = ValidatedBindings::default();

    check_stack_wide_instance_id_uniqueness(items, &mut out.errors);

    let instance_to_item = build_instance_lookup(items);

    for item in items {
        let (declared_pinned, declared_from_any) = collect_declared_slots(item.depends_on);
        let declared_csv = format_declared_keys(&declared_pinned, &declared_from_any);

        for instance in item.instances {
            let mut resolved: BTreeMap<String, SlotBinding> = BTreeMap::new();
            let mut from_any_explicit: BTreeMap<String, Vec<String>> = BTreeMap::new();
            let mut seen_keys: std::collections::BTreeSet<&str> = std::collections::BTreeSet::new();
            // Pinned link_ids whose KEY appeared in the binding map
            // (even if the resolution errored). Used to skip rule 1's
            // pinned-unbound report so we don't double-emit on top of a
            // BindingTargetMismatch / UnknownInstanceId for the same
            // slot.
            let mut pinned_keys_seen: std::collections::BTreeSet<&str> =
                std::collections::BTreeSet::new();

            for (binding_key, target_id) in &instance.bindings {
                // Rule 6 defensive check.
                if !seen_keys.insert(binding_key.as_str()) {
                    out.errors.push(ParsingError::BindingDuplicateKey {
                        owner_instance_id: instance.instance_id.to_string(),
                        binding: binding_key.clone(),
                    });
                    continue;
                }

                if let Some(slot) = declared_pinned.get(binding_key.as_str()).copied() {
                    // Rule 2: KEY matches a declared pinned link_id.
                    pinned_keys_seen.insert(binding_key.as_str());
                    let Some(target_item) = instance_to_item.get(target_id.as_str()) else {
                        out.errors.push(ParsingError::UnknownInstanceId {
                            owner_instance_id: instance.instance_id.to_string(),
                            binding: binding_key.clone(),
                            instance_id: target_id.clone(),
                        });
                        continue;
                    };
                    let producer_satisfies = match slot.kind {
                        SlotKind::Node => {
                            target_item.node_name == slot.name && target_item.node_tag == slot.tag
                        }
                        SlotKind::Interface => producer_satisfies_interface(
                            target_item.conforms_to,
                            slot.name,
                            slot.tag,
                        ),
                    };
                    if !producer_satisfies {
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
                            SlotKind::Interface => ParsingError::BindingInterfaceNotConformed(
                                Box::new(BindingInterfaceNotConformed {
                                    owner_instance_id: instance.instance_id.to_string(),
                                    binding: binding_key.clone(),
                                    target_instance_id: target_id.clone(),
                                    interface_name: slot.name.to_string(),
                                    interface_tag: slot.tag.to_string(),
                                    producer_name: target_item.node_name.to_string(),
                                    producer_tag: target_item.node_tag.to_string(),
                                }),
                            ),
                        });
                        continue;
                    }
                    resolved.insert(
                        binding_key.clone(),
                        SlotBinding::Pinned {
                            producer_instance_id: target_id.clone(),
                        },
                    );
                    continue;
                }

                // KEY does not match a pinned link_id. Try to attach to a
                // from_any slot. Node slots match by `(name, tag)`;
                // interface slots match against the producer's
                // `conforms_to` claim.
                let Some(target_item) = instance_to_item.get(target_id.as_str()) else {
                    out.errors.push(ParsingError::UnknownInstanceId {
                        owner_instance_id: instance.instance_id.to_string(),
                        binding: binding_key.clone(),
                        instance_id: target_id.clone(),
                    });
                    continue;
                };
                let producer_name_tag =
                    format!("{}:{}", target_item.node_name, target_item.node_tag);

                let mut attached = false;
                for (slot_link_id, slot) in &declared_from_any {
                    let matches = match slot.kind {
                        SlotKind::Node => {
                            target_item.node_name == slot.name && target_item.node_tag == slot.tag
                        }
                        SlotKind::Interface => producer_satisfies_interface(
                            target_item.conforms_to,
                            slot.name,
                            slot.tag,
                        ),
                    };
                    if matches {
                        from_any_explicit
                            .entry((*slot_link_id).to_string())
                            .or_default()
                            .push(target_id.clone());
                        attached = true;
                        break;
                    }
                }
                if !attached {
                    out.errors
                        .push(ParsingError::BindingDeadKey(Box::new(BindingDeadKey {
                            owner_instance_id: instance.instance_id.to_string(),
                            binding: binding_key.clone(),
                            target_instance_id: target_id.clone(),
                            producer_name_tag,
                            declared_link_ids: declared_csv.clone(),
                        })));
                }
            }

            // After processing all bindings, materialize from_any slots.
            for slot_link_id in declared_from_any.keys() {
                let producers = from_any_explicit.remove(*slot_link_id);
                let slot = match producers {
                    Some(ids) => SlotBinding::FromAnyBound {
                        producer_instance_ids: ids,
                    },
                    None => SlotBinding::FromAnyUnbound,
                };
                resolved.insert((*slot_link_id).to_string(), slot);
            }

            // Rule 1: every pinned slot must be bound. Suppress when
            // the slot's KEY was present in the binding map but
            // errored elsewhere — surfacing both
            // `BindingTargetMismatch` and `BindingMissingForPinnedDep`
            // for the same slot is double-reporting one root cause.
            for (slot_link_id, slot) in &declared_pinned {
                if resolved.contains_key(*slot_link_id) {
                    continue;
                }
                if pinned_keys_seen.contains(*slot_link_id) {
                    continue;
                }
                let kind = match slot.kind {
                    SlotKind::Interface => "interfaces",
                    SlotKind::Node => "nodes",
                };
                out.errors
                    .push(ParsingError::BindingMissingForPinnedDep(Box::new(
                        BindingMissingForPinnedDep {
                            owner_instance_id: instance.instance_id.to_string(),
                            link_id: (*slot_link_id).to_string(),
                            kind: kind.to_string(),
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
                if *name_a == item.node_name && *tag_a == item.node_tag {
                    // Two instances of the same node-tag pair using the
                    // same id is a separate (intra-deployment) check
                    // performed by [`deserialize_instances`]. Skip here
                    // to avoid double-reporting.
                    continue;
                }
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

/// A producer node satisfies an interface slot iff its `conforms_to`
/// list contains an item with the slot's `(name, tag)`. sha256 is not
/// cross-checked here; each side independently verifies its own
/// declared sha256 against the on-disk interface document at cache
/// resolution time.
fn producer_satisfies_interface(
    producer_conforms_to: &[ConformsToItem],
    slot_name: &str,
    slot_tag: &str,
) -> bool {
    producer_conforms_to
        .iter()
        .any(|item| item.name.as_str() == slot_name && item.tag.as_str() == slot_tag)
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

    /// Like `item` but also threads a `conforms_to` slice — for tests
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
        let out = validate_bindings(&[]);
        assert!(out.errors.is_empty());
        assert!(out.slot_bindings.is_empty());
    }

    /// A consumer with no `depends_on` and no `bindings` is trivially
    /// valid.
    #[test]
    fn consumer_without_depends_on_and_without_bindings_is_valid() {
        let instances = parse_instances(r#"[{ instance_id: "cons1" }]"#);
        let items = vec![item("cons", "v1", &instances, None)];
        let out = validate_bindings(&items);
        assert!(out.errors.is_empty(), "unexpected errors: {:?}", out.errors);
        assert!(out.slot_bindings.is_empty());
    }

    /// Rule 4: a `--bind KEY@VALUE` whose KEY matches neither a pinned
    /// link_id nor a from_any slot for VALUE's (name, tag) is dead.
    #[test]
    fn rule4_rejects_dead_binding_key() {
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
        let out = validate_bindings(&items);
        assert_eq!(
            out.errors.len(),
            1,
            "expected one error, got {:?}",
            out.errors
        );
        let ParsingError::BindingDeadKey(info) = &out.errors[0] else {
            panic!("expected BindingDeadKey, got {:?}", out.errors[0]);
        };
        assert_eq!(info.owner_instance_id, "cons1");
        assert_eq!(info.binding, "stale_slot");
        assert_eq!(info.target_instance_id, "prod1");
        assert_eq!(info.producer_name_tag, "camera:v1");
        assert_eq!(info.declared_link_ids, "main");
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
        let out = validate_bindings(&items);
        assert_eq!(out.errors.len(), 1);
        let ParsingError::BindingMissingForPinnedDep(info) = &out.errors[0] else {
            panic!(
                "expected BindingMissingForPinnedDep, got {:?}",
                out.errors[0]
            );
        };
        assert_eq!(info.owner_instance_id, "cons1");
        assert_eq!(info.link_id, "main");
        assert_eq!(info.kind, "nodes");
        assert_eq!(info.expected_name, "camera");
        assert_eq!(info.expected_tag, "v1");
        let msg = info.to_string();
        assert!(
            msg.contains("pinned deps must be bound"),
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
        let out = validate_bindings(&items);
        assert!(out.errors.is_empty(), "unexpected errors: {:?}", out.errors);
        assert_eq!(
            slot_binding(&out, "cons1", "main"),
            Some(SlotBinding::Pinned {
                producer_instance_id: "prod1".to_string()
            })
        );
    }

    /// Rule 3 (happy path): a free-form key whose target's (name, tag)
    /// matches a from_any slot attaches the binding to that slot.
    #[test]
    fn rule3_free_form_key_resolves_to_from_any_slot() {
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
        let prod_instances = parse_instances(r#"[{ instance_id: "prod1" }]"#);
        let items = vec![
            item("cons", "v1", &cons_instances, Some(&depends_on)),
            item("camera", "v1", &prod_instances, None),
        ];
        let out = validate_bindings(&items);
        assert!(out.errors.is_empty(), "unexpected errors: {:?}", out.errors);
        assert_eq!(
            slot_binding(&out, "cons1", "extra"),
            Some(SlotBinding::FromAnyBound {
                producer_instance_ids: vec!["prod1".to_string()]
            })
        );
    }

    /// Rule 3: multiple free-form keys on the same from_any slot
    /// accumulate.
    #[test]
    fn rule3_multiple_free_form_keys_accumulate_on_from_any_slot() {
        let cons_instances = parse_instances(
            r#"[{
                instance_id: "cons1",
                bindings: { alpha: "prod1", beta: "prod2" }
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
        let out = validate_bindings(&items);
        assert!(out.errors.is_empty(), "unexpected errors: {:?}", out.errors);
        let Some(SlotBinding::FromAnyBound {
            producer_instance_ids,
        }) = slot_binding(&out, "cons1", "extra")
        else {
            panic!(
                "expected FromAnyBound, got {:?}",
                slot_binding(&out, "cons1", "extra")
            );
        };
        let mut ids = producer_instance_ids;
        ids.sort();
        assert_eq!(ids, vec!["prod1".to_string(), "prod2".to_string()]);
    }

    /// Rule 3: a free-form key whose target's (name, tag) doesn't
    /// match any from_any slot is dead.
    #[test]
    fn rule3_free_form_key_without_matching_from_any_is_dead() {
        let cons_instances = parse_instances(
            r#"[{
                instance_id: "cons1",
                bindings: { the_extra: "lidar_inst" }
            }]"#,
        );
        let depends_on = parse_depends_on(
            r#"{
                nodes: [{ name: "camera", tag: "v1", link_id: "extra", from_any: true }]
            }"#,
        );
        let prod_instances = parse_instances(r#"[{ instance_id: "lidar_inst" }]"#);
        let items = vec![
            item("cons", "v1", &cons_instances, Some(&depends_on)),
            item("lidar", "v1", &prod_instances, None),
        ];
        let out = validate_bindings(&items);
        assert_eq!(out.errors.len(), 1);
        assert!(matches!(out.errors[0], ParsingError::BindingDeadKey(_)));
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
        let out = validate_bindings(&items);
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
        let out = validate_bindings(&items);
        assert_eq!(out.errors.len(), 1);
        let ParsingError::BindingMissingForPinnedDep(info) = &out.errors[0] else {
            panic!(
                "expected BindingMissingForPinnedDep, got {:?}",
                out.errors[0]
            );
        };
        assert_eq!(info.kind, "interfaces");
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
        let out = validate_bindings(&items);
        assert_eq!(out.errors.len(), 1);
        let ParsingError::BindingTargetMismatch(info) = &out.errors[0] else {
            panic!("expected BindingTargetMismatch, got {:?}", out.errors[0]);
        };
        assert_eq!(info.owner_instance_id, "cons1");
        assert_eq!(info.binding, "main");
        assert_eq!(info.target_instance_id, "actually_lidar");
    }

    /// Pinned interface dep targets a producer that does NOT declare
    /// `conforms_to` for the requested interface: rejected with
    /// `BindingInterfaceNotConformed`. This is the symmetric regression
    /// for the from_any path — pinned interface bindings used to skip
    /// the producer check entirely, silently accepting non-conforming
    /// producers.
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
        let out = validate_bindings(&items);
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
        let out = validate_bindings(&items);
        assert!(out.errors.is_empty(), "unexpected errors: {:?}", out.errors);
        assert_eq!(
            slot_binding(&out, "cons1", "depth"),
            Some(SlotBinding::Pinned {
                producer_instance_id: "webcam_inst_1".to_string()
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
                    the_extra_camera: "depth_cam_inst1"
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
        // Producer's node name `depth_camera:v1` coincidentally matches
        // the interface name, but the validator now only honors explicit
        // `conforms_to` claims — so the producer must declare it.
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
        let out = validate_bindings(&items);
        assert!(out.errors.is_empty(), "unexpected errors: {:?}", out.errors);
        assert_eq!(
            slot_binding(&out, "backbone_inst_1", "wrist_left_camera"),
            Some(SlotBinding::Pinned {
                producer_instance_id: "depth_cam_inst1".to_string()
            })
        );
        assert_eq!(
            slot_binding(&out, "backbone_inst_1", "wrist_right_camera"),
            Some(SlotBinding::Pinned {
                producer_instance_id: "depth_cam_inst1".to_string()
            })
        );
        assert_eq!(
            slot_binding(&out, "backbone_inst_1", "extra_cam"),
            Some(SlotBinding::FromAnyBound {
                producer_instance_ids: vec!["depth_cam_inst1".to_string()]
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
        let out = validate_bindings(&items);
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
        let out = validate_bindings(&items);
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

    /// Stack-wide check doesn't double-report intra-group duplicates
    /// (those are caught by the deserializer's
    /// `deserialize_instances`).
    #[test]
    fn rule7_does_not_double_report_intra_group_duplicates() {
        // Two instances under the same (name, tag) — would be rejected
        // by the deserializer in real parsing, but if they slip
        // through, this validator must not double-fire.
        let camera_instances = parse_instances(
            r#"[
                { instance_id: "inst_a" },
                { instance_id: "inst_b" }
            ]"#,
        );
        let items = vec![item("camera", "v1", &camera_instances, None)];
        let out = validate_bindings(&items);
        assert!(out.errors.is_empty(), "unexpected errors: {:?}", out.errors);
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
        let out = validate_bindings(&items);
        assert_eq!(
            out.errors.len(),
            2,
            "expected two errors, got {:?}",
            out.errors
        );
        // BindingDeadKey is emitted first (in iteration order),
        // BindingMissingForPinnedDep after.
        assert!(matches!(out.errors[0], ParsingError::BindingDeadKey(_)));
        assert!(matches!(
            out.errors[1],
            ParsingError::BindingMissingForPinnedDep(_)
        ));
    }

    /// A `from_any` interface dep accepts a producer whose
    /// `interfaces.conforms_to` includes the requested interface, even
    /// when the producer's node name differs from the interface name.
    /// This is the canonical positive case for the bug fix.
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
        let out = validate_bindings(&items);
        assert!(out.errors.is_empty(), "unexpected errors: {:?}", out.errors);
        assert_eq!(
            slot_binding(&out, "cons1", "extra_cam"),
            Some(SlotBinding::FromAnyBound {
                producer_instance_ids: vec!["webcam_inst_1".to_string()]
            })
        );
    }

    /// A `from_any` interface dep rejects a producer that lacks the
    /// matching `conforms_to`, even when its node name coincidentally
    /// equals the requested interface name+tag. Codifies the bug: a
    /// `depth_camera:v1` node that does not declare conformance must
    /// not satisfy a `depth_camera:v1` interface slot.
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
        // Producer's node identity matches the interface name+tag, but
        // it declares no `conforms_to`. Today's bug accepts this; the
        // fix rejects it.
        let items = vec![
            item("cons", "v1", &cons_instances, Some(&depends_on)),
            item("depth_camera", "v1", &prod_instances, None),
        ];
        let out = validate_bindings(&items);
        assert_eq!(out.errors.len(), 1, "errors: {:?}", out.errors);
        let ParsingError::BindingDeadKey(info) = &out.errors[0] else {
            panic!("expected BindingDeadKey, got {:?}", out.errors[0]);
        };
        assert_eq!(info.binding, "extra_cam");
        assert_eq!(info.target_instance_id, "depth_cam_inst_1");
        assert_eq!(info.producer_name_tag, "depth_camera:v1");
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
        let out = validate_bindings(&items);
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
        let out = validate_bindings(&items);
        assert!(out.errors.is_empty(), "unexpected errors: {:?}", out.errors);
        assert_eq!(
            slot_binding(&out, "depth_cons", "feed"),
            Some(SlotBinding::Pinned {
                producer_instance_id: "multi_prod".to_string()
            })
        );
        assert_eq!(
            slot_binding(&out, "uvc_cons", "feed"),
            Some(SlotBinding::Pinned {
                producer_instance_id: "multi_prod".to_string()
            })
        );
    }

    /// When a producer's `conforms_to` could match multiple from_any
    /// interface slots, the validator picks the first slot in
    /// `BTreeMap` (alphabetical by link_id) order and attaches the
    /// producer there. Other matching slots remain unbound. Documents
    /// the deterministic first-match-wins shadowing.
    #[test]
    fn from_any_multiple_slots_first_match_wins() {
        let cons_instances = parse_instances(
            r#"[{
                instance_id: "cons1",
                bindings: { whatever: "multi_prod" }
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
        let out = validate_bindings(&items);
        assert!(out.errors.is_empty(), "unexpected errors: {:?}", out.errors);
        // `slot_a` sorts before `slot_b`, so the producer lands on
        // slot_a; slot_b stays unbound.
        assert_eq!(
            slot_binding(&out, "cons1", "slot_a"),
            Some(SlotBinding::FromAnyBound {
                producer_instance_ids: vec!["multi_prod".to_string()]
            })
        );
        assert_eq!(
            slot_binding(&out, "cons1", "slot_b"),
            Some(SlotBinding::FromAnyUnbound)
        );
    }
}
