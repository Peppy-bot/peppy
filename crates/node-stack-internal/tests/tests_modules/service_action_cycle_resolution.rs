//! Cross-invocation caller-driven cycle rejection at the node stack.
//!
//! `push_config` is the mechanism the daemon's `node add` uses to commit a node
//! into the persistent stack, so these tests exercise the exact path that
//! catches a service/action cycle completed across separate invocations: the
//! first node is added with no provider present yet, then the second node closes
//! the cycle and is rejected. Topics must stay bidirectional.

use std::path::PathBuf;

use node_stack::{NodeStack, NodeStackError};

use crate::helpers::config_common::core_node_config;

/// Build an interface-consuming node. `conforms_to` is the interface this node
/// provides; `dep_iface`/`link_id` is the interface it depends on; `consumes`
/// is the raw `interfaces` consume block (service/action/topic) referencing
/// `link_id`.
fn interface_node(
    name: &str,
    conforms_to: &str,
    dep_iface: &str,
    link_id: &str,
    consumes: &str,
) -> config::node::NodeConfig {
    let json5 = format!(
        r#"{{
            peppy_schema: "node_v1",
            manifest: {{
                name: "{name}",
                tag: "v1",
                depends_on: {{
                    interfaces: [{{ name: "{dep_iface}", tag: "v1", link_id: "{link_id}" }}]
                }},
            }},
            interfaces: {{
                conforms_to: [{{ name: "{conforms_to}", tag: "v1" }}],
                {consumes}
            }},
            execution: {{ language: "rust", run_cmd: ["{name}"] }},
        }}"#
    );
    serde_json5::from_str(&json5).unwrap_or_else(|e| panic!("node `{name}` config: {e}\n{json5}"))
}

fn new_stack() -> NodeStack {
    NodeStack::new(core_node_config(), None, PathBuf::from("/tmp"))
}

#[test]
fn mutual_service_through_interfaces_rejected_when_second_node_added() {
    let stack = new_stack();

    // `a` depends on `iface_b` for a service and conforms to `iface_b`'s
    // counterpart `iface_a`. No provider for `iface_b` exists yet, so this adds
    // cleanly.
    let a = interface_node(
        "a",
        "iface_a",
        "iface_b",
        "to_b",
        r#"services: { consumes: [{ link_id: "to_b", name: "do_b" }] }"#,
    );
    stack
        .push_config(a, false, PathBuf::from("/tmp"))
        .expect("first node adds with no provider present yet");
    assert_eq!(stack.len(), 2, "core + a");

    // `b` closes the cycle: it conforms to `iface_b` (so it provides what `a`
    // consumes) and consumes a service from `iface_a` (which `a` provides).
    let b = interface_node(
        "b",
        "iface_b",
        "iface_a",
        "to_a",
        r#"services: { consumes: [{ link_id: "to_a", name: "do_a" }] }"#,
    );
    let err = stack
        .push_config(b, false, PathBuf::from("/tmp"))
        .expect_err("mutual service through interfaces must be rejected");

    match err {
        NodeStackError::ServiceActionInterfaceCycle { kind, .. } => assert_eq!(kind, "service"),
        other => panic!("expected ServiceActionInterfaceCycle, got {other:?}"),
    }
    assert_eq!(stack.len(), 2, "rejected node must not be added");
    assert!(!stack.contains("b", "v1"));
}

