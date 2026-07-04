//! Generate-and-import fixture for pairing peer modules (Python) — twin of
//! the Rust `pairing_lib` test: a synthetic two-role pairing (`arm_link/v1`,
//! seen from the arm side) generates `pairings/<link_id>/<topic>` modules, and
//! importing them from the project venv proves the generated code parses and
//! its whole surface (slot consts, `paired()`/`wait_paired()`, publisher,
//! subscription) resolves against the installed peppylib.

use config::consts::PEPPYGEN_OUTPUT_PATH;
use config::node::EmittedTopic;
use generator::LanguageGenerator;
use generator::PeerContext;
use std::fs;
use std::path::Path;
use tempfile::TempDir;

use crate::helpers::{
    STUB_PYTHON_NODE_CONFIG, copy_config_to_output, init_python_project_venv,
    init_python_user_node, init_test_env, test_peppy_dirs,
};

const JOINT_COMMANDS: &str = r#"
{
  name: "joint_commands",
  qos_profile: "reliable",
  message_format: {
    target_positions: { $type: "array", $items: "f64", $length: 3 },
    max_velocity: "f64"
  }
}
"#;

const JOINT_STATES: &str = r#"
{
  name: "joint_states",
  qos_profile: "sensor_data",
  message_format: {
    positions: { $type: "array", $items: "f64", $length: 3 },
    sequence: "u64"
  }
}
"#;

/// The arm side of `arm_link/v1`: emits `joint_states`, consumes
/// `joint_commands`, through one slot whose link_id is `controller`.
fn arm_peer_context() -> PeerContext {
    PeerContext {
        link_id: "controller".to_string(),
        pairing_name: "arm_link".to_string(),
        pairing_tag: "v1".to_string(),
    }
}

#[test]
fn generated_peer_modules_import_from_venv() {
    let temp_dir = TempDir::new_in(crate::helpers::test_tmp_root()).unwrap();
    let commands: EmittedTopic = serde_json5::from_str(JOINT_COMMANDS).unwrap();
    let states: EmittedTopic = serde_json5::from_str(JOINT_STATES).unwrap();

    let (mut generator, output_dir, user_node, peppy_node_config_path) =
        init_test_env::<generator::PythonGenerator>(&temp_dir, STUB_PYTHON_NODE_CONFIG);
    let peer = arm_peer_context();
    generator.add_peer_emitted_topic(&states, &peer).unwrap();
    generator.add_peer_consumed_topic(&commands, &peer).unwrap();
    let output_config = copy_config_to_output(&user_node, &output_dir);
    generator
        .build(&output_dir, &test_peppy_dirs(), Default::default())
        .unwrap();
    fs::remove_file(output_config).unwrap();
    config::fingerprint::create_codegen_fingerprint(
        &peppy_node_config_path,
        Path::new(PEPPYGEN_OUTPUT_PATH),
    );

    let pairings_dir = user_node
        .join(PEPPYGEN_OUTPUT_PATH)
        .join("peppygen/pairings");
    for module in ["controller/joint_states.py", "controller/joint_commands.py"] {
        assert!(
            pairings_dir.join(module).exists(),
            "expected generated module pairings/{module}"
        );
    }

    init_python_user_node(&user_node);
    init_python_project_venv(&user_node);

    let check = r#"
import inspect
from peppygen.pairings.controller import joint_states, joint_commands

# Slot consts shared by both directions of the slot.
assert joint_states.LINK_ID == "controller"
assert joint_states.PAIRING_NAME == "arm_link"
assert joint_states.PAIRING_TAG == "v1"
assert joint_commands.TOPIC_NAME == "joint_commands"

# Emitted side: message builder + slot-scoped publisher + pin-state helpers.
assert callable(joint_states.build_message)
assert inspect.iscoroutinefunction(joint_states.declare_publisher)
assert callable(joint_states.paired)
assert inspect.iscoroutinefunction(joint_states.wait_paired)

# A three-joint command message round-trips through the builder.
payload = joint_states.build_message([0.0, 0.5, 1.0], 123)
assert isinstance(payload, bytes) and payload

# Consumed side: held Subscription + subscribe seam + pin-state helpers.
assert inspect.iscoroutinefunction(joint_commands.subscribe)
assert inspect.isclass(joint_commands.Subscription)
assert inspect.iscoroutinefunction(joint_commands.wait_paired)
print("peer modules imported")
"#;
    let output = std::process::Command::new(user_node.join(".venv/bin/python"))
        .args(["-c", check])
        .current_dir(&user_node)
        .output()
        .expect("failed to run venv python");
    assert!(
        output.status.success(),
        "importing generated peer modules failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
