//! Python mirror of `tests/rust/services_conforms_to.rs`: verifies the
//! Python generator nests conformed services under
//! `peppygen/exposed_services/{iface_name}/{iface_tag}/<service>.py` and
//! splices the matching `peppylib.SenderTarget.interface(...)` expression
//! into each `ServiceMessenger.listen` call (or
//! `peppylib.SenderTarget.node(...)` for the native leaf).

use crate::helpers::{prepare_directories, test_peppy_dirs};
use config::node::{ExposedService, MessageFormat, PeppygenLanguage, SchemaType, TypeToken};
use generator::{
    CrateDeployMode, DeploymentInterface, InterfaceOrigin, InterfaceVariant, generate_peppygen_lib,
};
use indexmap::IndexMap;
use std::fs;
use tempfile::TempDir;

fn make_service(marker: &str) -> ExposedService {
    let mut req: IndexMap<String, SchemaType> = IndexMap::new();
    req.insert(marker.to_string(), SchemaType::Type(TypeToken::Bool));
    let mut resp: IndexMap<String, SchemaType> = IndexMap::new();
    resp.insert(format!("{marker}_ack"), SchemaType::Type(TypeToken::Bool));
    ExposedService {
        name: "control".to_string(),
        request_message_format: Some(MessageFormat(req)),
        response_message_format: Some(MessageFormat(resp)),
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
  peppy_schema: "node_v1",
  manifest: {
    name: "control_panel",
    tag: "v1"
  },
  execution: {
    language: "python",
    run_cmd: ["python", "main.py"]
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

#[test]
fn nests_conformed_services_under_iface_name_and_tag() {
    let temp_dir = TempDir::new().expect("temp dir");
    let (_output_dir, user_node, peppy_node_config) = prepare_directories(&temp_dir);
    fs::write(&peppy_node_config, NODE_CONFIG).expect("write node config");

    let extras = vec![
        conformed("camera", "v1", make_service("camera_v1_marker")),
        conformed("arm", "v2", make_service("arm_v2_marker")),
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
    let svc_dir = pkg.join("exposed_services");

    let native_path = svc_dir.join("control.py");
    let camera_v1 = svc_dir.join("camera/v1/control.py");
    let arm_v2 = svc_dir.join("arm/v2/control.py");

    for path in [&native_path, &camera_v1, &arm_v2] {
        assert!(path.exists(), "expected service file at {path:?}");
    }

    let native_src = fs::read_to_string(&native_path).expect("read native");
    assert!(
        native_src.contains("ServiceMessenger.listen"),
        "native source should call ServiceMessenger.listen:\n{native_src}",
    );
    assert!(
        native_src.contains("peppylib.SenderTarget.node("),
        "native leaf should pass `peppylib.SenderTarget.node(...)`:\n{native_src}",
    );

    let camera_v1_src = fs::read_to_string(&camera_v1).expect("read camera v1");
    assert!(
        camera_v1_src.contains("peppylib.SenderTarget.interface(\"camera\", \"v1\")"),
        "camera v1 leaf should pass `SenderTarget.interface(\"camera\", \"v1\")`:\n{camera_v1_src}",
    );

    let arm_v2_src = fs::read_to_string(&arm_v2).expect("read arm v2");
    assert!(
        arm_v2_src.contains("peppylib.SenderTarget.interface(\"arm\", \"v2\")"),
        "arm v2 leaf should pass `SenderTarget.interface(\"arm\", \"v2\")`:\n{arm_v2_src}",
    );
}
