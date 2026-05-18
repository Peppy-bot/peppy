//! Python mirror of `tests/rust/conforms_to.rs`: same realsense_d435
//! scenario, but verifying the Python generator emits the corresponding
//! `peppygen/emitted_topics/{iface_name}/{iface_tag}/{topic}.py` files with
//! the right `__init__.py` chains and the matching `iface_name` / `iface_tag`
//! strings inside each emit call.

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
    language: "python",
    run_cmd: ["python", "main.py"]
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

/// Realsense_d435 scenario for the Python generator: one native
/// `video_stream` emit plus three resolved conformed interfaces
/// (`depth_camera:v1`, `depth_camera:v2`, `uvc_camera:v1`) each shaped as
/// `video_stream` with a distinguishing marker field. Verifies that:
///   1. Conformed artifacts nest under
///      `emitted_topics/{iface_name}/{iface_tag}/{topic}.py` while the native
///      artifact stays flat at `emitted_topics/{topic}.py`.
///   2. The `__init__.py` chain at each level imports its direct children.
///   3. Each leaf's emit body passes the matching `iface_name`/`iface_tag`
///      `peppylib.Iface.conformed(...)` expression to the messenger
///      (and `peppylib.Iface.native()` for the native leaf).
///   4. The per-interface marker fields land in their own files — proof the
///      four artifacts weren't cross-wired during generation.
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
        PeppygenLanguage::Python,
        &user_node,
        extras,
        "test-hash",
        &peppy_dirs,
        CrateDeployMode::default(),
        Some(&peppy_node_config),
    )
    .expect("generation should succeed");

    let pkg = user_node
        .join(config::consts::PEPPYGEN_OUTPUT_PATH)
        .join("peppygen");
    let emit_dir = pkg.join("emitted_topics");

    // Native artifact at the flat root.
    let native_path = emit_dir.join("video_stream.py");
    assert!(
        native_path.exists(),
        "native video_stream.py should exist at {native_path:?}",
    );

    // Conformed artifacts nest one level deeper than the rust scaffold —
    // category dir, then iface_name, then iface_tag.
    let depth_v1 = emit_dir.join("depth_camera/v1/video_stream.py");
    let depth_v2 = emit_dir.join("depth_camera/v2/video_stream.py");
    let uvc_v1 = emit_dir.join("uvc_camera/v1/video_stream.py");
    for path in [&depth_v1, &depth_v2, &uvc_v1] {
        assert!(path.exists(), "expected conformed topic at {path:?}");
    }

    // __init__.py chain — each intermediate directory imports its children.
    let root_init = fs::read_to_string(emit_dir.join("__init__.py")).expect("root __init__.py");
    for expected in [
        "from . import video_stream",
        "from . import depth_camera",
        "from . import uvc_camera",
    ] {
        assert!(
            root_init.contains(expected),
            "emitted_topics/__init__.py missing `{expected}`:\n{root_init}",
        );
    }

    let depth_init = fs::read_to_string(emit_dir.join("depth_camera/__init__.py"))
        .expect("depth_camera/__init__.py");
    assert!(
        depth_init.contains("from . import v1") && depth_init.contains("from . import v2"),
        "depth_camera/__init__.py should import v1 and v2:\n{depth_init}",
    );

    // Each leaf's emit body passes a matching `peppylib.SenderTarget` expression
    // to the messenger. Native gets `SenderTarget.node(...)`; conformed leaves
    // pass `SenderTarget.interface("<name>", "<tag>")` with the producer's segments.
    let native_src = fs::read_to_string(&native_path).expect("read native");
    assert!(
        native_src.contains("peppylib.SenderTarget.node("),
        "native source should pass `peppylib.SenderTarget.node(...)`:\n{native_src}",
    );

    let depth_v1_src = fs::read_to_string(&depth_v1).expect("read depth v1");
    assert!(
        depth_v1_src.contains("peppylib.SenderTarget.interface(\"depth_camera\", \"v1\")"),
        "depth_v1 source missing SenderTarget.interface literal:\n{depth_v1_src}",
    );

    let depth_v2_src = fs::read_to_string(&depth_v2).expect("read depth v2");
    assert!(
        depth_v2_src.contains("peppylib.SenderTarget.interface(\"depth_camera\", \"v2\")"),
        "depth_v2 source missing SenderTarget.interface literal:\n{depth_v2_src}",
    );

    let uvc_v1_src = fs::read_to_string(&uvc_v1).expect("read uvc v1");
    assert!(
        uvc_v1_src.contains("peppylib.SenderTarget.interface(\"uvc_camera\", \"v1\")"),
        "uvc_v1 source missing SenderTarget.interface literal:\n{uvc_v1_src}",
    );

    // Distinguishing message-format markers should still be present in their
    // respective files (sanity that we didn't cross-wire the four artifacts).
    assert!(native_src.contains("native_marker"));
    assert!(depth_v1_src.contains("depth_v1_marker"));
    assert!(depth_v2_src.contains("depth_v2_marker"));
    assert!(uvc_v1_src.contains("uvc_v1_marker"));
}

/// Exercises hyphen-to-underscore normalization in the `iface_tag` for the
/// Python generator: a tag `v1-beta` becomes the directory `v1_beta` (Python
/// identifiers can't carry hyphens) while the literal `"v1-beta"` is still
/// embedded in the emit body — the messaging layer normalizes hyphens at the
/// wire boundary, so the generator keeps the raw value.
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
        PeppygenLanguage::Python,
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
        .join("peppygen")
        .join("emitted_topics");
    let leaf = emit_dir.join("depth_camera/v1_beta/video_stream.py");
    assert!(
        leaf.exists(),
        "hyphenated tag should land under underscored dir at {leaf:?}",
    );
    let src = fs::read_to_string(&leaf).expect("read leaf");
    assert!(
        src.contains("peppylib.SenderTarget.interface(\"depth_camera\", \"v1-beta\")"),
        "generator should pass the raw tag (messaging normalizes hyphens):\n{src}",
    );
}
