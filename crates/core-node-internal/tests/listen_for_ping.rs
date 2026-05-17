mod common;

use common::{CALLER_INSTANCE_ID, start_core_node_with_mock_messenger};
use core_node::names;
use core_node_api::encoding::{PingRequest, PingResponse};
use peppylib::ServiceWireSender;
use peppylib::messaging::Iface;
use std::time::Duration;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_ping_roundtrip_succeed() {
    let started = start_core_node_with_mock_messenger().await;

    // Send a ping request to the core node
    let timestamp = 12345u64;
    let ping_request = PingRequest::new(timestamp);
    let request_payload = ping_request.encode().expect("encode should succeed");

    let response = started
        .caller_handle
        .poll_service(
            &ServiceWireSender::new(
                &started.core_node_name,
                CALLER_INSTANCE_ID,
                Some(&started.core_node_name),
                None,
                &started.core_node_name,
                Iface::native(),
                names::PING,
            )
            .expect("valid wire fields"),
            request_payload,
            Duration::from_secs(5),
        )
        .await
        .expect("ping request should succeed");

    let ping_response = PingResponse::decode(&response.payload()).expect("decode should succeed");

    // Verify the response echoes back the timestamp
    assert_eq!(
        ping_response.timestamp, timestamp,
        "timestamp should match the request"
    );
    assert!(
        !ping_response.message.is_empty(),
        "message should not be empty"
    );
}
