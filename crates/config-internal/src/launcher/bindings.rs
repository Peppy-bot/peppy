//! Plan-phase validation for the launcher's per-instance `bindings`
//! field. Runs after node configs are loaded so the validator can
//! cross-reference the launcher's bindings against each consumer's
//! `depends_on` declarations and each binding target's deploying node.
//!
//! In the harmonized wire model, producers always advertise the `_`
//! link_id sentinel; consumers pin a specific producer by
//! `from_instance_id` derived from this `bindings` map. The checks here
//! exist to turn three classes of silent-failure configurations into
//! loud parse-time errors.

use crate::consts::DEFAULT_LINK_ID_SENTINEL;
use crate::error::{
    BindingMissingForPinnedDep, BindingTargetMismatch, DuplicateConsumerPin, ParsingError,
};
use crate::node::DependsOn;
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

/// Four sub-checks per consumer instance (1–3) or producer group (4):
///   1. Each `binding` key matches a `link_id` declared in the
///      consumer's `depends_on.{nodes,interfaces}` (dead-binding check).
///   2. Each pinned `depends_on` entry (`from_any: false` and a
///      non-default `link_id`) has a matching binding declared on the
///      consumer instance (no silent-loss check).
///   3. For node-typed pinned deps, the binding's target instance
///      deploys a node whose `(name, tag)` matches the dep declaration.
///   4. No two instances of the same `(node_name, node_tag)` end up
///      advertising the same producer `link_id`. A producer `link_id`
///      is a 1:1 contract from the consumer's perspective, so two
///      sibling instances claiming it would silently multiplex the wire.
///
/// Interface-typed deps bypass check 3 because they do not pre-commit
/// to a producer node identity; verifying that the bound target
/// `exposes` the interface contract is left to a future hardening pass.
pub fn validate_bindings(items: &[BindingValidationItem<'_>]) -> Vec<ParsingError> {
    let instance_to_item = build_instance_lookup(items);

    let mut errors: Vec<ParsingError> = Vec::new();
    for item in items {
        let (declared_node_deps, declared_interface_deps) = collect_declared_deps(item.depends_on);
        let declared_csv = format_declared_keys(&declared_node_deps, &declared_interface_deps);

        for instance in item.instances {
            check_dead_keys_and_target_mismatch(
                instance,
                &declared_node_deps,
                &declared_interface_deps,
                &declared_csv,
                &instance_to_item,
                &mut errors,
            );
            check_missing_pinned_bindings(instance, item.depends_on, &mut errors);
        }
    }

    check_duplicate_producer_link_ids(items, &instance_to_item, &mut errors);

    errors
}

/// Group all planned instances by `(node_name, node_tag)`, derive each
/// instance's producer-side link_ids by inverting every consumer's
/// `bindings` map, and emit a `DuplicateConsumerPin` for every pair
/// of sibling instances that end up claiming the same `link_id`.
///
/// Why this matters: at runtime `prepare_and_spawn` enforces the same
/// invariant (see `NodeEntity::prepare_and_spawn` in node-stack), but
/// the launcher spawns instances sequentially. Without a parse-time
/// check, a colliding launcher manifest would partially deploy — first
/// instance wins, second one fails mid-flight — and leave the operator
/// to clean up. We catch it before any spawn side-effect.
fn check_duplicate_producer_link_ids(
    items: &[BindingValidationItem<'_>],
    instance_to_item: &BTreeMap<&str, &BindingValidationItem<'_>>,
    errors: &mut Vec<ParsingError>,
) {
    // Per (node_name, node_tag) group: producer_instance_id -> link_ids
    // the consumers have asked that producer to advertise.
    let mut derived: BTreeMap<(&str, &str), BTreeMap<&str, Vec<&str>>> = BTreeMap::new();
    // Register every declared instance up front so unbound producers
    // still appear and trivially pass the duplicate check.
    for item in items {
        let group = derived.entry((item.node_name, item.node_tag)).or_default();
        for inst in item.instances {
            group.entry(inst.instance_id.as_str()).or_default();
        }
    }
    // Walk every consumer binding and attribute each link_id to its
    // target producer (scoped to that producer's node group).
    for item in items {
        for inst in item.instances {
            for (link_id, target_id) in &inst.bindings {
                let Some(target_item) = instance_to_item.get(target_id.as_str()) else {
                    // Already reported as UnknownInstanceId.
                    continue;
                };
                if let Some(group) = derived.get_mut(&(target_item.node_name, target_item.node_tag))
                    && let Some(link_ids) = group.get_mut(target_id.as_str())
                    && !link_ids.contains(&link_id.as_str())
                {
                    link_ids.push(link_id.as_str());
                }
            }
        }
    }
    // For each group, invert producer -> link_ids into link_id ->
    // producers; emit one error per (group, link_id) pair with more
    // than one producer. Pairs are sorted so error ordering is
    // deterministic across runs.
    for ((node_name, node_tag), producers) in &derived {
        let mut owners: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
        for (producer_id, link_ids) in producers {
            for link_id in link_ids {
                owners.entry(*link_id).or_default().push(*producer_id);
            }
        }
        for (link_id, mut producer_ids) in owners {
            if producer_ids.len() < 2 {
                continue;
            }
            producer_ids.sort();
            for (i, a) in producer_ids.iter().enumerate() {
                for b in &producer_ids[i + 1..] {
                    errors.push(ParsingError::DuplicateConsumerPin(Box::new(
                        DuplicateConsumerPin {
                            node_name: (*node_name).to_string(),
                            node_tag: (*node_tag).to_string(),
                            link_id: link_id.to_string(),
                            instance_a: (*a).to_string(),
                            instance_b: (*b).to_string(),
                        },
                    )));
                }
            }
        }
    }
}

fn build_instance_lookup<'a>(
    items: &'a [BindingValidationItem<'a>],
) -> BTreeMap<&'a str, &'a BindingValidationItem<'a>> {
    let mut lookup = BTreeMap::new();
    for item in items {
        for instance in item.instances {
            lookup.insert(instance.instance_id.as_str(), item);
        }
    }
    lookup
}