#[test]
fn mutual_service_through_interfaces_rejected_on_permissive_add() {
    // `node add` pushes with `allow_missing_dependencies = true`, which skips
    // the dependency check. That must NOT skip the cycle check: a permissive
    // add that closes a service/action cycle has to be rejected just like a
    // strict one.
    let stack = new_stack();

    let a = interface_node(
        "a",
        "iface_a",
        "iface_b",
        "to_b",
        r#"services: { consumes: [{ link_id: "to_b", name: "do_b" }] }"#,
    );
    stack
        .push_config(a, true, PathBuf::from("/tmp"))
        .expect("first node adds permissively with no provider present yet");
    assert_eq!(stack.len(), 2, "core + a");

    let b = interface_node(
        "b",
        "iface_b",
        "iface_a",
        "to_a",
        r#"services: { consumes: [{ link_id: "to_a", name: "do_a" }] }"#,
    );
    let err = stack
        .push_config(b, true, PathBuf::from("/tmp"))
        .expect_err("a permissive add must still reject a service cycle");

    match err {
        NodeStackError::ServiceActionInterfaceCycle { kind, .. } => assert_eq!(kind, "service"),
        other => panic!("expected ServiceActionInterfaceCycle, got {other:?}"),
    }
    assert_eq!(stack.len(), 2, "rejected node must not be added");
    assert!(!stack.contains("b", "v1"));
}

#[test]
fn mutual_action_through_interfaces_rejected_when_second_node_added() {
    let stack = new_stack();

    let a = interface_node(
        "a",
        "iface_a",
        "iface_b",
        "to_b",
        r#"actions: { consumes: [{ link_id: "to_b", name: "do_b" }] }"#,
    );
    stack
        .push_config(a, false, PathBuf::from("/tmp"))
        .expect("first node adds with no provider present yet");

    let b = interface_node(
        "b",
        "iface_b",
        "iface_a",
        "to_a",
        r#"actions: { consumes: [{ link_id: "to_a", name: "do_a" }] }"#,
    );
    let err = stack
        .push_config(b, false, PathBuf::from("/tmp"))
        .expect_err("mutual action through interfaces must be rejected");

    match err {
        NodeStackError::ServiceActionInterfaceCycle { kind, .. } => assert_eq!(kind, "action"),
        other => panic!("expected ServiceActionInterfaceCycle, got {other:?}"),
    }
    assert!(!stack.contains("b", "v1"));
}

#[test]
fn bidirectional_topics_through_interfaces_allowed() {
    let stack = new_stack();

    let a = interface_node(
        "a",
        "iface_a",
        "iface_b",
        "to_b",
        r#"topics: { consumes: [{ link_id: "to_b", name: "telemetry" }] }"#,
    );
    stack
        .push_config(a, false, PathBuf::from("/tmp"))
        .expect("first topic consumer adds cleanly");

    let b = interface_node(
        "b",
        "iface_b",
        "iface_a",
        "to_a",
        r#"topics: { consumes: [{ link_id: "to_a", name: "telemetry" }] }"#,
    );
    stack
        .push_config(b, false, PathBuf::from("/tmp"))
        .expect("mutual topics must stay allowed");

    assert_eq!(stack.len(), 3, "core + a + b");
    assert!(stack.contains("b", "v1"));
}

#[test]
fn one_directional_service_through_interface_allowed() {
    let stack = new_stack();

    // Provider `b` conforms to `iface_b` and consumes nothing back.
    let provider = serde_json5::from_str::<config::node::NodeConfig>(
        r#"{
            peppy_schema: "node_v1",
            manifest: { name: "b", tag: "v1" },
            interfaces: {
                conforms_to: [{ name: "iface_b", tag: "v1" }],
                services: { exposes: [{ name: "do_b" }] },
            },
            execution: { language: "rust", run_cmd: ["b"] },
        }"#,
    )
    .expect("valid provider config");
    stack
        .push_config(provider, false, PathBuf::from("/tmp"))
        .expect("provider adds cleanly");

    // Consumer `a` calls `b`'s service through the interface; no back-edge.
    let a = interface_node(
        "a",
        "iface_a",
        "iface_b",
        "to_b",
        r#"services: { consumes: [{ link_id: "to_b", name: "do_b" }] }"#,
    );
    stack
        .push_config(a, false, PathBuf::from("/tmp"))
        .expect("a one-way service dependency is fine");

    assert_eq!(stack.len(), 3, "core + b + a");
}
