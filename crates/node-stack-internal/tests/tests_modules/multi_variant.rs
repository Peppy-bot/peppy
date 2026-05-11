//! Multi-variant identity tests.
//!
//! These cover the regressions surfaced by the multi-variant rollout: two
//! variants of the same `(name, tag)` must coexist in the stack, the
//! interface-consistency assertion must reject mismatched variants, and
//! `apply_from` must round-trip every variant rather than collapsing them
//! at insert time.

use std::path::PathBuf;

use config::node::NodeConfig;
use node_stack::{DEFAULT_VARIANT, NodeStack, NodeStackError};

use crate::helpers::config_common::core_node_config;

fn sensor_config_with_extra_topic(extra_topic: Option<&str>) -> NodeConfig {
    let topics = match extra_topic {
        Some(extra) => format!(
            r#"emits: [
                {{ name: "data_stream", qos_profile: "sensor_data" }},
                {{ name: "{extra}", qos_profile: "sensor_data" }}
            ]"#
        ),
        None => r#"emits: [{ name: "data_stream", qos_profile: "sensor_data" }]"#.to_owned(),
    };
    let raw = format!(
        r#"{{
            peppy_schema: "node_v1",
            manifest: {{
                name: "sensor",
                tag: "1.0.0",
            }},
            interfaces: {{
                topics: {{ {topics} }}
            }},
            execution: {{
                language: "rust",
                run_cmd: ["sensor"]
            }}
        }}"#
    );
    serde_json5::from_str(&raw).expect("valid node config")
}

fn baseline_sensor_config() -> NodeConfig {
    sensor_config_with_extra_topic(None)
}

#[tokio::test]
async fn two_variants_of_same_name_tag_coexist() {
    // Regression: pre-rollout, the second push silently overwrote the slot
    // because key_to_index was keyed on `(name, tag)` only.
    let stack = NodeStack::new(core_node_config(), None, PathBuf::from("/tmp"));
    let config_path = PathBuf::from("/tmp/sensor.json5");

    stack
        .push_config_with_variant(
            baseline_sensor_config(),
            false,
            &config_path,
            "realsense_d405".to_owned(),
        )
        .expect("first variant should push");
    stack
        .push_config_with_variant(
            baseline_sensor_config(),
            false,
            &config_path,
            "realsense_d435".to_owned(),
        )
        .expect("second variant should push without overwriting the first");

    assert_eq!(
        stack.len(),
        3,
        "stack should contain root + two variants of sensor:1.0.0"
    );
    let variants = stack.find_all_variants("sensor", "1.0.0");
    assert_eq!(variants.len(), 2, "both variants should be reachable");
    let mut variant_names: Vec<String> = variants
        .iter()
        .map(|h| h.read().variant_name().to_owned())
        .collect();
    variant_names.sort();
    assert_eq!(variant_names, vec!["realsense_d405", "realsense_d435"]);

    assert!(stack.contains("sensor", "1.0.0", "realsense_d405"));
    assert!(stack.contains("sensor", "1.0.0", "realsense_d435"));
    assert!(!stack.contains("sensor", "1.0.0", DEFAULT_VARIANT));
}

#[tokio::test]
async fn interface_consistency_rejects_mismatched_second_variant() {
    let stack = NodeStack::new(core_node_config(), None, PathBuf::from("/tmp"));
    let config_path = PathBuf::from("/tmp/sensor.json5");

    stack
        .push_config_with_variant(
            baseline_sensor_config(),
            false,
            &config_path,
            "realsense_d405".to_owned(),
        )
        .expect("first variant defines the canonical interface");

    let err = stack
        .push_config_with_variant(
            sensor_config_with_extra_topic(Some("extra_topic")),
            false,
            &config_path,
            "realsense_d435".to_owned(),
        )
        .expect_err("second variant with extra topic must be rejected");

    match err {
        NodeStackError::InterfaceMismatchAcrossVariants {
            node_name,
            node_tag,
            canonical_variant,
            new_variant,
        } => {
            assert_eq!(node_name, "sensor");
            assert_eq!(node_tag, "1.0.0");
            assert_eq!(canonical_variant, "realsense_d405");
            assert_eq!(new_variant, "realsense_d435");
        }
        other => panic!("expected InterfaceMismatchAcrossVariants, got {other:?}"),
    }

    // The mismatched variant must not have been inserted.
    assert!(!stack.contains("sensor", "1.0.0", "realsense_d435"));
    assert_eq!(stack.find_all_variants("sensor", "1.0.0").len(), 1);
}

