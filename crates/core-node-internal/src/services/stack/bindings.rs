//! Plan-phase validation for the launcher's per-instance `bindings`
//! field. Runs after node configs are loaded so the validator can
//! cross-reference the launcher's bindings against each consumer's
//! `depends_on` declarations and each binding target's deploying node.
//!
//! The producer's runtime `link_ids` are derived from the launcher's
//! bindings at launch time (see [`config::launcher::link_ids_by_instance_id`]
//! and [`crate::services::stack::launch::start_node_instances`]); the
//! checks here exist to turn the three classes of silent-failure
//! configurations into loud parse-time errors.

use config::consts::DEFAULT_LINK_ID_SENTINEL;
use config::launcher::DeploymentInstance;
use config::node::DependsOn;
use config::{BindingMissingForPinnedDep, BindingTargetMismatch, ParsingError};
use std::collections::BTreeMap;

/// Minimal view of one planned deployment needed for binding
/// validation. Built by the launcher with borrowed references to avoid
/// cloning the full `PlannedDeployment` graph; consumed by
/// [`validate_bindings`].
pub(super) struct BindingValidationItem<'a> {
    pub node_name: &'a str,
    pub node_tag: &'a str,
    pub instances: &'a [DeploymentInstance],
    pub depends_on: Option<&'a DependsOn>,
}

/// Three sub-checks per consumer instance:
///   1. Each `binding` key matches a `link_id` declared in the
///      consumer's `depends_on.{nodes,interfaces}` (dead-binding check).
///   2. Each pinned `depends_on` entry (`from_any: false` and a
///      non-default `link_id`) has a matching binding declared on the
///      consumer instance (no silent-loss check).
///   3. For node-typed pinned deps, the binding's target instance
///      deploys a node whose `(name, tag)` matches the dep declaration.
///
/// Interface-typed deps bypass check 3 because they do not pre-commit
/// to a producer node identity; verifying that the bound target
/// `exposes` the interface contract is left to a future hardening pass.
pub(super) fn validate_bindings(items: &[BindingValidationItem<'_>]) -> Vec<ParsingError> {
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

    errors
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
        use config::node::{Name as NodeName, NodeDependency};

        let instances = parse_instances(r#"[{ instance_id: "cons1" }]"#);
        let depends_on = DependsOn {
            nodes: vec![NodeDependency {
                name: NodeName::new("camera").expect("name should be valid"),
                tag: "v1".to_string(),
                link_id: "_".to_string(),
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
            ParsingError::BindingMissingForPinnedDep { .. }
        ));
    }
}
