use super::*;
use config::node::{ExposedService, SubscribedService};

const EXPOSED_SERVICE_EXAMPLE: &str = r#"
{
  name: "enable_camera",
  accept_message_format: {
    enable: "bool"
  },
  return_message_format: {
    enabled: "bool",
    error_msg: "string"
  }
}
"#;

#[test]
fn exposed_service_gen_calling_code() {
    let service: ExposedService = serde_json5::from_str(EXPOSED_SERVICE_EXAMPLE).unwrap();

    let mut generator = RustGenerator::new();
    generator.add_exposed_service(&service).unwrap();
    let artifacts: Vec<String> = generator
        .into_artifacts()
        .into_iter()
        .map(|artifact| artifact.code_output)
        .collect();
    let rendered = single_artifact(artifacts);

    println!("generated service code:\n{rendered}");

    assert_rendered!(
        rendered.contains("pub fn enable_camera("),
        &rendered,
        "expected sync service function"
    );
    assert_rendered!(
        rendered.contains("-> crate::Result<()>"),
        &rendered,
        "expected crate result type for service function"
    );
    assert_rendered!(
        rendered.contains("pub async fn enable_camera_async("),
        &rendered,
        "expected async service function"
    );
    assert_rendered!(
        rendered.contains("capnp::message::Builder::new_default"),
        &rendered,
        "expected capnp builder usage"
    );
    assert_rendered!(
        rendered.contains("enable_camera_message_capnp::enable_camera_message::Builder"),
        &rendered,
        "expected service-specific schema builder"
    );
    assert_rendered!(
        rendered.contains("capnp::serialize::write_message"),
        &rendered,
        "expected serialization call"
    );
    assert_rendered!(
        rendered.contains("crate::Error::RuntimeInitialization"),
        &rendered,
        "expected explicit runtime initialization error variant"
    );
    assert_rendered!(
        rendered.contains("crate::Error::CapnpSerialize"),
        &rendered,
        "expected explicit capnp serialization error variant"
    );
    assert_rendered!(
        rendered.contains("enable: bool"),
        &rendered,
        "expected bool argument"
    );
}

#[test]
fn subscribed_service_returns_arguments() {
    let service = r#"
        {
            node: "uvc_camera",
            name: "get_camera_info",
            tag: "0.1.0"
        }
        "#;
    let service: SubscribedService = serde_json5::from_str(service).unwrap();
    let format = r#"
        {
            card_type: "string",
            size: "string",
            interval: "string"
        }
        "#;
    let format: MessageFormat = serde_json5::from_str(format).unwrap();

    let mut generator = RustGenerator::new();
    generator
        .add_subscribed_service(&service, Some(&format))
        .unwrap();
    let artifacts: Vec<String> = generator
        .into_artifacts()
        .into_iter()
        .map(|artifact| artifact.code_output)
        .collect();
    let rendered = single_artifact(artifacts);

    println!("generated subscribed service code:\n{rendered}");

    assert_rendered!(
        rendered.contains("pub async fn on_get_camera_info() -> OnGetCameraInfoArguments"),
        &rendered,
        "expected async service subscriber"
    );
    assert_rendered!(
        rendered.contains("pub struct OnGetCameraInfoArguments"),
        &rendered,
        "expected return struct"
    );
    assert_rendered!(
        rendered.contains("card_type: String"),
        &rendered,
        "expected field mapping"
    );
}
