//! Generator-level test that a node exposing the same action `move_arm` both
//! natively and via two contract-backed slots (`arm:v1`, `arm:v2`, each with a
//! `ContractOrigin`) produces distinct generated files AND each
//! `ActionMessenger::expose` call inside those files passes the matching
//! `contract_name`/`contract_tag` literals.

use crate::helpers::{prepare_directories, test_peppy_dirs};
use config::node::{
    GoalServiceEndpoint, MessageFormat, NativeExposedAction, PeppygenLanguage, SchemaType,
    TypeToken,
};
use generator::{
    ContractOrigin, CrateDeployMode, DeploymentInterface, InterfaceVariant, generate_peppygen_lib,
};
use indexmap::IndexMap;
use std::fs;
use tempfile::TempDir;

fn make_action(marker: &str) -> NativeExposedAction {
    let mut req: IndexMap<String, SchemaType> = IndexMap::new();
    req.insert(marker.to_string(), SchemaType::Type(TypeToken::I32));
    let mut resp: IndexMap<String, SchemaType> = IndexMap::new();
    resp.insert(format!("{marker}_ack"), SchemaType::Type(TypeToken::Bool));
    NativeExposedAction {
        name: "move_arm".to_string(),
        goal_service: Some(GoalServiceEndpoint {
            request_message_format: Some(MessageFormat(req)),
            response_message_format: Some(MessageFormat(resp)),
        }),
        feedback_topic: None,
        result_service: None,
    }
}

fn contract_backed(
    link_id: &str,
    name: &str,
    tag: &str,
    action: NativeExposedAction,
) -> DeploymentInterface {
    DeploymentInterface::new(InterfaceVariant::ExposedAction {
        action,
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

/// One native `move_arm` action plus two contract-backed `arm:v1`/`arm:v2`
/// slots each exposing an action named `move_arm`. Verifies:
///   1. Contract-backed artifacts nest under
///      `exposed_actions/{contract_name}/{contract_tag}/move_arm.rs` while the
///      native artifact stays flat at `exposed_actions/move_arm.rs`.
///   2. The category `exposed_actions.rs` lists the native leaf and one
///      module entry per implemented contract.
///   3. Each leaf calls `peppylib::ActionMessenger::expose` with the matching
///      sender target: `SenderTarget::contract("name", "tag")?` for contract-backed
///      leaves and `SenderTarget::node("name", "tag")?` for the native leaf.
///   4. Per-interface marker fields land in the right file.
#[test]
fn nests_contract_backed_actions_under_link_id() {
    let temp_dir = TempDir::new_in(crate::helpers::test_tmp_root()).expect("temp dir");
    let (_output_dir, user_node, peppy_node_config) = prepare_directories(&temp_dir);
    fs::write(&peppy_node_config, NODE_CONFIG).expect("write node config");

    let extras = vec![
        contract_backed("arm_v1", "arm", "v1", make_action("arm_v1_marker")),
        contract_backed("arm_v2", "arm", "v2", make_action("arm_v2_marker")),
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
    let arm_v1 = act_dir.join("arm_v1/move_arm.rs");
    let arm_v2 = act_dir.join("arm_v2/move_arm.rs");

    for path in [&native_path, &arm_v1, &arm_v2] {
        assert!(path.exists(), "expected action file at {path:?}");
    }

    let category_mod = fs::read_to_string(src.join("exposed_actions.rs"))
        .expect("exposed_actions.rs should exist");
    for expected in ["pub mod move_arm;", "pub mod arm_v1;", "pub mod arm_v2;"] {
        assert!(
            category_mod.contains(expected),
            "exposed_actions.rs missing `{expected}`:\n{category_mod}",
        );
    }

    let arm_v1_mod =
        fs::read_to_string(act_dir.join("arm_v1/mod.rs")).expect("arm_v1/mod.rs should exist");
    assert!(
        arm_v1_mod.contains("pub mod move_arm;"),
        "arm_v1/mod.rs should declare move_arm:\n{arm_v1_mod}",
    );
    let arm_v2_mod =
        fs::read_to_string(act_dir.join("arm_v2/mod.rs")).expect("arm_v2/mod.rs should exist");
    assert!(
        arm_v2_mod.contains("pub mod move_arm;"),
        "arm_v2/mod.rs should declare move_arm:\n{arm_v2_mod}",
    );

    let native_src = fs::read_to_string(&native_path).expect("read native");
    assert!(
        native_src.contains("ConcurrentAction::expose"),
        "native source should call ConcurrentAction::expose:\n{native_src}",
    );
    assert!(
        native_src.contains("SenderTarget::node("),
        "native leaf should pass `SenderTarget::node(...)`:\n{native_src}",
    );
    assert!(
        native_src.contains("native_marker") && native_src.contains("native_marker_ack"),
        "native source should carry its declared request and response fields",
    );

    let arm_v1_src = fs::read_to_string(&arm_v1).expect("read arm v1");
    assert!(
        arm_v1_src.contains("SenderTarget::contract("),
        "arm v1 leaf should be contract-addressed via `SenderTarget::contract(...)`:\n{arm_v1_src}",
    );
    assert!(
        arm_v1_src.contains("\"arm\"") && arm_v1_src.contains("\"v1\""),
        "arm v1 leaf should pass `arm`,`v1`:\n{arm_v1_src}",
    );
    assert!(
        arm_v1_src.contains("arm_v1_marker") && arm_v1_src.contains("arm_v1_marker_ack"),
        "arm v1 source should carry its declared request and response fields",
    );

    let arm_v2_src = fs::read_to_string(&arm_v2).expect("read arm v2");
    assert!(
        arm_v2_src.contains("SenderTarget::contract("),
        "arm v2 leaf should be contract-addressed via `SenderTarget::contract(...)`:\n{arm_v2_src}",
    );
    assert!(
        arm_v2_src.contains("\"arm\"") && arm_v2_src.contains("\"v2\""),
        "arm v2 leaf should pass `arm`,`v2`:\n{arm_v2_src}",
    );
    assert!(
        arm_v2_src.contains("arm_v2_marker") && arm_v2_src.contains("arm_v2_marker_ack"),
        "arm v2 source should carry its declared request and response fields",
    );
}
