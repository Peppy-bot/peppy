//! Regression guard: consumed services scope their cap'n proto schema by
//! producer node, so two consumers depending on services that share a
//! `name` but come from different producer nodes don't collide.
//!
//! Concretely, `add_consumed_service` ([rust.rs `add_consumed_service`])
//! registers each schema under
//! `method_label = "poll_{producer_node}_{service_name}"`, NOT under the
//! bare `service_name`, so the cross-producer same-service-name scenario
//! produces two distinct capnp files (one per producer) and the consumers'
//! deserializers can each reference their own producer's exact message
//! shape, even when the two producers' formats diverge.
//!
//! Today this works. This test pins that down. If a future refactor ever
//! drops the producer-node prefix from the service schema key (making
//! consumed_service susceptible to the same dedup divergence that bit
//! consumed_topic), the assertion on the per-producer capnp file pair, or
//! the build itself, will fail.
//!
//! The Python counterpart lives in
//! `tests/python/consumed_services_dedup.rs`.

use config::consts::{NODE_CONFIG_FILE, PEPPYGEN_OUTPUT_PATH};
use config::node::{ConsumedService, MessageFormat, PeppygenLanguage};
use generator::{DependencyContext, DeploymentInterface, InterfaceVariant, generate_peppygen_lib};
use std::fs;
use tempfile::TempDir;

use crate::helpers;

const NODE_CONFIG: &str = r#"{
  peppy_schema: "node/v1",
  manifest: {
    name: "test_consumer",
    tag: "v1",
    depends_on: {
      nodes: [
        { name: "uvc_camera", tag: "v1", link_id: "front_cam" },
        { name: "rtsp_camera", tag: "v1", link_id: "rear_cam" }
      ]
    }
  },
  execution: {
    language: "rust",
    run_cmd: ["./target/debug/test_consumer"]
  }
}"#;

const FRONT_CONSUMER: &str = r#"{ link_id: "front_cam", name: "enable" }"#;
const REAR_CONSUMER: &str = r#"{ link_id: "rear_cam",  name: "enable" }"#;

/// `uvc_camera::enable`: request `{ enabled: bool }`, response `{ ok: bool }`.
const FRONT_REQUEST_FORMAT: &str = r#"{ enabled: "bool" }"#;
const FRONT_RESPONSE_FORMAT: &str = r#"{ ok: "bool" }"#;

/// `rtsp_camera::enable`: a deliberately *different* shape that only happens
/// to share the service name. Different field set on both request and
/// response. If schemas were keyed by bare service name, the deduplicated
/// capnp would only fit one producer and the other consumer's generated
/// deserializer would either fail to compile (mismatched fields) or
/// silently misread payloads at runtime.
const REAR_REQUEST_FORMAT: &str = r#"{ enabled: "bool", intensity: "u32" }"#;
const REAR_RESPONSE_FORMAT: &str = r#"{ status_code: "i32" }"#;

const USER_MAIN: &str = r#"
use peppygen::consumed_services::{front_cam_enable, rear_cam_enable};

#[allow(dead_code)]
fn _references_both_link_keyed_modules() {
    let _front: fn(_, _, _) -> _ = front_cam_enable::poll;
    let _rear: fn(_, _, _) -> _ = rear_cam_enable::poll;
}

fn main() {}
"#;

