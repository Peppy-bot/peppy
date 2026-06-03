//! Detection of caller-driven dependency cycles (services and actions).
//!
//! Topics are passive: a topic dependency means "I receive whatever is
//! published", so two nodes can each receive from the other with no runtime
//! ordering between them. Services and actions are caller-driven: depending on
//! one means "I will actively invoke the provider". Two nodes that each invoke
//! the other form a request/response cycle that deadlocks at runtime.
//!
//! The static dependency graph deliberately drops interface edges (that is what
//! makes bidirectional communication through interfaces possible), so a
//! service/action cycle routed through interfaces is invisible to the node-dep
//! DAG check. This module rebuilds the *caller-driven* edges only, resolving
//! interface dependencies to their providers via `conforms_to`, and reports any
//! cycle so callers can reject it.
//!
//! Detection is type-level: it reasons about node identities and their declared
//! `conforms_to` / consumes, not about resolved per-instance bindings. When an
//! interface has several conforming providers and only some of them close a
//! cycle, every conforming provider gets an edge, so a config that a specific
//! binding would keep acyclic can still be rejected. That is intentional: it is
//! conservative-safe (it never lets a real deadlock through) and it catches the
//! cycle the moment the second node is declared, including the deferred
//! (`--bind-deferred`) case where the two nodes are added in separate
//! invocations.

use std::collections::{BTreeMap, HashMap};

use config::node::{InterfaceKind, Interfaces, NodeConfig};
use petgraph::algo::tarjan_scc;
use petgraph::graph::{DiGraph, NodeIndex};

/// Identity plus config view of one node, enough to compute its caller-driven
/// edges. Borrowed so callers can build the slice from whatever store they own
/// (a transient batch, the persistent node stack) without cloning configs into
/// the helper.
pub struct CycleCheckNode<'a> {
    pub name: &'a str,
    pub tag: &'a str,
    pub config: &'a NodeConfig,
}

/// A detected caller-driven dependency cycle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceActionCycle {
    /// `name:tag` labels of the nodes on the cycle, sorted for deterministic
    /// reporting.
    pub nodes: Vec<String>,
    /// The `name:tag` of the dependency whose caller-driven edge closes the
    /// cycle. This is an interface for an interface-routed edge, or a direct
    /// node identity for a direct `depends_on.nodes` edge — hence the neutral
    /// name rather than `interface`.
    pub closing_dependency: String,
    /// Whether the closing edge is a service or an action dependency.
    pub kind: InterfaceKind,
}

/// One caller-driven edge `from -> to` (caller depends on provider), tagged
/// with the interface/node it routes through so a detected cycle can name it.
struct Edge {
    from: usize,
    to: usize,
    kind: InterfaceKind,
    label: String,
}

