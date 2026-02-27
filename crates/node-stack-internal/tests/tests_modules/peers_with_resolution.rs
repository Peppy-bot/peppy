use std::path::PathBuf;

use node_stack::{NodeStack, collect_peer_specs, validate_peer_specs};

use crate::helpers::config_common::daemon_node_config;

#[test]
fn collect_peer_specs_extracts_topics_and_services() {
    let config: config::node::NodeConfig = serde_json5::from_str(
        r#"{
            schema_version: 2,
            manifest: { name: "vision", tag: "0.1.0", language: "rust" },
            build: { start_cmd: ["./vision"] },
            interfaces: {
                peers_with: {
                    topics: [
                        { id: "arm_pos", node: "arm_controller", name: "arm_position", tag: "0.1.0" }
                    ],
                    services: [
                        { id: "arm_status", node: "arm_controller", name: "get_status", tag: "0.1.0" }
                    ]
                }
            }
        }"#,
    )
    .expect("valid config");

    let specs = collect_peer_specs(&config);
    assert_eq!(
        specs.len(),
        2,
        "should have one topic spec and one service spec"
    );
}

#[test]
fn collect_peer_specs_returns_empty_when_no_peers() {
    let config: config::node::NodeConfig = serde_json5::from_str(
        r#"{
            schema_version: 2,
            manifest: { name: "solo", tag: "0.1.0", language: "rust" },
            build: { start_cmd: ["./solo"] }
        }"#,
    )
    .expect("valid config");

    let specs = collect_peer_specs(&config);
    assert!(specs.is_empty());
}

#[test]
fn mutual_peers_can_be_added_to_stack_in_any_order() {
    // Two nodes that peer with each other's topics — no dependency edges, no cycle.
    let vision: config::node::NodeConfig = serde_json5::from_str(
        r#"{
            schema_version: 2,
            manifest: { name: "vision", tag: "0.1.0", language: "rust" },
            build: { start_cmd: ["./vision"] },
            interfaces: {
                exposes: {
                    topics: [{ name: "object_position", qos_profile: "sensor_data", message_format: { x: "f64", y: "f64" } }]
                },
                peers_with: {
                    topics: [{ id: "arm_pos", node: "arm_controller", name: "arm_position", tag: "0.1.0" }]
                }
            }
        }"#,
    )
    .expect("valid vision config");

    let arm: config::node::NodeConfig = serde_json5::from_str(
        r#"{
            schema_version: 2,
            manifest: { name: "arm_controller", tag: "0.1.0", language: "rust" },
            build: { start_cmd: ["./arm"] },
            interfaces: {
                exposes: {
                    topics: [{ name: "arm_position", qos_profile: "sensor_data", message_format: { x: "f64", y: "f64", z: "f64" } }]
                },
                peers_with: {
                    topics: [{ id: "obj_pos", node: "vision", name: "object_position", tag: "0.1.0" }]
                }
            }
        }"#,
    )
    .expect("valid arm config");

    let stack = NodeStack::new(daemon_node_config(), None, PathBuf::from("/tmp"));

    // peers_with creates NO dependency edges, so both can be added in any order.
    // push_config only validates subscribes_to dependencies, not peers_with.
    stack
        .push_config(vision, false, PathBuf::from("/tmp"))
        .expect("vision should add without dependency errors");
    stack
        .push_config(arm, false, PathBuf::from("/tmp"))
        .expect("arm should add without dependency errors");

    assert_eq!(stack.len(), 3, "stack should have daemon + vision + arm");
}

#[test]
fn validate_peer_specs_detects_missing_peer_node() {
    let config: config::node::NodeConfig = serde_json5::from_str(
        r#"{
            schema_version: 2,
            manifest: { name: "vision", tag: "0.1.0", language: "rust" },
            build: { start_cmd: ["./vision"] },
            interfaces: {
                peers_with: {
                    topics: [{ id: "arm_pos", node: "arm_controller", name: "arm_position", tag: "0.1.0" }]
                }
            }
        }"#,
    )
    .expect("valid config");

    // Resolve against an empty set — peer node does not exist
    let errors = validate_peer_specs(&config, "vision", "0.1.0", |_, _| None);
    assert_eq!(errors.len(), 1, "should report one missing peer");
}

#[test]
fn validate_peer_specs_detects_missing_peer_interface() {
    let peer_config: config::node::NodeConfig = serde_json5::from_str(
        r#"{
            schema_version: 2,
            manifest: { name: "arm_controller", tag: "0.1.0", language: "rust" },
            build: { start_cmd: ["./arm"] },
            interfaces: {
                exposes: {
                    topics: [{ name: "wrong_topic", qos_profile: "standard" }]
                }
            }
        }"#,
    )
    .expect("valid peer config");

    let config: config::node::NodeConfig = serde_json5::from_str(
        r#"{
            schema_version: 2,
            manifest: { name: "vision", tag: "0.1.0", language: "rust" },
            build: { start_cmd: ["./vision"] },
            interfaces: {
                peers_with: {
                    topics: [{ id: "arm_pos", node: "arm_controller", name: "arm_position", tag: "0.1.0" }]
                }
            }
        }"#,
    )
    .expect("valid config");

    let errors = validate_peer_specs(&config, "vision", "0.1.0", |name, tag| {
        if name == "arm_controller" && tag == "0.1.0" {
            Some(peer_config.clone())
        } else {
            None
        }
    });
    assert_eq!(errors.len(), 1, "should report missing interface");
}

#[test]
fn validate_peer_specs_passes_when_peer_exposes_interface() {
    let peer_config: config::node::NodeConfig = serde_json5::from_str(
        r#"{
            schema_version: 2,
            manifest: { name: "arm_controller", tag: "0.1.0", language: "rust" },
            build: { start_cmd: ["./arm"] },
            interfaces: {
                exposes: {
                    topics: [{ name: "arm_position", qos_profile: "sensor_data", message_format: { x: "f64" } }]
                }
            }
        }"#,
    )
    .expect("valid peer config");

    let config: config::node::NodeConfig = serde_json5::from_str(
        r#"{
            schema_version: 2,
            manifest: { name: "vision", tag: "0.1.0", language: "rust" },
            build: { start_cmd: ["./vision"] },
            interfaces: {
                peers_with: {
                    topics: [{ id: "arm_pos", node: "arm_controller", name: "arm_position", tag: "0.1.0" }]
                }
            }
        }"#,
    )
    .expect("valid config");

    let errors = validate_peer_specs(&config, "vision", "0.1.0", |name, tag| {
        if name == "arm_controller" && tag == "0.1.0" {
            Some(peer_config.clone())
        } else {
            None
        }
    });
    assert!(
        errors.is_empty(),
        "should pass when peer exposes the interface"
    );
}
