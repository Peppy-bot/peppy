//! Regression guard for the case where two consumed topics share the same
//! producer node + topic name but use different `link_id`s. The generator
//! deduplicates the cap'n proto schema by `file_stem`; before the fix, the
//! second consumer's `register_schema` call overwrote the capnp struct name
//! while the first consumer's already-emitted per-link Rust module still
//! referenced the now-dead earlier name, producing
//! `error[E0433]: cannot find <link>_<topic>_message in <topic>_message_capnp`
//! at compile time.
//!
//! This is the Rust counterpart of the Python integration test
//! `python_handles_two_consumed_topics_sharing_topic_name` in
//! `tests/python/consumed_topics_dedup.rs`. The Python test installs the
//! generated package with `uv sync` and runs Python that imports both modules;
//! here we generate a peppygen lib, wire it into a user crate, and invoke
//! `cargo build` against the result.

use config::consts::{NODE_CONFIG_FILE, PEPPYGEN_OUTPUT_PATH};
use config::node::{ConsumedTopic, MessageFormat, PeppygenLanguage};
use generator::{DependencyContext, DeploymentInterface, InterfaceVariant, generate_peppygen_lib};
use std::fs;
use tempfile::TempDir;

use crate::helpers;

const NODE_CONFIG: &str = r#"{
  peppy_schema: "node/v1",
  manifest: {
    name: "test_controller",
    tag: "v1",
    depends_on: {
      nodes: [
        { name: "robot_arm", tag: "v1", link_id: "left_arm" },
        { name: "robot_arm", tag: "v1", link_id: "right_arm" }
      ]
    }
  },
  execution: {
    language: "rust",
    run_cmd: ["./target/debug/test_controller"]
  }
}"#;

const LEFT_CONSUMER: &str = r#"{ link_id: "left_arm", name: "joint_states" }"#;
const RIGHT_CONSUMER: &str = r#"{ link_id: "right_arm", name: "joint_states" }"#;
const SHARED_FORMAT: &str = r#"{
  positions: { $type: "array", $items: "f64", $length: 3 },
  velocities: { $type: "array", $items: "f64", $length: 3 },
  timestamp: "time"
}"#;

/// User crate `main.rs` that explicitly names both per-link consumer modules.
/// The references aren't strictly necessary to catch the bug — the generated
/// `peppygen` lib will fail to compile on its own — but they document what
/// the test is exercising and mirror the explicit-probe style of the Python
/// counterpart.
const USER_MAIN: &str = r#"
use peppygen::consumed_topics::{left_arm_joint_states, right_arm_joint_states};

#[allow(dead_code)]
fn _references_both_link_keyed_modules() {
    let _left: fn(_) -> _ = left_arm_joint_states::subscribe;
    let _right: fn(_) -> _ = right_arm_joint_states::subscribe;
}

fn main() {}
"#;

#[test]
fn rust_handles_two_consumed_topics_sharing_topic_name() {
    let temp_dir =
        TempDir::new_in(crate::helpers::test_tmp_root()).expect("failed to create temp directory");
    let user_node_dir = temp_dir.path().join("user_node");
    fs::create_dir_all(&user_node_dir).expect("failed to create user_node directory");

    fs::write(user_node_dir.join(NODE_CONFIG_FILE), NODE_CONFIG)
        .expect("failed to write peppy.json5");

    let left_topic: ConsumedTopic =
        serde_json5::from_str(LEFT_CONSUMER).expect("failed to parse left consumed topic");
    let right_topic: ConsumedTopic =
        serde_json5::from_str(RIGHT_CONSUMER).expect("failed to parse right consumed topic");
    let shared_format: MessageFormat =
        serde_json5::from_str(SHARED_FORMAT).expect("failed to parse shared message format");

    let interfaces = vec![
        DeploymentInterface::new(InterfaceVariant::ConsumedTopic {
            topic: left_topic,
            message_format: shared_format.clone(),
            dependency: DependencyContext::native("robot_arm", "v1"),
        }),
        DeploymentInterface::new(InterfaceVariant::ConsumedTopic {
            topic: right_topic,
            message_format: shared_format,
            dependency: DependencyContext::native("robot_arm", "v1"),
        }),
    ];

    generate_peppygen_lib(
        PeppygenLanguage::Rust,
        &user_node_dir,
        interfaces,
        "test-hash",
        &helpers::test_peppy_dirs(),
        Default::default(),
        None,
    )
    .expect("failed to generate peppygen lib");

    let peppygen_dir = user_node_dir.join(PEPPYGEN_OUTPUT_PATH);
    let left_module = peppygen_dir.join("src/consumed_topics/left_arm_joint_states.rs");
    let right_module = peppygen_dir.join("src/consumed_topics/right_arm_joint_states.rs");
    assert!(
        left_module.exists(),
        "left consumer module missing at {}",
        left_module.display()
    );
    assert!(
        right_module.exists(),
        "right consumer module missing at {}",
        right_module.display()
    );

    helpers::init_cargo_user_node(&user_node_dir);
    let src_dir = user_node_dir.join("src");
    fs::create_dir_all(&src_dir).expect("failed to create src dir");
    fs::write(src_dir.join("main.rs"), USER_MAIN).expect("failed to write user main.rs");

    helpers::compile_project(&user_node_dir);
}