/// Returns the first caller-driven (service or action) cycle among `nodes`, or
/// `None` when the caller-driven dependencies are acyclic.
///
/// Edge rule (direction is caller -> provider; cycle detection is
/// direction-agnostic but the convention is kept consistent):
/// - For every link a node consumes as a service or action, its matching
///   `depends_on.interfaces` entry adds an edge to every in-set node that
///   `conforms_to` that interface, and its matching `depends_on.nodes` entry
///   adds an edge to the in-set node with that identity (the mixed direct +
///   interface case).
/// - Links consumed only as topics add no edge, so bidirectional topics stay
///   acyclic by construction.
pub fn find_service_action_cycle(nodes: &[CycleCheckNode<'_>]) -> Option<ServiceActionCycle> {
    let identity_to_index: HashMap<(&str, &str), usize> = nodes
        .iter()
        .enumerate()
        .map(|(idx, node)| ((node.name, node.tag), idx))
        .collect();

    let edges = build_caller_driven_edges(nodes, &identity_to_index);

    let mut graph: DiGraph<(), ()> = DiGraph::with_capacity(nodes.len(), edges.len());
    let indices: Vec<NodeIndex> = (0..nodes.len()).map(|_| graph.add_node(())).collect();
    for edge in &edges {
        graph.add_edge(indices[edge.from], indices[edge.to], ());
    }

    for component in tarjan_scc(&graph) {
        let members: Vec<usize> = component.iter().map(|idx| idx.index()).collect();
        if let Some(cycle) = cycle_from_component(&members, &edges, nodes) {
            return Some(cycle);
        }
    }

    None
}

/// Build the caller-driven edge list. A link is caller-driven when the node
/// consumes it as a service or action; the matching `depends_on` entry (by
/// `link_id`) is what resolves the provider.
fn build_caller_driven_edges(
    nodes: &[CycleCheckNode<'_>],
    identity_to_index: &HashMap<(&str, &str), usize>,
) -> Vec<Edge> {
    let mut edges = Vec::new();

    for (caller_idx, caller) in nodes.iter().enumerate() {
        let caller_driven = caller_driven_link_ids(&caller.config.interfaces);
        if caller_driven.is_empty() {
            continue;
        }

        let Some(depends_on) = caller.config.manifest.depends_on.as_ref() else {
            continue;
        };

        for dep in &depends_on.interfaces {
            let Some(&kind) = caller_driven.get(dep.link_id.as_str()) else {
                continue;
            };
            let iface_name = dep.name.as_str();
            let iface_tag = dep.tag.as_str();
            for (provider_idx, provider) in nodes.iter().enumerate() {
                if node_conforms_to(provider.config, iface_name, iface_tag) {
                    edges.push(Edge {
                        from: caller_idx,
                        to: provider_idx,
                        kind,
                        label: format!("{iface_name}:{iface_tag}"),
                    });
                }
            }
        }

        for dep in &depends_on.nodes {
            let Some(&kind) = caller_driven.get(dep.link_id.as_str()) else {
                continue;
            };
            let dep_name = dep.name.as_str();
            let dep_tag = dep.tag.as_str();
            if let Some(&provider_idx) = identity_to_index.get(&(dep_name, dep_tag)) {
                edges.push(Edge {
                    from: caller_idx,
                    to: provider_idx,
                    kind,
                    label: format!("{dep_name}:{dep_tag}"),
                });
            }
        }
    }

    edges
}

/// Map each `link_id` this node consumes as a service or action to its kind.
/// Topic consumes are intentionally excluded: a passive subscription never
/// deadlocks, so it contributes no caller-driven edge. When a link is consumed
/// as both a service and an action, service wins as the reported kind (the edge
/// exists either way; only the diagnostic label differs).
fn caller_driven_link_ids(interfaces: &Interfaces) -> BTreeMap<&str, InterfaceKind> {
    let mut link_ids = BTreeMap::new();

    if let Some(services) = &interfaces.services
        && let Some(consumes) = &services.consumes
    {
        for consumed in consumes {
            link_ids.insert(consumed.link_id.as_str(), InterfaceKind::Service);
        }
    }

    if let Some(actions) = &interfaces.actions
        && let Some(consumes) = &actions.consumes
    {
        for consumed in consumes {
            link_ids
                .entry(consumed.link_id.as_str())
                .or_insert(InterfaceKind::Action);
        }
    }

    link_ids
}

/// Does this node declare conformance to `(name, tag)`? Interface providers are
/// matched solely by `conforms_to`, never by node-name identity, consistent
/// with the binding validator's `producer_satisfies_slot`.
fn node_conforms_to(node: &NodeConfig, name: &str, tag: &str) -> bool {
    node.interfaces
        .conforms_to
        .as_deref()
        .unwrap_or(&[])
        .iter()
        .any(|item| item.name.as_str() == name && item.tag == tag)
}

/// Turn one strongly-connected component into a [`ServiceActionCycle`], or
/// `None` when the component is acyclic (a single node with no self-edge).
fn cycle_from_component(
    members: &[usize],
    edges: &[Edge],
    nodes: &[CycleCheckNode<'_>],
) -> Option<ServiceActionCycle> {
    let member_set: std::collections::HashSet<usize> = members.iter().copied().collect();

    // A single-node component is a cycle only when it has a self-edge (a node
    // consuming its own service/action interface). Multi-node components are
    // always cycles.
    if members.len() == 1
        && !edges
            .iter()
            .any(|e| e.from == members[0] && e.to == members[0])
    {
        return None;
    }

    // Pick the closing edge deterministically: among edges internal to the
    // component, the one with the smallest (from_label, to_label).
    let closing = edges
        .iter()
        .filter(|e| member_set.contains(&e.from) && member_set.contains(&e.to))
        .min_by(|a, b| {
            let a_key = (label_of(nodes, a.from), label_of(nodes, a.to));
            let b_key = (label_of(nodes, b.from), label_of(nodes, b.to));
            a_key.cmp(&b_key)
        })?;

    let mut labels: Vec<String> = members.iter().map(|&idx| label_of(nodes, idx)).collect();
    labels.sort();

    Some(ServiceActionCycle {
        nodes: labels,
        closing_dependency: closing.label.clone(),
        kind: closing.kind,
    })
}

fn label_of(nodes: &[CycleCheckNode<'_>], idx: usize) -> String {
    format!("{}:{}", nodes[idx].name, nodes[idx].tag)
}

#[cfg(test)]
mod tests {
    use super::*;
    use config::node::NodeConfigParser;

    /// Readable builder for a node config. All tags default to `v1`; only the
    /// pieces relevant to a test are set, the rest stay empty.
    struct NodeSpec {
        name: String,
        conforms_to: Vec<(String, String)>,
        iface_deps: Vec<(String, String, String)>,
        node_deps: Vec<(String, String, String)>,
        service_consumes: Vec<String>,
        action_consumes: Vec<String>,
        topic_consumes: Vec<(String, String)>,
    }

    impl NodeSpec {
        fn new(name: &str) -> Self {
            Self {
                name: name.to_owned(),
                conforms_to: Vec::new(),
                iface_deps: Vec::new(),
                node_deps: Vec::new(),
                service_consumes: Vec::new(),
                action_consumes: Vec::new(),
                topic_consumes: Vec::new(),
            }
        }

        fn conforms(mut self, name: &str, tag: &str) -> Self {
            self.conforms_to.push((name.to_owned(), tag.to_owned()));
            self
        }

        fn iface_dep(mut self, name: &str, tag: &str, link_id: &str) -> Self {
            self.iface_deps
                .push((name.to_owned(), tag.to_owned(), link_id.to_owned()));
            self
        }

        fn node_dep(mut self, name: &str, tag: &str, link_id: &str) -> Self {
            self.node_deps
                .push((name.to_owned(), tag.to_owned(), link_id.to_owned()));
            self
        }

        fn consumes_service(mut self, link_id: &str) -> Self {
            self.service_consumes.push(link_id.to_owned());
            self
        }

        fn consumes_action(mut self, link_id: &str) -> Self {
            self.action_consumes.push(link_id.to_owned());
            self
        }

        fn consumes_topic(mut self, link_id: &str, name: &str) -> Self {
            self.topic_consumes
                .push((link_id.to_owned(), name.to_owned()));
            self
        }

        fn build(&self) -> NodeConfig {
            let conforms = join(&self.conforms_to, |(n, t)| {
                format!(r#"{{ name: "{n}", tag: "{t}" }}"#)
            });
            let iface_deps = join(&self.iface_deps, |(n, t, l)| {
                format!(r#"{{ name: "{n}", tag: "{t}", link_id: "{l}" }}"#)
            });
            let node_deps = join(&self.node_deps, |(n, t, l)| {
                format!(r#"{{ name: "{n}", tag: "{t}", link_id: "{l}" }}"#)
            });
            let services = join(&self.service_consumes, |l| {
                format!(r#"{{ link_id: "{l}" }}"#)
            });
            let actions = join(&self.action_consumes, |l| {
                format!(r#"{{ link_id: "{l}" }}"#)
            });
            let topics = join(&self.topic_consumes, |(l, n)| {
                format!(r#"{{ link_id: "{l}", name: "{n}" }}"#)
            });

            let name = &self.name;
            let json5 = format!(
                r#"{{
                    peppy_schema: "node_v1",
                    manifest: {{
                        name: "{name}",
                        tag: "v1",
                        depends_on: {{ nodes: [{node_deps}], interfaces: [{iface_deps}] }},
                    }},
                    interfaces: {{
                        conforms_to: [{conforms}],
                        services: {{ consumes: [{services}] }},
                        actions: {{ consumes: [{actions}] }},
                        topics: {{ consumes: [{topics}] }},
                    }},
                    execution: {{ language: "rust", run_cmd: ["./bin"] }},
                }}"#
            );
            NodeConfigParser::from_content(&json5)
                .unwrap_or_else(|e| panic!("spec for `{name}` should parse: {e}\n{json5}"))
        }
    }

    fn join<T>(items: &[T], render: impl Fn(&T) -> String) -> String {
        items.iter().map(render).collect::<Vec<_>>().join(", ")
    }

    fn view(configs: &[NodeConfig]) -> Vec<CycleCheckNode<'_>> {
        configs
            .iter()
            .map(|config| CycleCheckNode {
                name: config.manifest.name.as_str(),
                tag: config.manifest.tag.as_str(),
                config,
            })
            .collect()
    }

    fn find_cycle(configs: &[NodeConfig]) -> Option<ServiceActionCycle> {
        find_service_action_cycle(&view(configs))
    }

    #[test]
    fn mutual_service_via_interfaces_is_cycle() {
        let a = NodeSpec::new("a")
            .conforms("iface_a", "v1")
            .iface_dep("iface_b", "v1", "to_b")
            .consumes_service("to_b")
            .build();
        let b = NodeSpec::new("b")
            .conforms("iface_b", "v1")
            .iface_dep("iface_a", "v1", "to_a")
            .consumes_service("to_a")
            .build();

        let cycle = find_cycle(&[a, b]).expect("mutual service should be a cycle");
        assert_eq!(cycle.kind, InterfaceKind::Service);
        assert!(cycle.nodes.contains(&"a:v1".to_owned()));
        assert!(cycle.nodes.contains(&"b:v1".to_owned()));
    }

    #[test]
    fn mutual_action_via_interfaces_is_cycle() {
        let a = NodeSpec::new("a")
            .conforms("iface_a", "v1")
            .iface_dep("iface_b", "v1", "to_b")
            .consumes_action("to_b")
            .build();
        let b = NodeSpec::new("b")
            .conforms("iface_b", "v1")
            .iface_dep("iface_a", "v1", "to_a")
            .consumes_action("to_a")
            .build();

        let cycle = find_cycle(&[a, b]).expect("mutual action should be a cycle");
        assert_eq!(cycle.kind, InterfaceKind::Action);
    }

    #[test]
    fn bidirectional_topic_via_interfaces_is_not_cycle() {
        let a = NodeSpec::new("a")
            .conforms("iface_a", "v1")
            .iface_dep("iface_b", "v1", "to_b")
            .consumes_topic("to_b", "telemetry")
            .build();
        let b = NodeSpec::new("b")
            .conforms("iface_b", "v1")
            .iface_dep("iface_a", "v1", "to_a")
            .consumes_topic("to_a", "telemetry")
            .build();

        assert!(
            find_cycle(&[a, b]).is_none(),
            "mutual topics must stay allowed"
        );
    }

    #[test]
    fn three_node_service_cycle_detected() {
        let a = NodeSpec::new("a")
            .conforms("iface_a", "v1")
            .iface_dep("iface_b", "v1", "to_b")
            .consumes_service("to_b")
            .build();
        let b = NodeSpec::new("b")
            .conforms("iface_b", "v1")
            .iface_dep("iface_c", "v1", "to_c")
            .consumes_service("to_c")
            .build();
        let c = NodeSpec::new("c")
            .conforms("iface_c", "v1")
            .iface_dep("iface_a", "v1", "to_a")
            .consumes_service("to_a")
            .build();

        let cycle = find_cycle(&[a, b, c]).expect("three-node service cycle should be detected");
        assert_eq!(cycle.nodes.len(), 3);
    }

    #[test]
    fn one_directional_service_interface_dep_is_ok() {
        let a = NodeSpec::new("a")
            .iface_dep("iface_b", "v1", "to_b")
            .consumes_service("to_b")
            .build();
        let b = NodeSpec::new("b").conforms("iface_b", "v1").build();

        assert!(
            find_cycle(&[a, b]).is_none(),
            "a one-way service dependency is fine"
        );
    }

    #[test]
    fn mixed_node_dep_and_interface_dep_service_cycle_detected() {
        // a -> b via a direct node-dep service call; b -> a via an interface
        // service call. Neither the node-dep DAG check nor an interface-only
        // check would catch this on its own.
        let a = NodeSpec::new("a")
            .conforms("iface_a", "v1")
            .node_dep("b", "v1", "to_b")
            .consumes_service("to_b")
            .build();
        let b = NodeSpec::new("b")
            .iface_dep("iface_a", "v1", "to_a")
            .consumes_service("to_a")
            .build();

        let cycle = find_cycle(&[a, b]).expect("mixed node+interface service cycle");
        assert_eq!(cycle.kind, InterfaceKind::Service);
    }

    #[test]
    fn provider_without_conforms_to_adds_no_edge() {
        // The would-be provider's node name equals the interface name, but it
        // does not declare `conforms_to`, so it never satisfies the dep.
        let a = NodeSpec::new("a")
            .conforms("iface_a", "v1")
            .iface_dep("iface_b", "v1", "to_b")
            .consumes_service("to_b")
            .build();
        let b = NodeSpec::new("iface_b")
            .iface_dep("iface_a", "v1", "to_a")
            .consumes_service("to_a")
            .build();

        assert!(
            find_cycle(&[a, b]).is_none(),
            "interface providers match by conforms_to, never node-name identity"
        );
    }

    #[test]
    fn multi_provider_over_approximation_is_rejected() {
        // A consumes iface_b; both b1 and b2 conform. Only b1 closes a cycle
        // back to A. A specific binding could pick b2 and stay acyclic, but the
        // conservative type-level check rejects regardless. This locks in the
        // documented over-approximation.
        let a = NodeSpec::new("a")
            .conforms("iface_a", "v1")
            .iface_dep("iface_b", "v1", "to_b")
            .consumes_service("to_b")
            .build();
        let b1 = NodeSpec::new("b1")
            .conforms("iface_b", "v1")
            .iface_dep("iface_a", "v1", "to_a")
            .consumes_service("to_a")
            .build();
        let b2 = NodeSpec::new("b2").conforms("iface_b", "v1").build();

        let cycle = find_cycle(&[a, b1, b2]).expect("conservative check rejects multi-provider");
        assert!(cycle.nodes.contains(&"a:v1".to_owned()));
        assert!(cycle.nodes.contains(&"b1:v1".to_owned()));
    }

    #[test]
    fn self_service_consume_is_cycle() {
        let a = NodeSpec::new("a")
            .conforms("iface_a", "v1")
            .iface_dep("iface_a", "v1", "to_self")
            .consumes_service("to_self")
            .build();

        let cycle = find_cycle(&[a]).expect("consuming your own service is a self-deadlock");
        assert_eq!(cycle.nodes, vec!["a:v1".to_owned()]);
    }

    #[test]
    fn independent_nodes_have_no_cycle() {
        let a = NodeSpec::new("a")
            .iface_dep("iface_b", "v1", "to_b")
            .consumes_service("to_b")
            .build();
        let b = NodeSpec::new("b").conforms("iface_b", "v1").build();
        let c = NodeSpec::new("c").build();

        assert!(find_cycle(&[a, b, c]).is_none());
    }
}
