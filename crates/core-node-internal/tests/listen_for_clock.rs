mod common;

use common::{CALLER_INSTANCE_ID, start_core_node_with_mock_messenger};
use core_node::names;
use core_node_api::encoding::{ClockRequest, ClockResponse, wall_now_ns};
use peppylib::ServiceMessenger;
use std::time::Duration;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_clock_roundtrip_succeed() {
    let started = start_core_node_with_mock_messenger().await;

    let t0 = wall_now_ns();
    let request_payload = ClockRequest::new(t0)
        .encode()
        .expect("encode should succeed");

    let response = ServiceMessenger::poll(
        &started.caller_handle,
        &started.core_node_name,
        CALLER_INSTANCE_ID,
        &started.core_node_name,
        names::CLOCK,
        Some(&started.core_node_name),
        None,
        request_payload,
        Duration::from_secs(5),
    )
    .await
    .expect("clock request should succeed");

    let t3 = wall_now_ns();
    let clock_response = ClockResponse::decode(&response.payload()).expect("decode should succeed");

    assert_eq!(
        clock_response.client_send_time, t0,
        "server should echo the client_send_time unchanged"
    );
    // Causal chain t0 ≤ t1 ≤ t2 ≤ t3 catches both unit mismatches (ns vs ms)
    // and t1/t2 stamping-order regressions in one assert.
    assert!(
        t0 <= clock_response.server_recv_time
            && clock_response.server_recv_time <= clock_response.server_send_time
            && clock_response.server_send_time <= t3,
        "expected t0 ({}) ≤ t1 ({}) ≤ t2 ({}) ≤ t3 ({})",
        t0,
        clock_response.server_recv_time,
        clock_response.server_send_time,
        t3,
    );
}
