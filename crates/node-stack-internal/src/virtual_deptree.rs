//! Transient, in-memory dependency tree built from a set of locally-discovered
//! nodes (typically from a filesystem walk).
//!
//! Used by `peppy node sync -a` to determine the order in which nodes must be
//! synced so that each node's dependencies are already known by the time the
//! daemon tries to resolve their interfaces. Unlike the persistent
//! [`crate::NodeStack`], a `VirtualDeptree` is built from on-disk
//! `peppy.json5` files, lives only for the duration of the operation, and is
//! discarded immediately after; it never mutates the daemon's real node
//! stack.
//!
//! Edges in the underlying graph go from a *dependency* to its *dependant*, so
//! `petgraph::algo::toposort` returns a slice with all dependencies before any
//! of their dependants.

use std::collections::HashMap;
use std::path::PathBuf;

use config::node::NodeConfig;
use petgraph::algo::toposort;
use petgraph::stable_graph::{NodeIndex, StableDiGraph};

use crate::error::{Error, Result};
use crate::service_action_cycle::{CycleCheckNode, find_service_action_cycle};

/// `(name, tag)` identifier used to address a node in the virtual dep tree.
pub type NodeKey = (String, String);

/// One node sitting in the virtual dependency tree.
#[derive(Debug, Clone)]
pub struct VirtualNodeInfo {
    pub root_dir: PathBuf,
    pub config: NodeConfig,
}

impl VirtualNodeInfo {
    pub fn key(&self) -> NodeKey {
        (
            self.config.manifest.name.as_str().to_owned(),
            self.config.manifest.tag.clone(),
        )
    }
}

/// Transient dependency graph for a batch of locally-discovered nodes.
///
/// Construction validates uniqueness (no two inputs may share the same
/// `name:tag`) and freedom from cycles. `depends_on` entries that don't match
/// any node in the input set are silently ignored; those are external
/// dependencies that the daemon will resolve via its persistent node stack.
#[derive(Debug)]
pub struct VirtualDeptree {
    infos: HashMap<NodeIndex, VirtualNodeInfo>,
    /// The nodes of `infos`, every dependency before its dependants.
    order: Vec<NodeIndex>,
}

impl VirtualDeptree {
    /// Builds a virtual dep tree from a list of `(root_dir, NodeConfig)` pairs.
    ///
    /// Returns:
    /// - [`Error::DuplicateLocalNode`] when two inputs share the same
    ///   `name:tag`.
    /// - [`Error::VirtualDeptreeCycle`] when the resulting graph is not a DAG.
    pub fn build(nodes: Vec<(PathBuf, NodeConfig)>) -> Result<Self> {
        let mut graph: StableDiGraph<NodeKey, ()> = StableDiGraph::new();
        let mut by_key: HashMap<NodeKey, NodeIndex> = HashMap::with_capacity(nodes.len());
        let mut infos: HashMap<NodeIndex, VirtualNodeInfo> = HashMap::with_capacity(nodes.len());

        // First pass: register every input node and detect duplicates.
        for (root_dir, config) in nodes {
            let info = VirtualNodeInfo { root_dir, config };
            let key = info.key();

            if let Some(first) = by_key.get(&key) {
                return Err(Error::DuplicateLocalNode {
                    name: key.0,
                    tag: key.1,
                    first: infos[first].root_dir.clone(),
                    second: info.root_dir,
                });
            }

            let idx = graph.add_node(key.clone());
            by_key.insert(key, idx);
            infos.insert(idx, info);
        }

        // Second pass: add edges from each declared dependency that is also
        // present in the input set. Unknown deps are skipped here; the daemon
        // will resolve them against its persistent node stack at sync time.
        for idx in graph.node_indices().collect::<Vec<_>>() {
            let info = &infos[&idx];
            let Some(depends_on) = info.config.manifest.depends_on.as_ref() else {
                continue;
            };
            for dep in &depends_on.nodes {
                let dep_key = (dep.name.as_str().to_owned(), dep.tag.clone());
                if let Some(dep_idx) = by_key.get(&dep_key) {
                    // Edge: dep -> dependant. Toposort returns a slice in
                    // dependency order (deps first).
                    graph.add_edge(*dep_idx, idx, ());
                }
            }
        }

        // The graph is a DAG exactly when it has a topological order.
        let order = match toposort(&graph, None) {
            Ok(order) => order,
            Err(cycle) => {
                let (name, tag) = &graph[cycle.node_id()];
                return Err(Error::VirtualDeptreeCycle {
                    nodes: vec![format!("{name}:{tag}")],
                });
            }
        };

        // Contract deps were dropped from the node-dep graph above, so a
        // service/action cycle routed through contracts survives the toposort.
        // Reject it explicitly: caller-driven dependencies must stay acyclic.
        let mut cycle_nodes: Vec<CycleCheckNode<'_>> = infos
            .values()
            .map(|info| CycleCheckNode {
                name: info.config.manifest.name.as_str(),
                tag: info.config.manifest.tag.as_str(),
                config: &info.config,
            })
            .collect();
        // `infos` is a `HashMap`, so its iteration order is nondeterministic.
        // Sort by `(name, tag)` so the node-index order handed to the finder
        // (and thus which SCC it reports when several exist) is stable.
        cycle_nodes.sort_by(|a, b| (a.name, a.tag).cmp(&(b.name, b.tag)));
        if let Some(cycle) = find_service_action_cycle(&cycle_nodes) {
            return Err(Error::ServiceActionContractCycle {
                nodes: cycle.nodes,
                closing_dependency: cycle.closing_dependency,
                kind: cycle.kind.to_string(),
            });
        }

        Ok(Self { infos, order })
    }

