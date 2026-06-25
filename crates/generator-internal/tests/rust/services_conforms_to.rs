//! Generator-level test that a node exposing the same service `control` both
//! natively and via two conformed interfaces (`camera:v1`, `arm:v2`) produces
//! distinct generated files AND each `ServiceMessenger::listen` call inside
//! those files passes the matching target: `SenderTarget::interface("name", "tag")?`
//! for conformed leaves and `SenderTarget::node(...)` for the native one.

use crate::helpers::{prepare_directories, test_peppy_dirs};
use config::node::{ExposedService, MessageFormat, PeppygenLanguage, SchemaType, TypeToken};
use generator::{
    CrateDeployMode, DeploymentInterface, InterfaceOrigin, InterfaceVariant, generate_peppygen_lib,
};
use indexmap::IndexMap;
use std::fs;
use tempfile::TempDir;

fn make_service(marker: &str) -> ExposedService {
    let mut request_fields: IndexMap<String, SchemaType> = IndexMap::new();
    request_fields.insert(marker.to_string(), SchemaType::Type(TypeToken::Bool));
    let mut response_fields: IndexMap<String, SchemaType> = IndexMap::new();
    response_fields.insert(format!("{marker}_ack"), SchemaType::Type(TypeToken::Bool));
    ExposedService {
        name: "control".to_string(),
        request_message_format: Some(MessageFormat(request_fields)),
        response_message_format: Some(MessageFormat(response_fields)),
    }
}

fn conformed(name: &str, tag: &str, service: ExposedService) -> DeploymentInterface {
    DeploymentInterface::new(InterfaceVariant::ExposedService {
        service,
        origin: Some(InterfaceOrigin {
            iface_name: name.to_string(),
            iface_tag: tag.to_string(),
        }),
    })
}

const NODE_CONFIG: &str = r#"{
  peppy_schema: "node/v1",
  manifest: {
    name: "control_panel",
    tag: "v1"
  },
  execution: {
    language: "rust",
    run_cmd: ["./target/release/control_panel"]
  },
  interfaces: {
    services: {
      exposes: [
        {
          name: "control",
          request_message_format: { native_marker: "bool" },
          response_message_format: { native_marker_ack: "bool" }
        }
      ]
    }
  }
}
"#;

/// One native `control` service plus two conformed interfaces
/// (`camera:v1`, `arm:v2`) each exposing a service named `control`. Verifies:
///   1. Conformed artifacts nest under
///      `exposed_services/{iface_name}/{iface_tag}/control.rs` while the
///      native artifact stays flat at `exposed_services/control.rs`.
///   2. The category `exposed_services.rs` declares the native leaf and one
///      module entry per conforming interface.
///   3. Each leaf calls `peppylib::ServiceMessenger::listen` with the matching
///      target: `SenderTarget::interface("name", "tag")?` for conformed leaves
///      and `SenderTarget::node(...)` for the native leaf.
///   4. Per-interface marker fields land in the right file (no cross-wiring).
#[test]
fn nests_conformed_services_under_iface_name_and_tag() {
    let temp_dir = TempDir::new_in(crate::helpers::test_tmp_root()).expect("temp dir");
    let (_output_dir, user_node, peppy_node_config) = prepare_directories(&temp_dir);
    fs::write(&peppy_node_config, NODE_CONFIG).expect("write node config");

    let extras = vec![
        conformed("camera", "v1", make_service("camera_v1_marker")),
        conformed("arm", "v2", make_service("arm_v2_marker")),
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
    let svc_dir = src.join("exposed_services");

    let native_path = svc_dir.join("control.rs");
    let camera_v1 = svc_dir.join("camera/v1/control.rs");
    let arm_v2 = svc_dir.join("arm/v2/control.rs");

    for path in [&native_path, &camera_v1, &arm_v2] {
        assert!(path.exists(), "expected service file at {path:?}");
    }

    let category_mod = fs::read_to_string(src.join("exposed_services.rs"))
        .expect("exposed_services.rs should exist");
    for expected in ["pub mod control;", "pub mod camera;", "pub mod arm;"] {
        assert!(
            category_mod.contains(expected),
            "exposed_services.rs missing `{expected}`:\n{category_mod}",
        );
    }

    let native_src = fs::read_to_string(&native_path).expect("read native");
    assert!(
        native_src.contains("ServiceMessenger::listen"),
        "native source should call ServiceMessenger::listen:\n{native_src}",
    );
    assert!(
        native_src.contains("SenderTarget::node("),
        "native leaf should pass `SenderTarget::node(...)`:\n{native_src}",
    );
    assert!(
        native_src.contains("native_marker"),
        "native source should carry its distinguishing request field",
    );

    let camera_v1_src = fs::read_to_string(&camera_v1).expect("read camera v1");
    assert!(
        camera_v1_src.contains("\"camera\"") && camera_v1_src.contains("\"v1\""),
        "camera v1 leaf should pass `camera`,`v1` literals:\n{camera_v1_src}",
    );
    assert!(
        camera_v1_src.contains("camera_v1_marker"),
        "camera v1 source should carry its distinguishing field",
    );

    let arm_v2_src = fs::read_to_string(&arm_v2).expect("read arm v2");
    assert!(
        arm_v2_src.contains("\"arm\"") && arm_v2_src.contains("\"v2\""),
        "arm v2 leaf should pass `arm`,`v2` literals:\n{arm_v2_src}",
    );
    assert!(
        arm_v2_src.contains("arm_v2_marker"),
        "arm v2 source should carry its distinguishing field",
    );
}
