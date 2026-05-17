//! Generator-level test that a node exposing the same action `move` both
//! natively and via two conformed interfaces (`arm:v1`, `arm:v2`) produces
//! distinct generated files AND each `ActionMessenger::expose` call inside
//! those files passes the matching `iface_name`/`iface_tag` literals.

use crate::helpers::{prepare_directories, test_peppy_dirs};
use config::node::{
    ActionServiceEndpoint, ExposedAction, MessageFormat, PeppygenLanguage, QoSProfile, SchemaType,
    TypeToken,
};
use generator::{
    CrateDeployMode, DeploymentInterface, InterfaceOrigin, InterfaceVariant, generate_peppygen_lib,
};
use indexmap::IndexMap;
use std::fs;
use tempfile::TempDir;

fn make_action(marker: &str) -> ExposedAction {
    let mut req: IndexMap<String, SchemaType> = IndexMap::new();
    req.insert(marker.to_string(), SchemaType::Type(TypeToken::I32));
    let mut resp: IndexMap<String, SchemaType> = IndexMap::new();
    resp.insert(format!("{marker}_ack"), SchemaType::Type(TypeToken::Bool));
    ExposedAction {
        name: "move_arm".to_string(),
        goal_service: Some(ActionServiceEndpoint {
            request_message_format: Some(MessageFormat(req)),
            response_message_format: Some(MessageFormat(resp)),
            qos_profile: QoSProfile::Reliable,
        }),
        feedback_topic: None,
        result_service: None,
    }
}

fn conformed(name: &str, tag: &str, action: ExposedAction) -> DeploymentInterface {
    DeploymentInterface::new(InterfaceVariant::ExposedAction {
        action,
        origin: Some(InterfaceOrigin {
            iface_name: name.to_string(),
            iface_tag: tag.to_string(),
        }),
    })
}

const NODE_CONFIG: &str = r#"{
  peppy_schema: "node_v1",
  manifest: {
    name: "arm_driver",
    tag: "v1"
  },
  execution: {
    language: "rust",
    run_cmd: ["./target/release/arm_driver"]
  },
  interfaces: {
    actions: {
      exposes: [
        {
          name: "move_arm",
          goal_service: {
            request_message_format: { native_marker: "i32" },
            response_message_format: { native_marker_ack: "bool" }
          }
        }
      ]
    }
  }
}
"#;

/// One native `move` action plus two conformed `arm:v1`/`arm:v2` interfaces
/// each exposing an action named `move`. Verifies:
///   1. Conformed artifacts nest under
///      `exposed_actions/{iface_name}/{iface_tag}/move_arm.rs` while the native
///      artifact stays flat at `exposed_actions/move_arm.rs`.
///   2. The category `exposed_actions.rs` lists the native leaf and one
///      module entry per conforming interface.
///   3. Each leaf calls `peppylib::ActionMessenger::expose` with the matching
///      iface: `Iface::new("name", "tag")?` for conformed leaves and
///      `Iface::native()` for the native leaf.
///   4. Per-interface marker fields land in the right file.
#[test]
fn nests_conformed_actions_under_iface_name_and_tag() {
    let temp_dir = TempDir::new().expect("temp dir");
    let (_output_dir, user_node, peppy_node_config) = prepare_directories(&temp_dir);
    fs::write(&peppy_node_config, NODE_CONFIG).expect("write node config");

    let extras = vec![
        conformed("arm", "v1", make_action("arm_v1_marker")),
        conformed("arm", "v2", make_action("arm_v2_marker")),
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
    let act_dir = src.join("exposed_actions");

    let native_path = act_dir.join("move_arm.rs");
    let arm_v1 = act_dir.join("arm/v1/move_arm.rs");
    let arm_v2 = act_dir.join("arm/v2/move_arm.rs");

    for path in [&native_path, &arm_v1, &arm_v2] {
        assert!(path.exists(), "expected action file at {path:?}");
    }

    let category_mod = fs::read_to_string(src.join("exposed_actions.rs"))
        .expect("exposed_actions.rs should exist");
    for expected in ["pub mod move_arm;", "pub mod arm;"] {
        assert!(
            category_mod.contains(expected),
            "exposed_actions.rs missing `{expected}`:\n{category_mod}",
        );
    }

    let arm_mod = fs::read_to_string(act_dir.join("arm/mod.rs")).expect("arm/mod.rs should exist");
    assert!(
        arm_mod.contains("pub mod v1;") && arm_mod.contains("pub mod v2;"),
        "arm/mod.rs should declare v1 and v2:\n{arm_mod}",
    );

    let native_src = fs::read_to_string(&native_path).expect("read native");
    assert!(
        native_src.contains("ActionMessenger::expose"),
        "native source should call ActionMessenger::expose:\n{native_src}",
    );
    assert!(
        native_src.contains("Iface::native()"),
        "native leaf should pass `Iface::native()`:\n{native_src}",
    );
    assert!(
        native_src.contains("native_marker"),
        "native source should carry its distinguishing field",
    );

    let arm_v1_src = fs::read_to_string(&arm_v1).expect("read arm v1");
    assert!(
        arm_v1_src.contains("\"arm\"") && arm_v1_src.contains("\"v1\""),
        "arm v1 leaf should pass `arm`,`v1`:\n{arm_v1_src}",
    );
    assert!(
        arm_v1_src.contains("arm_v1_marker"),
        "arm v1 source should carry its distinguishing field",
    );

    let arm_v2_src = fs::read_to_string(&arm_v2).expect("read arm v2");
    assert!(
        arm_v2_src.contains("\"arm\"") && arm_v2_src.contains("\"v2\""),
        "arm v2 leaf should pass `arm`,`v2`:\n{arm_v2_src}",
    );
    assert!(
        arm_v2_src.contains("arm_v2_marker"),
        "arm v2 source should carry its distinguishing field",
    );
}
