//! Regression guard for two consumed topics that share a topic name but carry
//! different message formats: `rgb_camera:v1` and `rgbd_camera:v1` both emit
//! `video_stream`, and only the rgbd header carries `align_mode`. Consumed
//! topic cap'n proto schemas are keyed per slot (`on_next_<link_id>_<topic>`),
//! so each slot writes its own file from its own format and the per-slot
//! decode code always matches the schema it reads.
//!
//! Before per-slot keying both slots resolved `on_next_video_stream_message`;
//! the first registration owned the file and every later slot was handed the
//! first struct while its decode code was still generated from its own
//! format. Declaring the rgbd slot after the rgb one produced
//! `error[E0599]: no method named get_align_mode` on the shared reader and the
//! node did not build; the other order built but read the rgb slot through a
//! schema with a field it never sets. Both declaration orders are covered.
//!
//! This is the Rust counterpart of `tests/python/consumed_topics_distinct_formats.rs`.

use config::consts::{NODE_CONFIG_FILE, PEPPYGEN_OUTPUT_PATH};
use config::node::{ConsumedTopic, MessageFormat, PeppygenLanguage};
use generator::{DeploymentInterface, InterfaceVariant, NodeTree, generate_peppygen_lib};
use std::fs;
use std::path::Path;
use tempfile::TempDir;

use crate::helpers;

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
    language: "rust",
    run_cmd: ["./target/debug/camera_consumer"]
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

/// User crate `main.rs` that constructs both per-slot messages: the chest
/// header carries `align_mode`, the wrist header is built without it (so the
/// build fails if the wrist slot ever picks up the rgbd schema, or the chest
/// slot the rgb one).
const USER_MAIN: &str = r#"
use peppygen::consumed_topics::chest::video_stream as chest_video_stream;
use peppygen::consumed_topics::wrist::video_stream as wrist_video_stream;

fn main() {
    let wrist = wrist_video_stream::Message {
        header: wrist_video_stream::MessageHeader {
            timestamp: std::time::SystemTime::UNIX_EPOCH,
            frame_id: 1,
        },
        encoding: String::from("rgb8"),
        width: 640,
        height: 480,
        frame: Vec::new(),
    };
    let chest = chest_video_stream::Message {
        header: chest_video_stream::MessageHeader {
            timestamp: std::time::SystemTime::UNIX_EPOCH,
            frame_id: 2,
            align_mode: String::from("depth_to_color"),
        },
        encoding: String::from("rgb8"),
        width: 640,
        height: 480,
        frame: Vec::new(),
    };
    assert_eq!(wrist.header.frame_id, 1);
    assert_eq!(chest.header.align_mode, "depth_to_color");
}
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
            helpers::contract_dep("rgb_camera", "v1", "wrist"),
        ),
        Slot::Chest => (
            CHEST_CONSUMER,
            RGBD_FORMAT,
            helpers::contract_dep("rgbd_camera", "v1", "chest"),
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

fn generate_and_build(order: [Slot; 2]) {
    let temp_dir =
        TempDir::new_in(crate::helpers::test_tmp_root()).expect("failed to create temp directory");
    let user_node_dir = temp_dir.path().join("user_node");
    fs::create_dir_all(&user_node_dir).expect("failed to create user_node directory");

    fs::write(user_node_dir.join(NODE_CONFIG_FILE), NODE_CONFIG)
        .expect("failed to write peppy.json5");

    let interfaces = order.into_iter().map(consumed_topic).collect();

    generate_peppygen_lib(
        PeppygenLanguage::Rust,
        &user_node_dir,
        interfaces,
        "test-hash",
        &helpers::test_peppy_dirs(),
        Default::default(),
        None,
        NodeTree::Source,
    )
    .expect("failed to generate peppygen lib");

    let peppygen_dir = user_node_dir.join(PEPPYGEN_OUTPUT_PATH);
    let capnp_dir = peppygen_dir.join("src/capnp");

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

    helpers::init_cargo_user_node(&user_node_dir);
    let src_dir = user_node_dir.join("src");
    fs::create_dir_all(&src_dir).expect("failed to create src dir");
    fs::write(src_dir.join("main.rs"), USER_MAIN).expect("failed to write user main.rs");

    helpers::compile_project(&user_node_dir);
}

#[test]
fn rust_builds_same_named_consumed_topics_with_distinct_formats() {
    generate_and_build([Slot::Wrist, Slot::Chest]);
}

#[test]
fn rust_builds_same_named_consumed_topics_with_distinct_formats_reversed_order() {
    generate_and_build([Slot::Chest, Slot::Wrist]);
}
