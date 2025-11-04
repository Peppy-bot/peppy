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
        rendered.contains("pub async fn handle_enable_camera_next_request<F>("),
        &rendered,
        "expected async service handler with handle_*_next_request naming"
    );
    assert_rendered!(
        rendered.contains("handler: F"),
        &rendered,
        "expected handler callback parameter"
    );
    assert_rendered!(
        rendered.contains("F: Fn(bool) -> crate::Result<EnableCameraResponse>"),
        &rendered,
        "expected Fn trait bound for handler"
    );
    assert_rendered!(
        rendered.contains("-> crate::Result<()>"),
        &rendered,
        "expected async function to return unit result"
    );
    assert_rendered!(
        rendered.contains("messenger.handle().listen(namespace, service_name).await?"),
        &rendered,
        "expected service listen call"
    );
    assert_rendered!(
        rendered.contains("service.next_request().await?"),
        &rendered,
        "expected next_request call to receive incoming request"
    );
    assert_rendered!(
        rendered.contains("enable_camera_deserialize_request"),
        &rendered,
        "expected request deserializer helper function"
    );
    assert_rendered!(
        rendered.contains("let response_payload = bytes::Bytes::new();"),
        &rendered,
        "expected inline response serialization"
    );
    assert_rendered!(
        rendered.contains("handler(enable)?"),
        &rendered,
        "expected handler callback invocation with deserialized parameter"
    );
    assert_rendered!(
        rendered.contains("service.send_response(request.id, response_payload).await?"),
        &rendered,
        "expected send_response call"
    );
    assert_rendered!(
        rendered.contains("capnp::serialize::read_message"),
        &rendered,
        "expected request deserialization"
    );
    assert_rendered!(
        rendered.contains("crate::capnp::enable_camera_message_capnp::enable_camera_message::Reader"),
        &rendered,
        "expected service request schema reader"
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
