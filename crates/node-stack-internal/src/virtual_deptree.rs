//! Transient, in-memory dependency tree built from a set of locally-discovered
//! nodes (typically from a filesystem walk).
//!
//! Used by `peppy node sync -a` to determine the order in which nodes must be
//! synced so that each node's dependencies are already known by the time the
//! daemon tries to resolve their interfaces. Unlike the persistent
//! [`crate::NodeStack`], a `VirtualDeptree` is built from on-disk
//! `peppy.json5` files, lives only for the duration of the operation, and is
//! discarded immediately after — it never mutates the daemon's real node
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
/// any node in the input set are silently ignored — those are external
/// dependencies that the daemon will resolve via its persistent node stack.
#[derive(Debug)]
pub struct VirtualDeptree {
    graph: StableDiGraph<NodeKey, ()>,
    by_key: HashMap<NodeKey, NodeIndex>,
    infos: HashMap<NodeIndex, VirtualNodeInfo>,
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
        let mut origin_paths: HashMap<NodeKey, PathBuf> = HashMap::with_capacity(nodes.len());

        // First pass: register every input node and detect duplicates.
        for (root_dir, config) in nodes {
            let key = (
                config.manifest.name.as_str().to_owned(),
                config.manifest.tag.clone(),
            );

            if let Some(first) = origin_paths.get(&key) {
                return Err(Error::DuplicateLocalNode {
                    name: key.0,
                    tag: key.1,
                    first: first.clone(),
                    second: root_dir,
                });
            }

            let info = VirtualNodeInfo {
                root_dir: root_dir.clone(),
                config,
            };
            let idx = graph.add_node(key.clone());
            by_key.insert(key.clone(), idx);
            infos.insert(idx, info);
            origin_paths.insert(key, root_dir);
        }

        // Second pass: add edges from each declared dependency that is also
        // present in the input set. Unknown deps are skipped here — the daemon
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

        // Validate DAG.
        if let Err(cycle) = toposort(&graph, None) {
            let offending = cycle.node_id();
            let key = graph
                .node_weight(offending)
                .cloned()
                .unwrap_or_else(|| ("?".to_owned(), "?".to_owned()));
            return Err(Error::VirtualDeptreeCycle {
                nodes: vec![format!("{}:{}", key.0, key.1)],
            });
        }

        Ok(Self {
            graph,
            by_key,
            infos,
        })
    }

    /// Returns the dependency-ordered slice of node infos. Dependencies are
    /// guaranteed to appear before any of their dependants.
    pub fn topological_order(&self) -> Vec<&VirtualNodeInfo> {
        // Already validated as acyclic in `build`.
        let order = toposort(&self.graph, None).expect("graph is a DAG by construction");
        order.into_iter().map(|idx| &self.infos[&idx]).collect()
    }

    /// Returns the number of nodes in the tree.
    pub fn len(&self) -> usize {
        self.infos.len()
    }

    /// Returns `true` when the tree contains no nodes.
    pub fn is_empty(&self) -> bool {
        self.infos.is_empty()
    }

    /// Looks up a node by `(name, tag)`.
    pub fn get(&self, key: &NodeKey) -> Option<&VirtualNodeInfo> {
        self.by_key.get(key).and_then(|idx| self.infos.get(idx))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
                .map(|(n, t)| format!(r#"{{ name: "{n}", tag: "{t}", local_id: "{n}" }}"#))
                .collect::<Vec<_>>()
                .join(", ");
            format!("depends_on: {{ nodes: [{entries}] }},")
        };
        let json5 = format!(
            r#"{{
                peppy_schema: "node_v1",
                manifest: {{
                    name: "{name}",
                    tag: "0.1.0",
                    {depends_on}
                }},
                execution: {{ language: "rust", run_cmd: ["./bin"] }}
            }}"#
        );
        let path = dir.join(config::consts::NODE_CONFIG_FILE);
        std::fs::write(&path, json5).unwrap();
        NodeConfigParser::from_path(&path)
            .unwrap()
            .into_resolved()
            .unwrap()
    }

    fn key_of(info: &VirtualNodeInfo) -> NodeKey {
        info.key()
    }

    #[test]
    fn build_empty_returns_empty_order() {
        let tree = VirtualDeptree::build(vec![]).unwrap();
        assert!(tree.is_empty());
        assert_eq!(tree.len(), 0);
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
        assert_eq!(tree.len(), 2);
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
        let b = write_node(&b_dir, "b", &[("a", "0.1.0")]);
        let c = write_node(&c_dir, "c", &[("b", "0.1.0")]);

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
        let b = write_node(&tmp.path().join("b"), "b", &[("a", "0.1.0")]);
        let c = write_node(&tmp.path().join("c"), "c", &[("a", "0.1.0")]);
        let d = write_node(
            &tmp.path().join("d"),
            "d",
            &[("b", "0.1.0"), ("c", "0.1.0")],
        );

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
        let a = write_node(&tmp.path().join("a"), "a", &[("b", "0.1.0")]);
        let b = write_node(&tmp.path().join("b"), "b", &[("a", "0.1.0")]);

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
        let a = write_node(&a_dir, "a", &[("external_x", "9.9.9")]);

        let tree = VirtualDeptree::build(vec![(a_dir, a)]).unwrap();
        assert_eq!(tree.len(), 1);
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
                assert_eq!(tag, "0.1.0");
                assert_eq!(first, first_dir);
                assert_eq!(second, second_dir);
            }
            other => panic!("expected DuplicateLocalNode, got {:?}", other),
        }
    }
}
