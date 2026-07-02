#![allow(dead_code)] // Each test binary uses only a subset of these shared helpers.

use super::daemon::StartedCoreNode;
use super::{CALLER_INSTANCE_ID, core_node_target};
use config::node::QoSProfile;
use core_node::names;
use core_node_api::encoding::{
    ClockRequest, ClockResponse, ClockTick, DatastoreGetRequest, DatastoreGetResponse,
    DatastoreListRequest, DatastoreListResponse, DatastoreRemoveRequest, DatastoreRemoveResponse,
    DatastoreStoreRequest, DatastoreStoreResponse,
};
use peppylib::clock::wall_now_ns;
use peppylib::messaging::{ServiceTarget, TopicMessenger};
use peppylib::{Message, Payload, ServiceMessenger};
use std::time::Duration;

/// Polls a datastore service on the started core node using the shared test
/// routing and 5-second timeout, returning the response message. Panics on any
/// transport failure — the datastore endpoints should always answer a
/// well-formed request.
async fn poll_datastore(started: &StartedCoreNode, service: &str, payload: Payload) -> Message {
    ServiceMessenger::poll(
        &started.caller_handle,
        &started.core_node_name,
        CALLER_INSTANCE_ID,
        core_node_target(&started.core_node_name),
        service,
        ServiceTarget::Any, // discover the daemon's random per-boot service instance
        payload,
        Duration::from_secs(5),
    )
    .await
    .unwrap_or_else(|e| panic!("datastore {service} poll should succeed: {e}"))
}

/// Sends a `datastore_store` request to the started core node and decodes the
/// (empty) acknowledgement. Panics on any transport or decode failure — the
/// store endpoint should always succeed for a well-formed request.
pub async fn datastore_store(started: &StartedCoreNode, key: &str, value: &[u8], encoding: &str) {
    let payload = DatastoreStoreRequest::new(key, value.to_vec(), encoding)
        .expect("test key should be a valid datastore key")
        .encode()
        .expect("encode store request should succeed");
    let response = poll_datastore(started, names::DATASTORE_STORE, payload).await;
    DatastoreStoreResponse::decode(&response.payload()).expect("decode store response");
}

/// Sends a `datastore_get` request to the started core node and returns the
/// decoded response. Panics on any transport or decode failure.
pub async fn datastore_get(started: &StartedCoreNode, key: &str) -> DatastoreGetResponse {
    let payload = DatastoreGetRequest::new(key)
        .expect("test key should be a valid datastore key")
        .encode()
        .expect("encode get request should succeed");
    let response = poll_datastore(started, names::DATASTORE_GET, payload).await;
    DatastoreGetResponse::decode(&response.payload()).expect("decode get response")
}

/// Sends a `datastore_list` request to the started core node and returns the
/// decoded response. Panics on any transport or decode failure.
pub async fn datastore_list(started: &StartedCoreNode) -> DatastoreListResponse {
    let payload = DatastoreListRequest::new()
        .encode()
        .expect("encode list request should succeed");
    let response = poll_datastore(started, names::DATASTORE_LIST, payload).await;
    DatastoreListResponse::decode(&response.payload()).expect("decode list response")
}

/// Sends a `datastore_remove` request to the started core node and returns
/// whether the key existed. Panics on any transport or decode failure.
pub async fn datastore_remove(started: &StartedCoreNode, key: &str) -> bool {
    let payload = DatastoreRemoveRequest::new(key)
        .expect("test key should be a valid datastore key")
        .encode()
        .expect("encode remove request should succeed");
    let response = poll_datastore(started, names::DATASTORE_REMOVE, payload).await;
    DatastoreRemoveResponse::decode(&response.payload())
        .expect("decode remove response")
        .removed
}

/// Stores an arbitrary binary value, reads it back, and asserts the value and
/// encoding survive the round trip. Shared between the mock-messenger and
/// real-zenoh datastore tests — the latter exercises real cross-process
/// serialization of the Cap'n Proto `Data` field.
pub async fn assert_datastore_binary_round_trip(started: &StartedCoreNode) {
    let key = "binary_key_1";
    let value = vec![0u8, 255, 0x80, 0xFE, 0x00, 0x42];
    let encoding = "application/octet-stream";

    datastore_store(started, key, &value, encoding).await;
    let response = datastore_get(started, key).await;

    assert!(response.found, "stored key should be found");
    assert_eq!(response.value, value, "value should survive round trip");
    assert_eq!(
        response.encoding, encoding,
        "encoding should survive round trip"
    );
    assert_eq!(
        response.last_modified_by, CALLER_INSTANCE_ID,
        "get should report the writer's instance_id"
    );
}

/// Drives the NTP-style 4-timestamp exchange against the started core node and
/// asserts the wire contract: server echoes `t0` unchanged, and the causal
/// chain `t0 ≤ t1 ≤ t2 ≤ t3` holds. Shared between the mock-messenger and
/// real-zenoh round-trip tests.
pub async fn assert_clock_round_trip(started: &StartedCoreNode) {
    let t0 = wall_now_ns().expect("system clock should be available");
    let request_payload = ClockRequest::new(t0)
        .encode()
        .expect("encode should succeed");

    let response = ServiceMessenger::poll(
        &started.caller_handle,
        &started.core_node_name,
        CALLER_INSTANCE_ID,
        core_node_target(&started.core_node_name),
        names::CLOCK,
        ServiceTarget::Any, // discover the daemon's random per-boot service instance
        request_payload,
        Duration::from_secs(5),
    )
    .await
    .expect("clock service poll should succeed");

    let t3 = wall_now_ns().expect("system clock should be available");
    let clock_response = ClockResponse::decode(&response.payload()).expect("decode should succeed");

    assert_eq!(
        clock_response.client_send_time, t0,
        "server should echo client_send_time unchanged"
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

/// Subscribes to the `clock` topic, collects three consecutive `ClockTick`s,
/// and asserts they are strictly monotonic. Shared between the mock-messenger
/// and real-zenoh publish tests.
pub async fn assert_clock_topic_emits_monotonic_ticks(
    started: &StartedCoreNode,
    caller_core_node: &str,
    caller_instance_id: &str,
    tick_timeout: Duration,
) {
    let mut subscription = TopicMessenger::subscribe(
        &started.caller_handle,
        caller_core_node,
        caller_instance_id,
        Some(core_node_target(&started.core_node_name)),
        false,
        names::CLOCK,
        &peppylib::messaging::ConsumerFilter::Any,
        QoSProfile::SensorData,
    )
    .await
    .expect("clock topic subscription should succeed");

    let mut times = Vec::with_capacity(3);
    for _ in 0..3 {
        let message = tokio::time::timeout(tick_timeout, subscription.on_next_message())
            .await
            .unwrap_or_else(|_| panic!("clock tick should arrive within {tick_timeout:?}"))
            .expect("subscription should not close");

        let tick = ClockTick::decode(message.payload().as_ref())
            .expect("clock tick decode should succeed");
        times.push(tick.time);
    }

    // Strict (not non-strict) so a publisher that re-emits the same payload
    // doesn't silently pass.
    assert!(
        times.windows(2).all(|w| w[0] < w[1]),
        "clock ticks should be strictly monotonic, got {times:?}",
    );
}
