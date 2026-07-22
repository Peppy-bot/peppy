//! Python mirror of `tests/rust/topics_implements.rs`: same realsense_d435
//! scenario, but verifying the Python generator emits the corresponding
//! `peppygen/emitted_topics/{link_id}/{topic}.py` files
//! with the right `__init__.py` chains and the matching `contract_name` /
//! `contract_tag` strings inside each declare_publisher call.

use crate::helpers::{prepare_directories, test_peppy_dirs};
use config::node::{
    MessageFormat, NativeEmittedTopic, PeppygenLanguage, QoSProfile, SchemaType, TypeToken,
};
use generator::{
    ContractOrigin, CrateDeployMode, DeploymentInterface, InterfaceVariant, generate_peppygen_lib,
};
use indexmap::IndexMap;
use std::fs;
use tempfile::TempDir;

fn make_topic(distinguishing_field: &str) -> NativeEmittedTopic {
    let mut fields: IndexMap<String, SchemaType> = IndexMap::new();
    fields.insert(
        distinguishing_field.to_string(),
        SchemaType::Type(TypeToken::U32),
    );
    NativeEmittedTopic {
        name: "video_stream".to_string(),
        qos_profile: QoSProfile::SensorData,
        message_format: Some(MessageFormat(fields)),
    }
}

fn contract_backed(
    link_id: &str,
    name: &str,
    tag: &str,
    topic: NativeEmittedTopic,
) -> DeploymentInterface {
    DeploymentInterface::new(InterfaceVariant::EmittedTopic {
        topic,
        origin: Some(ContractOrigin {
            link_id: link_id.to_string(),
            contract_name: name.to_string(),
            contract_tag: tag.to_string(),
        }),
    })
}

