//! Reproduces the realsense_d435 example from the conforms_to spec: a node
//! that emits its own `video_stream` topic and also conforms to three
//! interfaces (`depth_camera:v1`, `depth_camera:v2`, `uvc_camera:v1`), each
//! of which exposes a `video_stream` topic with a distinct shape.
//!
//! We feed the resolved `DeploymentInterface`s directly to
//! `generate_peppygen_lib` (the cache-loading side is exercised by the sync
//! unit tests in `core-node-internal`), and verify:
//!   1. The generated file layout nests conformed artifacts under
//!      `emitted_topics/{iface_name}/{iface_tag}/{topic}.rs` while keeping the
//!      native artifact at `emitted_topics/{topic}.rs`.
//!   2. Each `mod.rs` declares the right children.
//!   3. The rendered emit call inside each leaf passes the matching
//!      `iface_name`, `iface_tag` literals (or `"_"`, `"_"` for native) to
//!      `peppylib::TopicMessenger::emit`.

use crate::helpers::{prepare_directories, test_peppy_dirs};
use config::node::{
    EmittedTopic, MessageFormat, PeppygenLanguage, QoSProfile, SchemaType, TypeToken,
};
use generator::{
    CrateDeployMode, DeploymentInterface, InterfaceOrigin, InterfaceVariant, generate_peppygen_lib,
};
use indexmap::IndexMap;
use std::fs;
use tempfile::TempDir;

/// Minimal `video_stream`-shaped topic with one distinguishing field per
/// interface tag so we can tell the generated files apart at a glance.
fn make_topic(distinguishing_field: &str) -> EmittedTopic {
    let mut fields: IndexMap<String, SchemaType> = IndexMap::new();
    fields.insert(
        distinguishing_field.to_string(),
        SchemaType::Type(TypeToken::U32),
    );
    EmittedTopic {
        name: "video_stream".to_string(),
        qos_profile: QoSProfile::SensorData,
        message_format: Some(MessageFormat(fields)),
    }
}

fn conformed(name: &str, tag: &str, topic: EmittedTopic) -> DeploymentInterface {
    DeploymentInterface::new(InterfaceVariant::EmittedTopic {
        topic,
        origin: Some(InterfaceOrigin {
            iface_name: name.to_string(),
            iface_tag: tag.to_string(),
        }),
    })
}

const NODE_CONFIG: &str = r#"{
  peppy_schema: "node_v1",
  manifest: {
    name: "realsense_d435",
    tag: "v1"
  },
  execution: {
    language: "rust",
    run_cmd: ["./target/release/realsense_d435"]
  },
  interfaces: {
    topics: {
      emits: [
        {
          name: "video_stream",
          qos_profile: "sensor_data",
          message_format: { native_marker: "u32" }
        }
      ]
    }
  }
}
"#;

