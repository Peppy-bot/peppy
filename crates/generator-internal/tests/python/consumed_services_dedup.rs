//! Python counterpart of the Rust regression guard in
//! `tests/rust/consumed_services_dedup.rs`.
//!
//! Locks down the per-producer scoping of consumed-service cap'n proto
//! schemas: two consumed services sharing a `name` but coming from
//! different producer nodes must produce two distinct capnp files (one
//! per producer), not a single deduplicated one. That's why the
//! consumed-topic divergence bug doesn't apply here even when the two
//! producers expose services with completely different message formats.

use crate::helpers::{init_python_project_venv, init_python_user_node, test_peppy_dirs};
use config::consts::{NODE_CONFIG_FILE, PEPPYGEN_OUTPUT_PATH};
use config::node::{ConsumedService, MessageFormat, PeppygenLanguage};
use generator::{DependencyContext, DeploymentInterface, InterfaceVariant, generate_peppygen_lib};
use std::fs;
use std::process::{Command, Stdio};
use tempfile::TempDir;

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
    language: "python",
    build_cmd: ["uv", "sync"],
    run_cmd: ["uv", "run", "test_consumer"]
  }
}"#;

const FRONT_CONSUMER: &str = r#"{ link_id: "front_cam", name: "enable" }"#;
const REAR_CONSUMER: &str = r#"{ link_id: "rear_cam",  name: "enable" }"#;

const FRONT_REQUEST_FORMAT: &str = r#"{ enabled: "bool" }"#;
const FRONT_RESPONSE_FORMAT: &str = r#"{ ok: "bool" }"#;
const REAR_REQUEST_FORMAT: &str = r#"{ enabled: "bool", intensity: "u32" }"#;
const REAR_RESPONSE_FORMAT: &str = r#"{ status_code: "i32" }"#;

/// Imports both per-link consumer modules, calls each module's lazy
/// `_*_capnp()` loader to force pycapnp's schema parse, and accesses the
/// cap'n proto struct each one's `_deserialize_response` references. If
/// the dedup logic ever drifted so that two consumed services with the
/// same name shared a single capnp module, the second consumer's
/// `getattr` would either resolve to the wrong producer's struct
/// (silent corruption) or raise `AttributeError`. By comparing the
/// modules returned by the loaders we also pin the per-producer scoping
/// at the module level; they must NOT be the same object.
const PYTHON_PROBE: &str = r#"
import importlib
import inspect
import re
import sys

def referenced_struct(mod, fn_name):
    fn = getattr(mod, fn_name)
    src = inspect.getsource(fn)
    m = re.search(r"_capnp\(\)\.(\w+)\.from_bytes", src)
    if m is None:
        sys.exit(f"could not locate capnp struct reference in {mod.__name__}.{fn_name}")
    return m.group(1)

def capnp_loaders(mod):
    return [
        getattr(mod, name)
        for name in dir(mod)
        if name.endswith("_capnp") and name.startswith("_") and callable(getattr(mod, name))
    ]

front = importlib.import_module("peppygen.consumed_services.front_cam_enable")
rear = importlib.import_module("peppygen.consumed_services.rear_cam_enable")

front_loaders = capnp_loaders(front)
rear_loaders = capnp_loaders(rear)
assert front_loaders, "no capnp loaders found in front_cam_enable"
assert rear_loaders, "no capnp loaders found in rear_cam_enable"

# Per-producer scoping check: the loader functions in the two modules must
# resolve to DIFFERENT pycapnp modules. If they returned the same object,
# the dedup would have collapsed the two producers' schemas into one.
front_caps = [fn() for fn in front_loaders]
rear_caps = [fn() for fn in rear_loaders]
for fc in front_caps:
    for rc in rear_caps:
        if fc is rc:
            sys.exit(
                f"front and rear consumer share capnp module {fc!r}; the two "
                f"producers' schemas were deduplicated"
            )

# Each consumer's referenced struct must actually exist in its own
# producer's capnp module: the front module's struct lives in the front
# capnp, the rear module's in the rear capnp.
front_struct = referenced_struct(front, "_deserialize_response")
rear_struct = referenced_struct(rear, "_deserialize_response")
front_resolved = any(hasattr(c, front_struct) for c in front_caps)
rear_resolved = any(hasattr(c, rear_struct) for c in rear_caps)
if not front_resolved:
    sys.exit(f"front consumer references {front_struct!r}, not found on its loaders")
if not rear_resolved:
    sys.exit(f"rear consumer references {rear_struct!r}, not found on its loaders")

print(f"front={front_struct} rear={rear_struct}")
"#;

#[test]
fn python_cross_producer_same_service_name_keeps_schemas_separate() {
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
        PeppygenLanguage::Python,
        &user_node_dir,
        interfaces,
        "test-hash",
        &test_peppy_dirs(),
        Default::default(),
        None,
    )
    .expect("failed to generate peppygen lib");

    let peppygen_dir = user_node_dir.join(PEPPYGEN_OUTPUT_PATH);
    let front_module = peppygen_dir.join("peppygen/consumed_services/front_cam_enable.py");
    let rear_module = peppygen_dir.join("peppygen/consumed_services/rear_cam_enable.py");
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

    // File-level evidence that the schemas weren't collapsed.
    let capnp_dir = peppygen_dir.join("peppygen/capnp");
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

    init_python_user_node(&user_node_dir);
    init_python_project_venv(&user_node_dir);

    let output = Command::new("uv")
        .args(["run", "python", "-c", PYTHON_PROBE])
        .current_dir(&user_node_dir)
        .stdin(Stdio::null())
        .output()
        .expect("failed to invoke uv run python");

    assert!(
        output.status.success(),
        "Python probe failed for cross-producer same-service-name scenario.\n\
         stdout:\n{}\n\
         stderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