#[test]
fn rust_cross_producer_same_service_name_keeps_schemas_separate() {
    let temp_dir =
        TempDir::new_in(crate::helpers::test_tmp_root()).expect("failed to create temp directory");
    let user_node_dir = temp_dir.path().join("user_node");
    fs::create_dir_all(&user_node_dir).expect("failed to create user_node directory");

    fs::write(user_node_dir.join(NODE_CONFIG_FILE), NODE_CONFIG)
        .expect("failed to write peppy.json5");

    let front_service: ConsumedService =
        serde_json5::from_str(FRONT_CONSUMER).expect("failed to parse front consumed service");
    let rear_service: ConsumedService =
        serde_json5::from_str(REAR_CONSUMER).expect("failed to parse rear consumed service");
    let parse_fmt = |raw: &str| -> MessageFormat {
        serde_json5::from_str(raw).expect("failed to parse message format")
    };

    let interfaces = vec![
        DeploymentInterface::new(InterfaceVariant::ConsumedService {
            service: front_service,
            request_format: parse_fmt(FRONT_REQUEST_FORMAT),
            response_format: parse_fmt(FRONT_RESPONSE_FORMAT),
            dependency: DependencyContext::native("uvc_camera", "v1", "uvc_camera"),
        }),
        DeploymentInterface::new(InterfaceVariant::ConsumedService {
            service: rear_service,
            request_format: parse_fmt(REAR_REQUEST_FORMAT),
            response_format: parse_fmt(REAR_RESPONSE_FORMAT),
            dependency: DependencyContext::native("rtsp_camera", "v1", "rtsp_camera"),
        }),
    ];

    generate_peppygen_lib(
        PeppygenLanguage::Rust,
        &user_node_dir,
        interfaces,
        "test-hash",
        &helpers::test_peppy_dirs(),
        Default::default(),
        None,
    )
    .expect("failed to generate peppygen lib");

    let peppygen_dir = user_node_dir.join(PEPPYGEN_OUTPUT_PATH);
    let front_module = peppygen_dir.join("src/consumed_services/front_cam_enable.rs");
    let rear_module = peppygen_dir.join("src/consumed_services/rear_cam_enable.rs");
    assert!(
        front_module.exists(),
        "front consumer module missing at {}",
        front_module.display()
    );
    assert!(
        rear_module.exists(),
        "rear consumer module missing at {}",
        rear_module.display()
    );

    // The producer-node-scoped schema keys must yield distinct capnp files
    // for each producer. If both producers ever shared a single capnp file,
    // one consumer's payload shape would lose to the other's; this is the
    // file-level evidence that the dedup bug from consumed_topic doesn't
    // apply here.
    let capnp_dir = peppygen_dir.join("src/capnp");
    let front_request_capnp = capnp_dir.join("poll_uvc_camera_enable_message.capnp");
    let front_response_capnp = capnp_dir.join("poll_uvc_camera_enable_response_message.capnp");
    let rear_request_capnp = capnp_dir.join("poll_rtsp_camera_enable_message.capnp");
    let rear_response_capnp = capnp_dir.join("poll_rtsp_camera_enable_response_message.capnp");
    for path in [
        &front_request_capnp,
        &front_response_capnp,
        &rear_request_capnp,
        &rear_response_capnp,
    ] {
        assert!(
            path.exists(),
            "expected producer-scoped capnp file at {}",
            path.display()
        );
    }

    // And each capnp file must encode that producer's exact format.
    let front_request_text =
        fs::read_to_string(&front_request_capnp).expect("read front request capnp");
    let rear_request_text =
        fs::read_to_string(&rear_request_capnp).expect("read rear request capnp");
    assert!(
        front_request_text.contains("enabled @"),
        "front request capnp should encode `enabled`, got:\n{front_request_text}"
    );
    assert!(
        !front_request_text.contains("intensity @"),
        "front request capnp must NOT carry the rear producer's `intensity` field; \
         that would prove the schemas got deduplicated. Got:\n{front_request_text}"
    );
    assert!(
        rear_request_text.contains("intensity @"),
        "rear request capnp should encode `intensity`, got:\n{rear_request_text}"
    );

    helpers::init_cargo_user_node(&user_node_dir);
    let src_dir = user_node_dir.join("src");
    fs::create_dir_all(&src_dir).expect("failed to create src dir");
    fs::write(src_dir.join("main.rs"), USER_MAIN).expect("failed to write user main.rs");

    // Build must succeed: each consumer references a struct that the
    // capnp file for its own producer actually defines.
    helpers::compile_project(&user_node_dir);
}