#[test]
fn nests_conformed_topics_under_iface_name_and_tag() {
    let temp_dir = TempDir::new().expect("temp dir");
    let (_output_dir, user_node, peppy_node_config) = prepare_directories(&temp_dir);
    fs::write(&peppy_node_config, NODE_CONFIG).expect("write node config");

    let extras = vec![
        conformed("depth_camera", "v1", make_topic("depth_v1_marker")),
        conformed("depth_camera", "v2", make_topic("depth_v2_marker")),
        conformed("uvc_camera", "v1", make_topic("uvc_v1_marker")),
    ];

    let peppy_dirs = test_peppy_dirs();
    generate_peppygen_lib(
        PeppygenLanguage::Rust,
        &user_node,
        extras,
        "test-hash",
        &peppy_dirs,
        CrateDeployMode::default(),
        Some(&peppy_node_config),
    )
    .expect("generation should succeed");

    let src = user_node
        .join(config::consts::PEPPYGEN_OUTPUT_PATH)
        .join("src");
    let emit_dir = src.join("emitted_topics");

    // Native artifact lives flat under `emitted_topics/`.
    let native_path = emit_dir.join("video_stream.rs");
    assert!(
        native_path.exists(),
        "native video_stream.rs should exist at {native_path:?}",
    );

    // Conformed artifacts nest under `<iface_name>/<iface_tag>/`.
    let depth_v1 = emit_dir.join("depth_camera/v1/video_stream.rs");
    let depth_v2 = emit_dir.join("depth_camera/v2/video_stream.rs");
    let uvc_v1 = emit_dir.join("uvc_camera/v1/video_stream.rs");
    for path in [&depth_v1, &depth_v2, &uvc_v1] {
        assert!(path.exists(), "expected conformed topic at {path:?}");
    }

    // The container mod.rs files declare every direct child module.
    let depth_mod = fs::read_to_string(emit_dir.join("depth_camera/mod.rs"))
        .expect("depth_camera/mod.rs should exist");
    assert!(
        depth_mod.contains("pub mod v1;"),
        "depth_camera/mod.rs missing v1: {depth_mod}",
    );
    assert!(
        depth_mod.contains("pub mod v2;"),
        "depth_camera/mod.rs missing v2: {depth_mod}",
    );

    // The category file at `src/emitted_topics.rs` lists the four entries:
    // the native leaf and the three interface directories.
    let category_mod =
        fs::read_to_string(src.join("emitted_topics.rs")).expect("emitted_topics.rs should exist");
    for expected in [
        "pub mod video_stream;",
        "pub mod depth_camera;",
        "pub mod uvc_camera;",
    ] {
        assert!(
            category_mod.contains(expected),
            "emitted_topics.rs missing `{expected}`:\n{category_mod}",
        );
    }

    // Each leaf calls `peppylib::TopicMessenger::emit(...)` with the iface
    // segments threaded through. We check that the literal segments match
    // the artifact's origin (or `"_"`, `"_"` for the native leaf).
    let native_src = fs::read_to_string(&native_path).expect("read native");
    assert!(
        native_src.contains("TopicMessenger::emit"),
        "native source should call TopicMessenger::emit:\n{native_src}",
    );
    // prettyplease may emit "_" or normalize whitespace; assert both segments
    // are present in their literal form. The native leaf gets two `"_"`
    // arguments back-to-back.
    let native_underscore_pairs = native_src.matches("\"_\"").count();
    assert!(
        native_underscore_pairs >= 2,
        "native leaf should pass two `\"_\"` iface segments:\n{native_src}",
    );

    let depth_v1_src = fs::read_to_string(&depth_v1).expect("read depth v1");
    assert!(
        depth_v1_src.contains("\"depth_camera\""),
        "depth_v1 leaf should pass iface_name `depth_camera`:\n{depth_v1_src}",
    );
    assert!(
        depth_v1_src.contains("\"v1\""),
        "depth_v1 leaf should pass iface_tag `v1`:\n{depth_v1_src}",
    );

    let depth_v2_src = fs::read_to_string(&depth_v2).expect("read depth v2");
    assert!(
        depth_v2_src.contains("\"depth_camera\"") && depth_v2_src.contains("\"v2\""),
        "depth_v2 leaf should pass `depth_camera`,`v2`:\n{depth_v2_src}",
    );

    let uvc_v1_src = fs::read_to_string(&uvc_v1).expect("read uvc v1");
    assert!(
        uvc_v1_src.contains("\"uvc_camera\"") && uvc_v1_src.contains("\"v1\""),
        "uvc_v1 leaf should pass `uvc_camera`,`v1`:\n{uvc_v1_src}",
    );

    // Distinguishing fields preserve per-leaf message format identity — a
    // belt-and-suspenders check that we didn't accidentally cross-wire the
    // four artifacts.
    assert!(
        native_src.contains("native_marker"),
        "native source should carry its distinguishing field:\n{native_src}",
    );
    assert!(
        depth_v1_src.contains("depth_v1_marker"),
        "depth_v1 source should carry its distinguishing field",
    );
    assert!(
        depth_v2_src.contains("depth_v2_marker"),
        "depth_v2 source should carry its distinguishing field",
    );
    assert!(
        uvc_v1_src.contains("uvc_v1_marker"),
        "uvc_v1 source should carry its distinguishing field",
    );
}

/// Exercises hyphen-to-underscore normalization in the `iface_tag`: a tag
/// `v1-beta` becomes the directory `v1_beta` and the literal `"v1-beta"` on
/// the wire (messaging.rs normalizes hyphens at the boundary — the raw value
/// is the one the generator embeds).
#[test]
fn hyphenated_tag_lands_in_underscore_directory() {
    let temp_dir = TempDir::new().expect("temp dir");
    let (_output_dir, user_node, peppy_node_config) = prepare_directories(&temp_dir);
    fs::write(&peppy_node_config, NODE_CONFIG).expect("write node config");

    let extras = vec![conformed(
        "depth_camera",
        "v1-beta",
        make_topic("hyphen_marker"),
    )];

    let peppy_dirs = test_peppy_dirs();
    generate_peppygen_lib(
        PeppygenLanguage::Rust,
        &user_node,
        extras,
        "test-hash",
        &peppy_dirs,
        CrateDeployMode::default(),
        Some(&peppy_node_config),
    )
    .expect("generation should succeed");

    let emit_dir = user_node
        .join(config::consts::PEPPYGEN_OUTPUT_PATH)
        .join("src")
        .join("emitted_topics");
    let leaf = emit_dir.join("depth_camera/v1_beta/video_stream.rs");
    assert!(
        leaf.exists(),
        "hyphenated tag should land in underscored dir at {leaf:?}",
    );
    let src = fs::read_to_string(&leaf).expect("read leaf");
    assert!(
        src.contains("\"v1-beta\""),
        "generator must pass the raw tag (messaging.rs normalizes hyphens):\n{src}",
    );
}
