//! Regression guard for the case where two consumed topics share the same
//! producer node + topic name but use different `link_id`s. The generator
//! deduplicates the cap'n proto schema by `file_stem`; before the fix on the
//! Rust path, the per-link Rust module would reference a struct name that had
//! been overwritten in the deduplicated capnp source. Python's
//! `register_schema` already derives the struct identity from the file_stem,
//! so this test passes both before and after the fix, but it acts as a
//! forward-looking guardrail so the Python generator can't regress into the
//! same divergence.
//!
//! This is the Python equivalent of the Rust test
//! `compile_lib_with_two_consumed_topics_sharing_topic_name` in
//! `src/generator/rust/tests/topics.rs`. The Rust test compiles the generated
//! crate via `cargo build`; here we install the generated package with
//! `uv sync` and execute Python that imports both per-link modules and
//! force-resolves the cap'n proto struct each module's `_deserialize_payload`
//! references.

use crate::helpers::TOPIC_DEDUP_SHARED_FORMAT as SHARED_FORMAT;
use crate::helpers::{
    init_python_project_venv, init_python_user_node, native_dep, test_peppy_dirs,
};
use config::consts::{NODE_CONFIG_FILE, PEPPYGEN_OUTPUT_PATH};
use config::node::{ConsumedTopic, MessageFormat, PeppygenLanguage};
use generator::{DeploymentInterface, InterfaceVariant, generate_peppygen_lib};
use std::fs;
use std::process::{Command, Stdio};
use tempfile::TempDir;

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
    language: "python",
    build_cmd: ["uv", "sync"],
    run_cmd: ["uv", "run", "test_controller"]
  }
}"#;

const LEFT_CONSUMER: &str = r#"{ link_id: "left_arm", name: "joint_states" }"#;
const RIGHT_CONSUMER: &str = r#"{ link_id: "right_arm", name: "joint_states" }"#;

/// Probes the generated Python modules by importing both per-link consumers
/// and accessing the cap'n proto struct each one's `_deserialize_payload`
/// references. If the dedup logic ever drifts so that a consumer module
/// references a struct name not present in the shared capnp file, the
/// `getattr` call here raises `AttributeError` and the subprocess exits
/// non-zero.
const PYTHON_PROBE: &str = r#"
import importlib
import inspect
import re
import sys

def referenced_struct(mod):
    src = inspect.getsource(mod._deserialize_payload)
    m = re.search(r"_capnp\(\)\.(\w+)\.from_bytes", src)
    if m is None:
        sys.exit(f"could not locate capnp struct reference in {mod.__name__}")
    return m.group(1)

def capnp_loader(mod):
    for name in dir(mod):
        if name.endswith("_capnp") and name.startswith("_") and callable(getattr(mod, name)):
            return getattr(mod, name)
    sys.exit(f"no capnp loader found in {mod.__name__}")

left = importlib.import_module("peppygen.consumed_topics.left_arm.joint_states")
right = importlib.import_module("peppygen.consumed_topics.right_arm.joint_states")

left_schema = capnp_loader(left)()
right_schema = capnp_loader(right)()

left_struct_name = referenced_struct(left)
right_struct_name = referenced_struct(right)

# Force-resolve each referenced struct. AttributeError here means the per-link
# Python module references a struct that doesn't exist in the deduplicated
# cap'n proto file, the same class of bug that bit the Rust generator.
getattr(left_schema, left_struct_name)
getattr(right_schema, right_struct_name)

print(f"left={left_struct_name} right={right_struct_name}")
"#;

#[test]
fn python_handles_two_consumed_topics_sharing_topic_name() {
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
            dependency: native_dep("robot_arm", "v1", "robot_arm"),
        }),
        DeploymentInterface::new(InterfaceVariant::ConsumedTopic {
            topic: right_topic,
            message_format: shared_format,
            dependency: native_dep("robot_arm", "v1", "robot_arm"),
        }),
    ];

    generate_peppygen_lib(
        PeppygenLanguage::Python,
        &user_node_dir,
        interfaces,
        "test-hash",
        &test_peppy_dirs(),
        Default::default(),
        None,
    )
    .expect("failed to generate peppygen lib");

    let peppygen_dir = user_node_dir.join(PEPPYGEN_OUTPUT_PATH);
    let left_module = peppygen_dir.join("peppygen/consumed_topics/left_arm/joint_states.py");
    let right_module = peppygen_dir.join("peppygen/consumed_topics/right_arm/joint_states.py");
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

    init_python_user_node(&user_node_dir);
    init_python_project_venv(&user_node_dir);

    let output = Command::new("uv")
        .args(["run", "python", "-c", PYTHON_PROBE])
        .current_dir(&user_node_dir)
        .stdin(Stdio::null())
        .output()
        .expect("failed to invoke uv run python");

    assert!(
        output.status.success(),
        "Python probe failed for two-consumed-topics dedup scenario.\n\
         stdout:\n{}\n\
         stderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