#[tokio::test]
async fn interface_consistency_passes_modulo_ordering() {
    // Same Interfaces, just declared in a different order should match
    // structurally (matches_unordered ignores list order).
    let stack = NodeStack::new(core_node_config(), None, PathBuf::from("/tmp"));
    let config_path = PathBuf::from("/tmp/sensor.json5");

    let raw_a = r#"{
        peppy_schema: "node_v1",
        manifest: { name: "sensor", tag: "1.0.0" },
        interfaces: {
            topics: {
                emits: [
                    { name: "alpha", qos_profile: "sensor_data" },
                    { name: "beta",  qos_profile: "sensor_data" }
                ]
            }
        },
        execution: { language: "rust", run_cmd: ["sensor"] }
    }"#;
    let raw_b = r#"{
        peppy_schema: "node_v1",
        manifest: { name: "sensor", tag: "1.0.0" },
        interfaces: {
            topics: {
                emits: [
                    { name: "beta",  qos_profile: "sensor_data" },
                    { name: "alpha", qos_profile: "sensor_data" }
                ]
            }
        },
        execution: { language: "rust", run_cmd: ["sensor"] }
    }"#;

    stack
        .push_config_with_variant(
            serde_json5::from_str::<NodeConfig>(raw_a).expect("valid"),
            false,
            &config_path,
            "variant_a".to_owned(),
        )
        .expect("first variant should push");
    stack
        .push_config_with_variant(
            serde_json5::from_str::<NodeConfig>(raw_b).expect("valid"),
            false,
            &config_path,
            "variant_b".to_owned(),
        )
        .expect("second variant with structurally identical interfaces should pass the assertion");
    assert_eq!(stack.find_all_variants("sensor", "1.0.0").len(), 2);
}

#[tokio::test]
async fn apply_from_round_trips_two_variants_of_one_name_tag() {
    let source = NodeStack::new(core_node_config(), None, PathBuf::from("/tmp"));
    let config_path = PathBuf::from("/tmp/sensor.json5");
    source
        .push_config_with_variant(
            baseline_sensor_config(),
            false,
            &config_path,
            "realsense_d405".to_owned(),
        )
        .expect("first variant in source");
    source
        .push_config_with_variant(
            baseline_sensor_config(),
            false,
            &config_path,
            "realsense_d435".to_owned(),
        )
        .expect("second variant in source");

    let target = NodeStack::new(core_node_config(), None, PathBuf::from("/tmp"));
    target.apply_from(&source).expect("apply_from succeeds");
    assert_eq!(
        target.find_all_variants("sensor", "1.0.0").len(),
        2,
        "both variants should be present in target after apply_from"
    );
}

#[tokio::test]
async fn dependents_fan_out_to_every_variant() {
    // A consumer that depends on `sensor:1.0.0` must produce edges to every
    // running variant of that slot, both for variants present at insert time
    // and for variants added later.
    let stack = NodeStack::new(core_node_config(), None, PathBuf::from("/tmp"));
    let config_path = PathBuf::from("/tmp/cfg.json5");

    let consumer_raw = r#"{
        peppy_schema: "node_v1",
        manifest: {
            name: "brain",
            tag: "1.0.0",
            depends_on: {
                nodes: [{ name: "sensor", tag: "1.0.0", local_id: "cam" }]
            }
        },
        interfaces: {
            topics: {
                consumes: [{ local_node_id: "cam", name: "data_stream" }]
            }
        },
        execution: { language: "rust", run_cmd: ["brain"] }
    }"#;
    let consumer: NodeConfig = serde_json5::from_str(consumer_raw).expect("valid");

    // First variant present, then consumer added: consumer attaches.
    stack
        .push_config_with_variant(
            baseline_sensor_config(),
            false,
            &config_path,
            "realsense_d405".to_owned(),
        )
        .expect("first variant");
    stack
        .push_config_with_variant(
            consumer.clone(),
            false,
            &config_path,
            DEFAULT_VARIANT.to_owned(),
        )
        .expect("consumer attaches to existing variant");
    assert_eq!(
        stack
            .dependencies_of("brain", "1.0.0", DEFAULT_VARIANT)
            .len(),
        1
    );

    // Second variant added later: consumer must gain an edge to it too.
    stack
        .push_config_with_variant(
            baseline_sensor_config(),
            false,
            &config_path,
            "realsense_d435".to_owned(),
        )
        .expect("second variant");
    assert_eq!(
        stack
            .dependencies_of("brain", "1.0.0", DEFAULT_VARIANT)
            .len(),
        2,
        "consumer should fan out to the newly added variant"
    );

    // Variant-agnostic dependents lookup unions across every variant.
    let dependents = stack.dependents_of_any_variant("sensor", "1.0.0");
    assert_eq!(
        dependents.len(),
        1,
        "a single brain consumer should be reported once even though it has edges to two variants"
    );
}