const NODE_CONFIG: &str = r#"{
  peppy_schema: "node/v1",
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
/// `video_stream` publisher plus three resolved contract-backed interfaces
/// (`depth_camera:v1`, `depth_camera:v2`, `uvc_camera:v1`) each shaped as
/// `video_stream` with a distinguishing marker field. Verifies that:
///   1. Contract-backed artifacts nest under
///      `emitted_topics/{link_id}/{topic}.py` while the
///      native artifact stays flat at `emitted_topics/{topic}.py`.
///   2. The `__init__.py` chain at each level imports its direct children.
///   3. Each leaf's declare_publisher body passes the matching sender target to
///      the messenger: `peppylib.SenderTarget.contract(...)` for contract-backed
///      leaves and `peppylib.SenderTarget.node(...)` for the native leaf.
///   4. The per-interface marker fields land in their own files, proof the
///      four artifacts weren't cross-wired during generation.
#[test]
fn nests_contract_backed_topics_under_link_id() {
    let temp_dir = TempDir::new_in(crate::helpers::test_tmp_root()).expect("temp dir");
    let (_output_dir, user_node, peppy_node_config) = prepare_directories(&temp_dir);
    fs::write(&peppy_node_config, NODE_CONFIG).expect("write node config");

    let extras = vec![
        contract_backed("depth_v1", "depth_camera", "v1", make_topic("depth_v1_marker")),
        contract_backed("depth_v2", "depth_camera", "v2", make_topic("depth_v2_marker")),
        contract_backed("uvc_v1", "uvc_camera", "v1", make_topic("uvc_v1_marker")),
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

    // Contract-backed artifacts nest under `<link_id>/`.
    let depth_v1 = emit_dir.join("depth_v1/video_stream.py");
    let depth_v2 = emit_dir.join("depth_v2/video_stream.py");
    let uvc_v1 = emit_dir.join("uvc_v1/video_stream.py");
    for path in [&depth_v1, &depth_v2, &uvc_v1] {
        assert!(path.exists(), "expected contract-backed topic at {path:?}");
    }

    // __init__.py chain: each intermediate directory imports its children.
    let root_init = fs::read_to_string(emit_dir.join("__init__.py")).expect("root __init__.py");
    for expected in [
        "from . import video_stream",
        "from . import depth_v1",
        "from . import depth_v2",
        "from . import uvc_v1",
    ] {
        assert!(
            root_init.contains(expected),
            "emitted_topics/__init__.py missing `{expected}`:\n{root_init}",
        );
    }

    let depth_v1_init = fs::read_to_string(emit_dir.join("depth_v1/__init__.py"))
        .expect("depth_v1/__init__.py");
    assert!(
        depth_v1_init.contains("from . import video_stream"),
        "depth_v1/__init__.py should import video_stream:\n{depth_v1_init}",
    );
    let depth_v2_init = fs::read_to_string(emit_dir.join("depth_v2/__init__.py"))
        .expect("depth_v2/__init__.py");
    assert!(
        depth_v2_init.contains("from . import video_stream"),
        "depth_v2/__init__.py should import video_stream:\n{depth_v2_init}",
    );

    // Each leaf's declare_publisher body passes a matching `peppylib.SenderTarget`
    // expression to the messenger. Native gets `SenderTarget.node(...)`; contract-backed leaves
    // pass `SenderTarget.contract("<name>", "<tag>")` with the producer's segments.
    let native_src = fs::read_to_string(&native_path).expect("read native");
    assert!(
        native_src.contains("peppylib.SenderTarget.node("),
        "native source should pass `peppylib.SenderTarget.node(...)`:\n{native_src}",
    );

    let depth_v1_src = fs::read_to_string(&depth_v1).expect("read depth v1");
    assert!(
        depth_v1_src.contains("peppylib.SenderTarget.contract(\"depth_camera\", \"v1\")"),
        "depth_v1 source missing SenderTarget.contract literal:\n{depth_v1_src}",
    );

    let depth_v2_src = fs::read_to_string(&depth_v2).expect("read depth v2");
    assert!(
        depth_v2_src.contains("peppylib.SenderTarget.contract(\"depth_camera\", \"v2\")"),
        "depth_v2 source missing SenderTarget.contract literal:\n{depth_v2_src}",
    );

    let uvc_v1_src = fs::read_to_string(&uvc_v1).expect("read uvc v1");
    assert!(
        uvc_v1_src.contains("peppylib.SenderTarget.contract(\"uvc_camera\", \"v1\")"),
        "uvc_v1 source missing SenderTarget.contract literal:\n{uvc_v1_src}",
    );

    // Distinguishing message-format markers should still be present in their
    // respective files (sanity that we didn't cross-wire the four artifacts).
    assert!(native_src.contains("native_marker"));
    assert!(depth_v1_src.contains("depth_v1_marker"));
    assert!(depth_v2_src.contains("depth_v2_marker"));
    assert!(uvc_v1_src.contains("uvc_v1_marker"));

    // Capnp schemas are resolved via `importlib.resources.files("peppygen")`,
    // which is independent of the calling file's depth. This regressed once
    // when the loader used `_PKG_DIR = Path(__file__).parent.parent` (fine
    // for the flat native path but one level short for nested contract-backed
    // artifacts at `peppygen/<category>/<link_id>/<leaf>.py`), which made
    // `capnp.load()` raise silently inside the asyncio loop and hung the
    // consumer. All four files (native + three contract-backed) should now produce
    // the same loader form.
    let expected_loader = "files(\"peppygen\") / \"capnp\" /";
    for (label, src) in [
        ("native", &native_src),
        ("depth_v1", &depth_v1_src),
        ("depth_v2", &depth_v2_src),
        ("uvc_v1", &uvc_v1_src),
    ] {
        assert!(
            src.contains(expected_loader),
            "{label} source should load schema via `{expected_loader}`:\n{src}",
        );
        assert!(
            !src.contains("_PKG_DIR"),
            "{label} source should no longer reference the legacy `_PKG_DIR` \
             constant:\n{src}",
        );
    }
}

/// Exercises hyphen-to-underscore normalization of the slot `link_id` for the
/// Python generator: a link_id `depth-beta` becomes the directory `depth_beta`
/// (Python identifiers can't carry hyphens) while the literal `"v1-beta"` tag is
/// still embedded in the declare_publisher body; the messaging layer normalizes hyphens
/// at the wire boundary, so the generator keeps the raw value.
#[test]
fn hyphenated_link_id_lands_in_underscore_directory() {
    let temp_dir = TempDir::new_in(crate::helpers::test_tmp_root()).expect("temp dir");
    let (_output_dir, user_node, peppy_node_config) = prepare_directories(&temp_dir);
    fs::write(&peppy_node_config, NODE_CONFIG).expect("write node config");

    let extras = vec![contract_backed(
        "depth-beta",
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
    let leaf = emit_dir.join("depth_beta/video_stream.py");
    assert!(
        leaf.exists(),
        "hyphenated link_id should land in an underscored dir at {leaf:?}",
    );
    let src = fs::read_to_string(&leaf).expect("read leaf");
    assert!(
        src.contains("peppylib.SenderTarget.contract(\"depth_camera\", \"v1-beta\")"),
        "generator should pass the raw tag (messaging normalizes hyphens):\n{src}",
    );
}
