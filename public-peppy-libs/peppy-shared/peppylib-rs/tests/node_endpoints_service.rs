mod common;

use common::{
    CALLER_INSTANCE_ID, TEST_CORE_NODE_NAME, TEST_INSTANCE_ID, TEST_NODE_NAME, get_client_server,
    test_node_target,
};
use peppylib::{
    encoding::endpoints::{NodeEndpointsRequest, NodeEndpointsResponse},
    messaging::{MessengerHandle, ProducerRef, ServiceMessenger, ServiceTarget},
    runtime::{AnnouncedEndpoint, EndpointBinding},
    services::endpoints::listen_for_node_endpoints,
};
use std::sync::Arc;
use std::time::Duration;

fn announced(label: &str, scheme: &str, address: &str, path: &str) -> AnnouncedEndpoint {
    AnnouncedEndpoint {
        label: label.to_string(),
        binding: EndpointBinding {
            scheme: scheme.to_string(),
            address: address.parse().expect("a socket address"),
            path: path.to_string(),
        },
    }
}

/// The service answers a request with the sealed set it was started with,
/// in label order, mirroring `node_health_service.rs`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn node_endpoints_request_response_roundtrip() {
    let (client, shared_messenger) = get_client_server().await;
    let server_handle = MessengerHandle::from_shared(Arc::clone(&shared_messenger));

    let sealed = vec![
        announced("panel", "http", "0.0.0.0:8765", ""),
        announced("viewer", "https", "[::]:8080", "/"),
    ];
    let _endpoints_task = listen_for_node_endpoints(
        &server_handle,
        TEST_CORE_NODE_NAME,
        TEST_INSTANCE_ID,
        test_node_target(TEST_NODE_NAME),
        sealed.clone(),
    )
    .await
    .expect("failed to start endpoints service");

    // Allow the service to fully establish its listeners
    tokio::time::sleep(Duration::from_millis(50)).await;

    let request_payload = NodeEndpointsRequest::new()
        .encode()
        .expect("failed to encode endpoints request");
    let response = ServiceMessenger::poll(
        &client.caller_handle,
        &client.core_node_name,
        CALLER_INSTANCE_ID,
        test_node_target(TEST_NODE_NAME),
        peppylib::messaging::NODE_ENDPOINTS_SERVICE,
        ServiceTarget::Producer(&ProducerRef::new(
            client.core_node_name.as_str(),
            client.instance_id.as_str(),
        )),
        request_payload,
        Duration::from_secs(2),
    )
    .await
    .expect("caller should receive response");

    let decoded = NodeEndpointsResponse::decode(&response.payload())
        .expect("should decode endpoints response");
    assert_eq!(decoded.endpoints, sealed);
    assert_eq!(response.instance_id(), client.instance_id);
}
