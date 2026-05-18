//! Python mirror of `tests/rust/actions_conforms_to.rs`.

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
    language: "python",
    run_cmd: ["python", "main.py"]
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
    let act_dir = pkg.join("exposed_actions");

    let native_path = act_dir.join("move_arm.py");
    let arm_v1 = act_dir.join("arm/v1/move_arm.py");
    let arm_v2 = act_dir.join("arm/v2/move_arm.py");

    for path in [&native_path, &arm_v1, &arm_v2] {
        assert!(path.exists(), "expected action file at {path:?}");
    }

    let native_src = fs::read_to_string(&native_path).expect("read native");
    assert!(
        native_src.contains("ActionMessenger.expose"),
        "native source should call ActionMessenger.expose:\n{native_src}",
    );
    assert!(
        native_src.contains("peppylib.SenderTarget.node("),
        "native leaf should pass `peppylib.Iface.native()`:\n{native_src}",
    );

    let arm_v1_src = fs::read_to_string(&arm_v1).expect("read arm v1");
    assert!(
        arm_v1_src.contains("peppylib.SenderTarget.interface(\"arm\", \"v1\")"),
        "arm v1 leaf should pass `Iface.conformed(\"arm\", \"v1\")`:\n{arm_v1_src}",
    );

    let arm_v2_src = fs::read_to_string(&arm_v2).expect("read arm v2");
    assert!(
        arm_v2_src.contains("peppylib.SenderTarget.interface(\"arm\", \"v2\")"),
        "arm v2 leaf should pass `Iface.conformed(\"arm\", \"v2\")`:\n{arm_v2_src}",
    );
}
