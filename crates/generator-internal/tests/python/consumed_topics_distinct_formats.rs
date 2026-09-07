//! Regression guard for two consumed topics that share a topic name but carry
//! different message formats: `rgb_camera:v1` and `rgbd_camera:v1` both emit
//! `video_stream`, and only the rgbd header carries `align_mode`. Consumed
//! topic cap'n proto schemas are keyed per slot (`on_next_<link_id>_<topic>`),
//! so each slot writes its own file from its own format and each per-slot
//! `_deserialize_payload` decodes through the schema it was generated from.
//!
//! Before per-slot keying both slots resolved `on_next_video_stream_message`
//! and the last registration overwrote the file, so every consumer decoded
//! through whichever format was declared last: rgb declared last made every
//! rgbd frame raise on `alignMode`, rgbd declared last silently read an empty
//! `align_mode` on rgb frames. Both declaration orders are covered.
//!
//! This is the Python counterpart of `tests/rust/consumed_topics_distinct_formats.rs`.

use crate::helpers::{
    contract_dep, init_python_project_venv, init_python_user_node, run_uv, test_peppy_dirs,
};
use config::consts::{NODE_CONFIG_FILE, PEPPYGEN_OUTPUT_PATH};
use config::node::{ConsumedTopic, MessageFormat, PeppygenLanguage};
use generator::{DeploymentInterface, InterfaceVariant, NodeTree, generate_peppygen_lib};
use std::fs;
use std::path::Path;
use tempfile::TempDir;

const NODE_CONFIG: &str = r#"{
  peppy_schema: "node/v1",
  manifest: {
    name: "camera_consumer",
    tag: "v1",
    depends_on: {
      contracts: [
        { name: "rgb_camera", tag: "v1", link_id: "wrist" },
        { name: "rgbd_camera", tag: "v1", link_id: "chest" }
      ]
    }
  },
  interfaces: {
    topics: {
      consumes: [
        { link_id: "wrist", name: "video_stream" },
        { link_id: "chest", name: "video_stream" }
      ]
    }
  },
  execution: {
    language: "python",
    build_cmd: ["uv", "sync"],
    run_cmd: ["uv", "run", "camera_consumer"]
  }
}"#;

const WRIST_CONSUMER: &str = r#"{ link_id: "wrist", name: "video_stream" }"#;
const CHEST_CONSUMER: &str = r#"{ link_id: "chest", name: "video_stream" }"#;

/// `rgb_camera:v1` `video_stream`, trimmed to the fields that matter.
const RGB_FORMAT: &str = r#"{
  header: { $type: "object", timestamp: "time", frame_id: "u32" },
  encoding: "string",
  width: "u32",
  height: "u32",
  frame: { $type: "array", $items: "u8" }
}"#;

/// `rgbd_camera:v1` `video_stream`: one more header field than the rgb one.
const RGBD_FORMAT: &str = r#"{
  header: { $type: "object", timestamp: "time", frame_id: "u32", align_mode: "string" },
  encoding: "string",
  width: "u32",
  height: "u32",
  frame: { $type: "array", $items: "u8" }
}"#;

/// Round-trips one message per slot through that slot's own cap'n proto
/// loader and `_deserialize_payload`. The chest (rgbd) frame must come back
/// with its `align_mode`; the wrist (rgb) frame must decode to a dataclass
/// with no such attribute, and its builder must reject the field outright.
const PYTHON_PROBE: &str = r#"
import importlib
import sys

def capnp_loader(mod):
    for name in dir(mod):
        if name.endswith("_capnp") and name.startswith("_") and callable(getattr(mod, name)):
            return getattr(mod, name)
    sys.exit(f"no capnp loader found in {mod.__name__}")

def fill_common(msg, frame_id):
    header = msg.init("header")
    timestamp = header.init("timestamp")
    timestamp.sec = 12
    timestamp.nsec = 34
    header.frameId = frame_id
    msg.encoding = "rgb8"
    msg.width = 640
    msg.height = 480
    msg.frame = b"\x01\x02\x03"
    return header

wrist = importlib.import_module("peppygen.consumed_topics.wrist.video_stream")
chest = importlib.import_module("peppygen.consumed_topics.chest.video_stream")

chest_schema = capnp_loader(chest)()
chest_msg = chest_schema.OnNextChestVideoStreamMessage.new_message()
chest_header = fill_common(chest_msg, 2)
chest_header.alignMode = "depth_to_color"
chest_decoded = chest._deserialize_payload(chest_msg.to_bytes())
assert chest_decoded.header.frame_id == 2, chest_decoded
assert chest_decoded.header.align_mode == "depth_to_color", chest_decoded
assert chest_decoded.frame == b"\x01\x02\x03", chest_decoded

