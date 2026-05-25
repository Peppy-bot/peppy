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
    BindingDeadKey, BindingMissingForPinnedDep, BindingTargetMismatch,
    DuplicateInstanceIdAcrossStack, ParsingError,
};
use crate::node::DependsOn;
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

                if let Some(&(expected_name, expected_tag)) =
                    declared_pinned.get(binding_key.as_str())
                {
                    // Rule 2: KEY matches a declared pinned link_id.
                    pinned_keys_seen.insert(binding_key.as_str());
                    let is_interface =
                        declared_pinned_is_interface(item.depends_on, binding_key.as_str());
                    let Some(target_item) = instance_to_item.get(target_id.as_str()) else {
                        out.errors.push(ParsingError::UnknownInstanceId {
                            owner_instance_id: instance.instance_id.to_string(),
                            binding: binding_key.clone(),
                            instance_id: target_id.clone(),
                        });
                        continue;
                    };
                    // Interface deps don't pre-commit to a producer
                    // node identity (a `conforms_to` producer can be
                    // any node that exposes the interface), so the
                    // target-mismatch check is rule-5-scoped to node
                    // deps only.
                    if !is_interface
                        && (target_item.node_name != expected_name
                            || target_item.node_tag != expected_tag)
                    {
                        out.errors
                            .push(ParsingError::BindingTargetMismatch(Box::new(
                                BindingTargetMismatch {
                                    owner_instance_id: instance.instance_id.to_string(),
                                    binding: binding_key.clone(),
                                    target_instance_id: target_id.clone(),
                                    expected_name: expected_name.to_string(),
                                    expected_tag: expected_tag.to_string(),
                                    actual_name: target_item.node_name.to_string(),
                                    actual_tag: target_item.node_tag.to_string(),
                                },
                            )));
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
                // from_any slot for VALUE's (name, tag).
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
                for (slot_link_id, (name, tag)) in &declared_from_any {
                    if *name == target_item.node_name && *tag == target_item.node_tag {
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
            for (slot_link_id, (_name, _tag)) in &declared_from_any {
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
            for (slot_link_id, (name, tag)) in &declared_pinned {
                if resolved.contains_key(*slot_link_id) {
                    continue;
                }
                if pinned_keys_seen.contains(*slot_link_id) {
                    continue;
                }
                let kind = if declared_pinned_is_interface(item.depends_on, slot_link_id) {
                    "interfaces"
                } else {
                    "nodes"
                };
                out.errors
                    .push(ParsingError::BindingMissingForPinnedDep(Box::new(
                        BindingMissingForPinnedDep {
                            owner_instance_id: instance.instance_id.to_string(),
                            link_id: (*slot_link_id).to_string(),
                            kind: kind.to_string(),
                            expected_name: (*name).to_string(),
                            expected_tag: (*tag).to_string(),
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

type DeclaredSlots<'a> = BTreeMap<&'a str, (&'a str, &'a str)>;

/// Split declared `depends_on` entries into pinned (`from_any: false`)
/// and `from_any: true` slots. Each map is keyed by `link_id` and
/// values carry the dep's `(name, tag)` for downstream lookups.
fn collect_declared_slots(
    depends_on: Option<&DependsOn>,
) -> (DeclaredSlots<'_>, DeclaredSlots<'_>) {
    let mut pinned = BTreeMap::new();
    let mut from_any = BTreeMap::new();
    if let Some(deps) = depends_on {
        for dep in &deps.nodes {
            let entry = (dep.name.as_str(), dep.tag.as_str());
            if dep.from_any {
                from_any.insert(dep.link_id.as_str(), entry);
            } else {
                pinned.insert(dep.link_id.as_str(), entry);
            }
        }
        for dep in &deps.interfaces {
            let entry = (dep.name.as_str(), dep.tag.as_str());
            if dep.from_any {
                from_any.insert(dep.link_id.as_str(), entry);
            } else {
                pinned.insert(dep.link_id.as_str(), entry);
            }
        }
    }
    (pinned, from_any)
}

fn declared_pinned_is_interface(depends_on: Option<&DependsOn>, link_id: &str) -> bool {
    let Some(deps) = depends_on else {
        return false;
    };
    deps.interfaces.iter().any(|i| i.link_id == link_id)
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

    /// Pinned interface dep bound to an instance whose node deploys
    /// something else: binding target lookup succeeds without the
    /// node-identity check (interface deps don't pre-commit to a node
    /// identity).
    #[test]
    fn interface_binding_skips_target_node_check() {
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
        assert!(out.errors.is_empty(), "unexpected errors: {:?}", out.errors);
        // Interface deps resolve as Pinned with no node-identity check.
        assert_eq!(
            slot_binding(&out, "cons1", "depth"),
            Some(SlotBinding::Pinned {
                producer_instance_id: "any_producer".to_string()
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
        let items = vec![
            item(
                "openarm01_backbone",
                "v1",
                &cons_instances,
                Some(&depends_on),
            ),
            item("depth_camera", "v1", &prod_instances, None),
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
}
