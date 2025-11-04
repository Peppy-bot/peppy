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
fn exposed_service() {
    let service: ExposedService = serde_json5::from_str(EXPOSED_SERVICE_EXAMPLE).unwrap();

    let mut generator = RustGenerator::new();
    generator.add_exposed_service(&service).unwrap();
    let artifacts: Vec<String> = generator
        .into_artifacts()
        .into_iter()
        .map(|artifact| artifact.code_output)
        .collect();
    assert_eq!(
        artifacts.len(),
        1,
        "expected a single generated artifact, got {}",
        artifacts.len()
    );
    let rendered = artifacts.into_iter().next().expect("artifact is present");

    assert_rendered!(
        rendered.contains("struct Endpoints;"),
        &rendered,
        "expected endpoints struct declaration"
    );
    assert_rendered!(
        rendered.contains("pub struct EnableCameraResponse"),
        &rendered,
        "expected response struct for exposed service"
    );
    assert_rendered!(
        rendered.contains("enabled: bool"),
        &rendered,
        "expected bool field in response struct"
    );
    assert_rendered!(
        rendered.contains("error_msg: String"),
        &rendered,
        "expected string field in response struct"
    );
    assert_rendered!(
        rendered.contains("pub async fn enable_camera_next_request("),
        &rendered,
        "expected async service function with _next_request suffix"
    );
    assert_rendered!(
        !rendered.contains("pub fn enable_camera("),
        &rendered,
        "expected removal of sync service wrapper"
    );
    assert_rendered!(
        rendered.contains("-> crate::Result<EnableCameraResponse>"),
        &rendered,
        "expected async function to return response struct"
    );
    assert_rendered!(
        rendered.contains("capnp::message::Builder::new_default"),
        &rendered,
        "expected capnp builder usage"
    );
    assert_rendered!(
        rendered
            .contains("crate::capnp::enable_camera_message_capnp::enable_camera_message::Builder"),
        &rendered,
        "expected service-specific schema builder"
    );
    assert_rendered!(
        rendered.contains("messenger.handle().listen(namespace, service_name).await?"),
        &rendered,
        "expected service listen call"
    );
    assert_rendered!(
        rendered.contains("service.handle_next_request(payload).await?"),
        &rendered,
        "expected handle_next_request call"
    );
    assert_rendered!(
        rendered.contains("messenger.namespace()"),
        &rendered,
        "expected namespace usage from messenger"
    );
    assert_rendered!(
        rendered.contains("capnp::serialize::write_message"),
        &rendered,
        "expected serialization call"
    );
    assert_rendered!(
        rendered.contains("crate::Error::CapnpSerialize"),
        &rendered,
        "expected explicit capnp serialization error variant"
    );
    assert_rendered!(
        rendered.contains("enable_camera_response_from_payload"),
        &rendered,
        "expected response parsing helper"
    );
    assert_rendered!(
        rendered.contains("capnp::serialize::read_message"),
        &rendered,
        "expected response deserialization call"
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
    assert_eq!(
        artifacts.len(),
        1,
        "expected a single generated artifact, got {}",
        artifacts.len()
    );
    let rendered = artifacts.into_iter().next().expect("artifact is present");

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