/// `link_id` → producer `(name, tag)` lookup, populated from a
/// consumer's `depends_on.nodes` or `depends_on.interfaces` list.
type DeclaredDeps<'a> = BTreeMap<&'a str, (&'a str, &'a str)>;

fn collect_declared_deps(depends_on: Option<&DependsOn>) -> (DeclaredDeps<'_>, DeclaredDeps<'_>) {
    let mut nodes = BTreeMap::new();
    let mut interfaces = BTreeMap::new();
    if let Some(deps) = depends_on {
        for dep in &deps.nodes {
            nodes.insert(dep.link_id.as_str(), (dep.name.as_str(), dep.tag.as_str()));
        }
        for dep in &deps.interfaces {
            interfaces.insert(dep.link_id.as_str(), (dep.name.as_str(), dep.tag.as_str()));
        }
    }
    (nodes, interfaces)
}

fn format_declared_keys(nodes: &DeclaredDeps<'_>, interfaces: &DeclaredDeps<'_>) -> String {
    let mut keys: Vec<&str> = nodes.keys().chain(interfaces.keys()).copied().collect();
    keys.sort();
    keys.dedup();
    keys.join(", ")
}

fn check_dead_keys_and_target_mismatch(
    instance: &DeploymentInstance,
    declared_node_deps: &DeclaredDeps<'_>,
    declared_interface_deps: &DeclaredDeps<'_>,
    declared_csv: &str,
    instance_to_item: &BTreeMap<&str, &BindingValidationItem<'_>>,
    errors: &mut Vec<ParsingError>,
) {
    for (binding_key, target_id) in &instance.bindings {
        let in_nodes = declared_node_deps.get(binding_key.as_str());
        let in_interfaces = declared_interface_deps.contains_key(binding_key.as_str());
        if in_nodes.is_none() && !in_interfaces {
            errors.push(ParsingError::BindingDeadKey {
                owner_instance_id: instance.instance_id.to_string(),
                binding: binding_key.clone(),
                declared_link_ids: declared_csv.to_string(),
            });
            continue;
        }

        let Some(&(expected_name, expected_tag)) = in_nodes else {
            continue;
        };
        let Some(target_item) = instance_to_item.get(target_id.as_str()) else {
            // The launcher-level deserializer already rejects bindings
            // whose target_instance_id is not declared elsewhere in the
            // launcher. If a target nonetheless fails to resolve at the
            // plan phase (e.g., the deployment that owned the target
            // was dropped between parsing and planning), fail loudly
            // here rather than silently passing the pinned dep through
            // to runtime.
            errors.push(ParsingError::UnknownInstanceId {
                owner_instance_id: instance.instance_id.to_string(),
                binding: binding_key.clone(),
                instance_id: target_id.clone(),
            });
            continue;
        };
        if target_item.node_name != expected_name || target_item.node_tag != expected_tag {
            errors.push(ParsingError::BindingTargetMismatch(Box::new(
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
        }
    }
}

fn check_missing_pinned_bindings(
    instance: &DeploymentInstance,
    depends_on: Option<&DependsOn>,
    errors: &mut Vec<ParsingError>,
) {
    let Some(deps) = depends_on else {
        return;
    };
    for dep in &deps.nodes {
        if requires_binding(dep.from_any, &dep.link_id)
            && !instance.bindings.contains_key(dep.link_id.as_str())
        {
            errors.push(ParsingError::BindingMissingForPinnedDep(Box::new(
                BindingMissingForPinnedDep {
                    owner_instance_id: instance.instance_id.to_string(),
                    link_id: dep.link_id.clone(),
                    kind: "nodes".to_string(),
                    expected_name: dep.name.as_str().to_string(),
                    expected_tag: dep.tag.clone(),
                },
            )));
        }
    }
    for dep in &deps.interfaces {
        if requires_binding(dep.from_any, &dep.link_id)
            && !instance.bindings.contains_key(dep.link_id.as_str())
        {
            errors.push(ParsingError::BindingMissingForPinnedDep(Box::new(
                BindingMissingForPinnedDep {
                    owner_instance_id: instance.instance_id.to_string(),
                    link_id: dep.link_id.clone(),
                    kind: "interfaces".to_string(),
                    expected_name: dep.name.as_str().to_string(),
                    expected_tag: dep.tag.clone(),
                },
            )));
        }
    }
}

/// A pinned `depends_on` entry requires a launcher binding only when
/// it commits to a non-default `link_id`. The reserved sentinel matches
/// the producer's natural fallback so no binding is needed.
fn requires_binding(from_any: bool, link_id: &str) -> bool {
    !from_any && link_id != DEFAULT_LINK_ID_SENTINEL
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

    #[test]
    fn empty_planned_set_returns_no_errors() {
        let errors = validate_bindings(&[]);
        assert!(errors.is_empty());
    }

    /// A consumer with no `depends_on` and no `bindings` is trivially
    /// valid.
    #[test]
    fn consumer_without_depends_on_and_without_bindings_is_valid() {
        let instances = parse_instances(r#"[{ instance_id: "cons1" }]"#);
        let items = vec![item("cons", "v1", &instances, None)];
        let errors = validate_bindings(&items);
        assert!(errors.is_empty(), "unexpected errors: {errors:?}");
    }

    /// (i) A binding whose key isn't declared in `depends_on` surfaces
    /// as `BindingDeadKey` with the consumer's full declared-link_ids
    /// list for context. The pinned `main` slot is bound here so that
    /// the dead-key error is the only one in play.
    #[test]
    fn rejects_dead_binding_key() {
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
        let errors = validate_bindings(&items);
        assert_eq!(errors.len(), 1, "expected one error, got {errors:?}");
        let ParsingError::BindingDeadKey {
            owner_instance_id,
            binding,
            declared_link_ids,
        } = &errors[0]
        else {
            panic!("expected BindingDeadKey, got {:?}", errors[0]);
        };
        assert_eq!(owner_instance_id, "cons1");
        assert_eq!(binding, "stale_slot");
        assert_eq!(declared_link_ids, "main");
    }

    /// (ii) A consumer with a pinned (non-`from_any`, non-`_`)
    /// `depends_on` entry but no matching binding is the exact
    /// silent-loss case the issue flags.
    #[test]
    fn rejects_missing_binding_for_pinned_node_dep() {
        let instances = parse_instances(r#"[{ instance_id: "cons1" }]"#);
        let depends_on = parse_depends_on(
            r#"{
                nodes: [{ name: "camera", tag: "v1", link_id: "main" }]
            }"#,
        );
        let items = vec![item("cons", "v1", &instances, Some(&depends_on))];
        let errors = validate_bindings(&items);
        assert_eq!(errors.len(), 1);
        let ParsingError::BindingMissingForPinnedDep(info) = &errors[0] else {
            panic!("expected BindingMissingForPinnedDep, got {:?}", errors[0]);
        };
        assert_eq!(info.owner_instance_id, "cons1");
        assert_eq!(info.link_id, "main");
        assert_eq!(info.kind, "nodes");
        assert_eq!(info.expected_name, "camera");
        assert_eq!(info.expected_tag, "v1");
    }

    /// A `depends_on.link_id == "_"` is the explicit "use the producer
    /// default" opt-in and requires no binding. The deserializer
    /// currently rejects `_` as a NodeDependency `link_id` so this
    /// state is unreachable through parsing; the test constructs the
    /// struct programmatically to exercise the defensive skip and
    /// guard against future deserializer loosening.
    #[test]
    fn skips_missing_when_link_id_is_underscore() {
        use crate::node::{Name as NodeName, NodeDependency};

        let instances = parse_instances(r#"[{ instance_id: "cons1" }]"#);
        let depends_on = DependsOn {
            nodes: vec![NodeDependency {
                name: NodeName::new("camera").expect("name should be valid"),
                tag: "v1".to_string(),
                link_id: DEFAULT_LINK_ID_SENTINEL.to_string(),
                from_any: false,
            }],
            interfaces: vec![],
        };
        let items = vec![item("cons", "v1", &instances, Some(&depends_on))];
        let errors = validate_bindings(&items);
        assert!(errors.is_empty(), "unexpected errors: {errors:?}");
    }

    /// A `from_any: true` consumer subscribes via wildcard and doesn't
    /// need a binding.
    #[test]
    fn skips_missing_when_from_any() {
        let instances = parse_instances(r#"[{ instance_id: "cons1" }]"#);
        let depends_on = parse_depends_on(
            r#"{
                nodes: [{ name: "camera", tag: "v1", link_id: "main", from_any: true }]
            }"#,
        );
        let items = vec![item("cons", "v1", &instances, Some(&depends_on))];
        let errors = validate_bindings(&items);
        assert!(errors.is_empty(), "unexpected errors: {errors:?}");
    }

    /// (iii) Binding target points at an instance whose deploying node
    /// doesn't match the consumer's `depends_on.nodes` entry.
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
        let errors = validate_bindings(&items);
        assert_eq!(errors.len(), 1, "expected one error, got {errors:?}");
        let ParsingError::BindingTargetMismatch(info) = &errors[0] else {
            panic!("expected BindingTargetMismatch, got {:?}", errors[0]);
        };
        assert_eq!(info.owner_instance_id, "cons1");
        assert_eq!(info.binding, "main");
        assert_eq!(info.target_instance_id, "actually_lidar");
        assert_eq!(info.expected_name, "camera");
        assert_eq!(info.expected_tag, "v1");
        assert_eq!(info.actual_name, "lidar");
        assert_eq!(info.actual_tag, "v1");
    }

    /// A `from_any: true` consumer with a binding is permissive: the
    /// binding is honored for producer-side wiring (other pinned
    /// consumers can rely on it) and the wildcard consumer simply
    /// ignores the link_id pin.
    #[test]
    fn allows_binding_on_from_any_dep() {
        let cons_instances = parse_instances(
            r#"[{
                instance_id: "cons1",
                bindings: { main: "prod1" }
            }]"#,
        );
        let depends_on = parse_depends_on(
            r#"{
                nodes: [{ name: "camera", tag: "v1", link_id: "main", from_any: true }]
            }"#,
        );
        let prod_instances = parse_instances(r#"[{ instance_id: "prod1" }]"#);
        let items = vec![
            item("cons", "v1", &cons_instances, Some(&depends_on)),
            item("camera", "v1", &prod_instances, None),
        ];
        let errors = validate_bindings(&items);
        assert!(errors.is_empty(), "unexpected errors: {errors:?}");
    }

    /// Interface deps are equally bindable: missing binding on a
    /// pinned interface dep is still a silent-loss bug.
    #[test]
    fn rejects_missing_binding_for_pinned_interface_dep() {
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
        let errors = validate_bindings(&items);
        assert_eq!(errors.len(), 1);
        let ParsingError::BindingMissingForPinnedDep(info) = &errors[0] else {
            panic!("expected BindingMissingForPinnedDep, got {:?}", errors[0]);
        };
        assert_eq!(info.kind, "interfaces");
        assert_eq!(info.link_id, "depth");
    }

    /// A binding key matching an interface dep does NOT trigger the
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
        let errors = validate_bindings(&items);
        assert!(errors.is_empty(), "unexpected errors: {errors:?}");
    }

    /// A complete, well-formed launcher passes with zero errors.
    #[test]
    fn well_formed_launcher_passes() {
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
        let errors = validate_bindings(&items);
        assert!(errors.is_empty(), "unexpected errors: {errors:?}");
    }

    /// A binding pointing at an `instance_id` that the planner did not
    /// produce (defensive case: launcher-level dedup should already
    /// catch this) surfaces as an `UnknownInstanceId` error instead of
    /// silently passing the pinned dep through to runtime.
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
        let errors = validate_bindings(&items);
        assert_eq!(errors.len(), 1, "expected one error, got {errors:?}");
        let ParsingError::UnknownInstanceId {
            owner_instance_id,
            binding,
            instance_id,
        } = &errors[0]
        else {
            panic!("expected UnknownInstanceId, got {:?}", errors[0]);
        };
        assert_eq!(owner_instance_id, "cons1");
        assert_eq!(binding, "main");
        assert_eq!(instance_id, "ghost_producer");
    }

    /// Two consumers each bind `main` to a different instance of the
    /// same `camera:v1` producer node — both producer instances would
    /// advertise `main` on the wire, which violates the 1:1 link_id
    /// contract. Surfaces as a single `DuplicateConsumerPin` naming
    /// both colliding producer instances.
    #[test]
    fn rejects_two_producer_instances_claiming_same_link_id() {
        let cons_instances = parse_instances(
            r#"[
                { instance_id: "cons_a", bindings: { main: "prod_a" } },
                { instance_id: "cons_b", bindings: { main: "prod_b" } }
            ]"#,
        );
        let depends_on = parse_depends_on(
            r#"{
                nodes: [{ name: "camera", tag: "v1", link_id: "main" }]
            }"#,
        );
        let prod_instances = parse_instances(
            r#"[
                { instance_id: "prod_a" },
                { instance_id: "prod_b" }
            ]"#,
        );
        let items = vec![
            item("cons", "v1", &cons_instances, Some(&depends_on)),
            item("camera", "v1", &prod_instances, None),
        ];
        let errors = validate_bindings(&items);
        assert_eq!(errors.len(), 1, "expected one error, got {errors:?}");
        let ParsingError::DuplicateConsumerPin(info) = &errors[0] else {
            panic!("expected DuplicateConsumerPin, got {:?}", errors[0]);
        };
        assert_eq!(info.node_name, "camera");
        assert_eq!(info.node_tag, "v1");
        assert_eq!(info.link_id, "main");
        assert_eq!(info.instance_a, "prod_a");
        assert_eq!(info.instance_b, "prod_b");
    }

    /// Producers of *different* nodes may share a `link_id` — the
    /// uniqueness contract is scoped to `(node_name, node_tag)`.
    #[test]
    fn allows_same_link_id_across_different_node_types() {
        let cons_instances = parse_instances(
            r#"[{
                instance_id: "cons1",
                bindings: { camera_main: "cam_prod", lidar_main: "lidar_prod" }
            }]"#,
        );
        let depends_on = parse_depends_on(
            r#"{
                nodes: [
                    { name: "camera", tag: "v1", link_id: "camera_main" },
                    { name: "lidar",  tag: "v1", link_id: "lidar_main" }
                ]
            }"#,
        );
        let cam_instances = parse_instances(r#"[{ instance_id: "cam_prod" }]"#);
        let lidar_instances = parse_instances(r#"[{ instance_id: "lidar_prod" }]"#);
        let items = vec![
            item("cons", "v1", &cons_instances, Some(&depends_on)),
            item("camera", "v1", &cam_instances, None),
            item("lidar", "v1", &lidar_instances, None),
        ];
        let errors = validate_bindings(&items);
        assert!(errors.is_empty(), "unexpected errors: {errors:?}");
    }

    /// Fan-in is fine: multiple consumers binding the same `link_id`
    /// to a *single* producer instance is the normal pattern.
    #[test]
    fn allows_multiple_consumers_binding_same_link_id_to_one_producer() {
        let cons_instances = parse_instances(
            r#"[
                { instance_id: "cons_a", bindings: { main: "prod1" } },
                { instance_id: "cons_b", bindings: { main: "prod1" } }
            ]"#,
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
        let errors = validate_bindings(&items);
        assert!(errors.is_empty(), "unexpected errors: {errors:?}");
    }

    /// Three sibling producer instances all claiming the same link_id
    /// surface as 3 pairwise errors (C(3,2)) in alphabetical instance
    /// order — proves the pairing is deterministic.
    #[test]
    fn three_way_collision_emits_pairwise_errors() {
        let cons_instances = parse_instances(
            r#"[
                { instance_id: "cons_a", bindings: { main: "prod_a" } },
                { instance_id: "cons_b", bindings: { main: "prod_b" } },
                { instance_id: "cons_c", bindings: { main: "prod_c" } }
            ]"#,
        );
        let depends_on = parse_depends_on(
            r#"{
                nodes: [{ name: "camera", tag: "v1", link_id: "main" }]
            }"#,
        );
        let prod_instances = parse_instances(
            r#"[
                { instance_id: "prod_a" },
                { instance_id: "prod_b" },
                { instance_id: "prod_c" }
            ]"#,
        );
        let items = vec![
            item("cons", "v1", &cons_instances, Some(&depends_on)),
            item("camera", "v1", &prod_instances, None),
        ];
        let errors = validate_bindings(&items);
        assert_eq!(errors.len(), 3, "expected 3 pairwise errors: {errors:?}");
        let pairs: Vec<(String, String)> = errors
            .iter()
            .map(|e| match e {
                ParsingError::DuplicateConsumerPin(info) => {
                    (info.instance_a.clone(), info.instance_b.clone())
                }
                other => panic!("expected DuplicateConsumerPin, got {other:?}"),
            })
            .collect();
        assert_eq!(
            pairs,
            vec![
                ("prod_a".to_string(), "prod_b".to_string()),
                ("prod_a".to_string(), "prod_c".to_string()),
                ("prod_b".to_string(), "prod_c".to_string()),
            ]
        );
    }

    /// All three sub-checks are aggregated: a single consumer triggers
    /// dead-key + missing-pinned in one pass; the validator does not
    /// short-circuit on the first error.
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
        let errors = validate_bindings(&items);
        assert_eq!(errors.len(), 2, "expected two errors, got {errors:?}");
        assert!(matches!(errors[0], ParsingError::BindingDeadKey { .. }));
        assert!(matches!(
            errors[1],
            ParsingError::BindingMissingForPinnedDep(_)
        ));
    }
}