    /// Returns the dependency-ordered slice of node infos. Dependencies are
    /// guaranteed to appear before any of their dependants.
    pub fn topological_order(&self) -> Vec<&VirtualNodeInfo> {
        self.order.iter().map(|idx| &self.infos[idx]).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::service_action_cycle::test_support::NodeSpec;
    use config::node::NodeConfigParser;
    use std::path::Path;
    use tempfile::TempDir;

    fn write_node(dir: &Path, name: &str, deps: &[(&str, &str)]) -> NodeConfig {
        std::fs::create_dir_all(dir).unwrap();
        let depends_on = if deps.is_empty() {
            String::new()
        } else {
            let entries = deps
                .iter()
                .map(|(n, t)| format!(r#"{{ name: "{n}", tag: "{t}", link_id: "{n}" }}"#))
                .collect::<Vec<_>>()
                .join(", ");
            format!("depends_on: {{ nodes: [{entries}] }},")
        };
        let json5 = format!(
            r#"{{
                peppy_schema: "node/v1",
                manifest: {{
                    name: "{name}",
                    tag: "v1",
                    description: ["A test node."],
                    {depends_on}
                }},
                interfaces: {{}},
                execution: {{ language: "rust", run_cmd: ["./bin"] }}
            }}"#
        );
        let path = dir.join(config::consts::NODE_CONFIG_FILE);
        std::fs::write(&path, json5).unwrap();
        NodeConfigParser::from_path(&path).unwrap()
    }

    fn key_of(info: &VirtualNodeInfo) -> NodeKey {
        info.key()
    }

    #[test]
    fn build_empty_returns_empty_order() {
        let tree = VirtualDeptree::build(vec![]).unwrap();
        assert!(tree.topological_order().is_empty());
    }

    #[test]
    fn build_two_independent_nodes_includes_both() {
        let tmp = TempDir::new().unwrap();
        let a_dir = tmp.path().join("a");
        let b_dir = tmp.path().join("b");
        let a = write_node(&a_dir, "a", &[]);
        let b = write_node(&b_dir, "b", &[]);

        let tree = VirtualDeptree::build(vec![(a_dir, a), (b_dir, b)]).unwrap();
        assert_eq!(tree.topological_order().len(), 2);
        let order = tree.topological_order();
        assert_eq!(order.len(), 2);
        let names: Vec<String> = order.iter().map(|n| key_of(n).0).collect();
        assert!(names.contains(&"a".to_string()));
        assert!(names.contains(&"b".to_string()));
    }

    #[test]
    fn build_chain_a_b_c_orders_a_first() {
        let tmp = TempDir::new().unwrap();
        let a_dir = tmp.path().join("a");
        let b_dir = tmp.path().join("b");
        let c_dir = tmp.path().join("c");
        let a = write_node(&a_dir, "a", &[]);
        let b = write_node(&b_dir, "b", &[("a", "v1")]);
        let c = write_node(&c_dir, "c", &[("b", "v1")]);

        let tree = VirtualDeptree::build(vec![
            (c_dir.clone(), c),
            (a_dir.clone(), a),
            (b_dir.clone(), b),
        ])
        .unwrap();
        let order: Vec<String> = tree
            .topological_order()
            .iter()
            .map(|n| key_of(n).0)
            .collect();
        assert_eq!(
            order,
            vec!["a".to_string(), "b".to_string(), "c".to_string()]
        );
    }

    #[test]
    fn build_diamond_orders_root_first_leaf_last() {
        let tmp = TempDir::new().unwrap();
        let a = write_node(&tmp.path().join("a"), "a", &[]);
        let b = write_node(&tmp.path().join("b"), "b", &[("a", "v1")]);
        let c = write_node(&tmp.path().join("c"), "c", &[("a", "v1")]);
        let d = write_node(&tmp.path().join("d"), "d", &[("b", "v1"), ("c", "v1")]);

        let tree = VirtualDeptree::build(vec![
            (tmp.path().join("d"), d),
            (tmp.path().join("c"), c),
            (tmp.path().join("b"), b),
            (tmp.path().join("a"), a),
        ])
        .unwrap();
        let order: Vec<String> = tree
            .topological_order()
            .iter()
            .map(|n| key_of(n).0)
            .collect();
        assert_eq!(order.first().unwrap(), "a");
        assert_eq!(order.last().unwrap(), "d");
        // b and c can be in either order in between
        let middle: Vec<&String> = order.iter().skip(1).take(2).collect();
        assert!(middle.iter().any(|s| s.as_str() == "b"));
        assert!(middle.iter().any(|s| s.as_str() == "c"));
    }

    #[test]
    fn build_detects_two_node_cycle() {
        let tmp = TempDir::new().unwrap();
        let a = write_node(&tmp.path().join("a"), "a", &[("b", "v1")]);
        let b = write_node(&tmp.path().join("b"), "b", &[("a", "v1")]);

        let result =
            VirtualDeptree::build(vec![(tmp.path().join("a"), a), (tmp.path().join("b"), b)]);
        match result {
            Err(Error::VirtualDeptreeCycle { .. }) => {}
            other => panic!("expected VirtualDeptreeCycle, got {:?}", other),
        }
    }

    #[test]
    fn build_ignores_external_dep() {
        let tmp = TempDir::new().unwrap();
        let a_dir = tmp.path().join("a");
        let a = write_node(&a_dir, "a", &[("external_x", "v999")]);

        let tree = VirtualDeptree::build(vec![(a_dir, a)]).unwrap();
        assert_eq!(tree.topological_order().len(), 1);
        let order = tree.topological_order();
        assert_eq!(order.len(), 1);
        assert_eq!(key_of(order[0]).0, "a");
    }

    #[test]
    fn build_rejects_duplicate_name_tag() {
        let tmp = TempDir::new().unwrap();
        let first_dir = tmp.path().join("first");
        let second_dir = tmp.path().join("second");
        let a1 = write_node(&first_dir, "shared", &[]);
        let a2 = write_node(&second_dir, "shared", &[]);

        let result = VirtualDeptree::build(vec![(first_dir.clone(), a1), (second_dir.clone(), a2)]);
        match result {
            Err(Error::DuplicateLocalNode {
                name,
                tag,
                first,
                second,
            }) => {
                assert_eq!(name, "shared");
                assert_eq!(tag, "v1");
                assert_eq!(first, first_dir);
                assert_eq!(second, second_dir);
            }
            other => panic!("expected DuplicateLocalNode, got {:?}", other),
        }
    }

    #[test]
    fn build_detects_mutual_service_contract_cycle() {
        let a = NodeSpec::new("a")
            .implements("iface_a", "v1")
            .contract_dep("iface_b", "v1", "to_b")
            .consumes_service("to_b")
            .build();
        let b = NodeSpec::new("b")
            .implements("iface_b", "v1")
            .contract_dep("iface_a", "v1", "to_a")
            .consumes_service("to_a")
            .build();

        match VirtualDeptree::build(vec![(PathBuf::from("a"), a), (PathBuf::from("b"), b)]) {
            Err(Error::ServiceActionContractCycle { kind, .. }) => assert_eq!(kind, "service"),
            other => panic!("expected ServiceActionContractCycle, got {:?}", other),
        }
    }

    #[test]
    fn build_detects_mutual_action_contract_cycle() {
        let a = NodeSpec::new("a")
            .implements("iface_a", "v1")
            .contract_dep("iface_b", "v1", "to_b")
            .consumes_action("to_b")
            .build();
        let b = NodeSpec::new("b")
            .implements("iface_b", "v1")
            .contract_dep("iface_a", "v1", "to_a")
            .consumes_action("to_a")
            .build();

        match VirtualDeptree::build(vec![(PathBuf::from("a"), a), (PathBuf::from("b"), b)]) {
            Err(Error::ServiceActionContractCycle { kind, .. }) => assert_eq!(kind, "action"),
            other => panic!("expected ServiceActionContractCycle, got {:?}", other),
        }
    }

    #[test]
    fn build_allows_bidirectional_topic_interfaces() {
        let a = NodeSpec::new("a")
            .implements("iface_a", "v1")
            .contract_dep("iface_b", "v1", "to_b")
            .consumes_topic("to_b", "telemetry")
            .build();
        let b = NodeSpec::new("b")
            .implements("iface_b", "v1")
            .contract_dep("iface_a", "v1", "to_a")
            .consumes_topic("to_a", "telemetry")
            .build();

        let tree = VirtualDeptree::build(vec![(PathBuf::from("a"), a), (PathBuf::from("b"), b)])
            .expect("mutual topics must stay allowed");
        assert_eq!(tree.topological_order().len(), 2);
    }

    #[test]
    fn build_allows_one_directional_service_interface() {
        let a = NodeSpec::new("a")
            .contract_dep("iface_b", "v1", "to_b")
            .consumes_service("to_b")
            .build();
        let b = NodeSpec::new("b").implements("iface_b", "v1").build();

        let tree = VirtualDeptree::build(vec![(PathBuf::from("a"), a), (PathBuf::from("b"), b)])
            .expect("one-directional service dep is fine");
        assert_eq!(tree.topological_order().len(), 2);
    }

    #[test]
    fn build_detects_three_node_service_cycle() {
        let a = NodeSpec::new("a")
            .implements("iface_a", "v1")
            .contract_dep("iface_b", "v1", "to_b")
            .consumes_service("to_b")
            .build();
        let b = NodeSpec::new("b")
            .implements("iface_b", "v1")
            .contract_dep("iface_c", "v1", "to_c")
            .consumes_service("to_c")
            .build();
        let c = NodeSpec::new("c")
            .implements("iface_c", "v1")
            .contract_dep("iface_a", "v1", "to_a")
            .consumes_service("to_a")
            .build();

        match VirtualDeptree::build(vec![
            (PathBuf::from("a"), a),
            (PathBuf::from("b"), b),
            (PathBuf::from("c"), c),
        ]) {
            Err(Error::ServiceActionContractCycle { nodes, .. }) => assert_eq!(nodes.len(), 3),
            other => panic!("expected ServiceActionContractCycle, got {:?}", other),
        }
    }
}