wrist_schema = capnp_loader(wrist)()
wrist_msg = wrist_schema.OnNextWristVideoStreamMessage.new_message()
wrist_header = fill_common(wrist_msg, 1)
try:
    wrist_header.alignMode = "depth_to_color"
except Exception:
    pass
else:
    sys.exit("the wrist (rgb) schema must not carry alignMode")
wrist_decoded = wrist._deserialize_payload(wrist_msg.to_bytes())
assert wrist_decoded.header.frame_id == 1, wrist_decoded
assert not hasattr(wrist_decoded.header, "align_mode"), wrist_decoded
assert wrist_decoded.frame == b"\x01\x02\x03", wrist_decoded

print("wrist and chest video_stream decode through their own per-slot schemas")
"#;

#[derive(Clone, Copy)]
enum Slot {
    Wrist,
    Chest,
}

fn consumed_topic(slot: Slot) -> DeploymentInterface {
    let (consumer, format, dependency) = match slot {
        Slot::Wrist => (
            WRIST_CONSUMER,
            RGB_FORMAT,
            contract_dep("rgb_camera", "v1", "wrist"),
        ),
        Slot::Chest => (
            CHEST_CONSUMER,
            RGBD_FORMAT,
            contract_dep("rgbd_camera", "v1", "chest"),
        ),
    };
    let topic: ConsumedTopic =
        serde_json5::from_str(consumer).expect("failed to parse consumed topic");
    let message_format: MessageFormat =
        serde_json5::from_str(format).expect("failed to parse message format");
    DeploymentInterface::new(InterfaceVariant::ConsumedTopic {
        topic,
        message_format,
        dependency,
    })
}

fn read_schema(capnp_dir: &Path, file_stem: &str) -> String {
    let path = capnp_dir.join(format!("{file_stem}.capnp"));
    fs::read_to_string(&path)
        .unwrap_or_else(|err| panic!("schema missing at {}: {err}", path.display()))
}

fn generate_and_probe(order: [Slot; 2]) {
    let temp_dir =
        TempDir::new_in(crate::helpers::test_tmp_root()).expect("failed to create temp directory");
    let user_node_dir = temp_dir.path().join("user_node");
    fs::create_dir_all(&user_node_dir).expect("failed to create user_node directory");

    fs::write(user_node_dir.join(NODE_CONFIG_FILE), NODE_CONFIG)
        .expect("failed to write peppy.json5");

    let interfaces = order.into_iter().map(consumed_topic).collect();

    generate_peppygen_lib(
        PeppygenLanguage::Python,
        &user_node_dir,
        interfaces,
        "test-hash",
        &test_peppy_dirs(),
        Default::default(),
        None,
        NodeTree::Source,
    )
    .expect("failed to generate peppygen lib");

    let peppygen_dir = user_node_dir.join(PEPPYGEN_OUTPUT_PATH);
    let capnp_dir = peppygen_dir.join("peppygen/capnp");

    let wrist_schema = read_schema(&capnp_dir, "on_next_wrist_video_stream_message");
    let chest_schema = read_schema(&capnp_dir, "on_next_chest_video_stream_message");
    assert!(
        !wrist_schema.contains("alignMode"),
        "the wrist (rgb) schema must not carry alignMode:\n{wrist_schema}"
    );
    assert!(
        chest_schema.contains("alignMode"),
        "the chest (rgbd) schema must carry alignMode:\n{chest_schema}"
    );
    assert_ne!(wrist_schema, chest_schema);

    let shared_stem = capnp_dir.join("on_next_video_stream_message.capnp");
    assert!(
        !shared_stem.exists(),
        "a topic-name-keyed schema file must not be generated: {}",
        shared_stem.display()
    );

    init_python_user_node(&user_node_dir);
    init_python_project_venv(&user_node_dir);

    let output = run_uv(&user_node_dir, &["run", "python", "-c", PYTHON_PROBE]);

    assert!(
        output.status.success(),
        "Python probe failed for same-named consumed topics with distinct formats.\n\
         stdout:\n{}\n\
         stderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn python_decodes_same_named_consumed_topics_with_distinct_formats() {
    generate_and_probe([Slot::Wrist, Slot::Chest]);
}

#[test]
fn python_decodes_same_named_consumed_topics_with_distinct_formats_reversed_order() {
    generate_and_probe([Slot::Chest, Slot::Wrist]);
}
