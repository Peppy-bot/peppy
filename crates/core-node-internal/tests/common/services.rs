#![allow(dead_code)] // Each test binary uses only a subset of these shared helpers.

use super::daemon::StartedCoreNode;
use super::{CALLER_INSTANCE_ID, core_node_target};
use config::node::QoSProfile;
use core_node_api::encoding::{
    ClockRequest, ClockTick, DatastoreGetRequest, DatastoreGetResponse, DatastoreListRequest,
    DatastoreListResponse, DatastoreRemoveRequest, DatastoreStoreExclusiveRequest,
    DatastoreStoreExclusiveResponse, DatastoreStoreRequest, NodeInfo, NodeInfoRequest,
    NodeInfoResponse,
};
use core_node_api::{ServiceRequest, TopicId};
use peppylib::clock::wall_now_ns;
use peppylib::messaging::{Failure, TopicMessenger};
use std::time::Duration;

/// Sends a `NODE_INFO` poll request and unwraps the `Found` body; a node the
/// daemon answers `NotInStack` for is a test's setup gone wrong.
pub async fn poll_node_info(
    started: &StartedCoreNode,
    request: &NodeInfoRequest,
    timeout: Duration,
) -> NodeInfo {
    match poll_core_node_with_timeout(started, request, timeout).await {
        NodeInfoResponse::Found(info) => *info,
        NodeInfoResponse::NotInStack => panic!(
            "node_info unexpectedly reported `{}:{}` as not in the stack",
            request.node_name, request.node_tag
        ),
    }
}

/// [`poll_core_node_with_timeout`] with the 5-second timeout that a request
/// the daemon answers without long work needs.
pub async fn poll_core_node<R: ServiceRequest>(
    started: &StartedCoreNode,
    request: &R,
) -> R::Response {
    poll_core_node_with_timeout(started, request, Duration::from_secs(5)).await
}

/// Polls the core-node service that `request` belongs to on the started core
/// node, with the shared test caller identity, and returns the decoded
/// response. Panics on any failure, the daemon's refusal of the request
/// included, and when no response arrives within `timeout`.
pub async fn poll_core_node_with_timeout<R: ServiceRequest>(
    started: &StartedCoreNode,
    request: &R,
    timeout: Duration,
) -> R::Response {
    poll_core_node_as(started, CALLER_INSTANCE_ID, request, timeout)
        .await
        .unwrap_or_else(|e| panic!("{} poll should succeed: {e}", R::ID.name()))
}

/// Polls the core-node service that `request` belongs to on the started core
/// node as the instance `caller_instance_id`, returning the daemon's answer
/// or its refusal of the request.
pub async fn poll_core_node_as<R: ServiceRequest>(
    started: &StartedCoreNode,
    caller_instance_id: &str,
    request: &R,
    timeout: Duration,
) -> peppylib::PeppyResult<R::Response> {
    peppylib::core_node::transport::poll(
        request,
        &started.caller_handle,
        &started.core_node_name,
        caller_instance_id,
        &started.core_node_name,
        timeout,
    )
    .await
}

/// The failure of the error reply that `error` carries. Panics when the
/// request failed without an error reply.
#[track_caller]
pub fn error_reply(error: &peppylib::PeppyError) -> Failure {
    error
        .failure()
        .cloned()
        .unwrap_or_else(|| panic!("the request failed without an error reply: {error}"))
}

/// Polls the core-node service that `request` belongs to and returns the
/// failure of the daemon's error reply. Panics when the daemon answers the
/// request, and when the poll fails without an error reply.
pub async fn poll_core_node_failure<R: ServiceRequest>(
    started: &StartedCoreNode,
    request: &R,
) -> Failure
where
    R::Response: std::fmt::Debug,
{
    let error = poll_core_node_as(started, CALLER_INSTANCE_ID, request, Duration::from_secs(5))
        .await
        .expect_err("the daemon gives an error reply");
    error.failure().cloned().unwrap_or_else(|| {
        panic!(
            "{} poll failed without an error reply: {error}",
            R::ID.name()
        )
    })
}

/// Sends a `datastore_store_exclusive` request for `key` as the instance
/// `writer`, returning the daemon's answer or its refusal of the request.
pub async fn datastore_store_exclusive_as(
    started: &StartedCoreNode,
    writer: &str,
    key: &str,
    value: &[u8],
) -> peppylib::PeppyResult<DatastoreStoreExclusiveResponse> {
    let request = DatastoreStoreExclusiveRequest::new(key, value.to_vec(), "text/plain")
        .expect("test key should be a valid datastore key");
    poll_core_node_as(started, writer, &request, Duration::from_secs(5)).await
}

/// Sends a `datastore_store` request to the started core node. Panics on any
/// failure, the refusal of a key another instance owns included.
pub async fn datastore_store(started: &StartedCoreNode, key: &str, value: &[u8], encoding: &str) {
    let request = DatastoreStoreRequest::new(key, value.to_vec(), encoding)
        .expect("test key should be a valid datastore key");
    poll_core_node(started, &request).await;
}

/// Sends a `datastore_get` request to the started core node and returns the
/// decoded response. Panics on any transport or decode failure.
pub async fn datastore_get(started: &StartedCoreNode, key: &str) -> DatastoreGetResponse {
    let request = DatastoreGetRequest::new(key).expect("test key should be a valid datastore key");
    poll_core_node(started, &request).await
}

/// Sends a `datastore_list` request to the started core node and returns the
/// decoded response. Panics on any transport or decode failure.
pub async fn datastore_list(started: &StartedCoreNode) -> DatastoreListResponse {
    poll_core_node(started, &DatastoreListRequest::new()).await
}

/// Sends a `datastore_remove` request to the started core node and returns
/// whether the key existed. Panics on any failure, the refusal of a key
/// another instance owns included.
pub async fn datastore_remove(started: &StartedCoreNode, key: &str) -> bool {
    let request =
        DatastoreRemoveRequest::new(key).expect("test key should be a valid datastore key");
    poll_core_node(started, &request).await.removed
}

/// Stores an arbitrary binary value, reads it back, and asserts the value and
/// encoding survive the round trip. The real-zenoh datastore tests use it to
/// exercise real cross-process serialization of the Cap'n Proto `Data` field.
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
    let clock_response = poll_core_node(started, &ClockRequest::new(t0)).await;
    let t3 = wall_now_ns().expect("system clock should be available");

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
    tick_timeout: Duration,
) {
    let mut subscription = TopicMessenger::subscribe_target_scoped(
        &started.caller_handle,
        &started.core_node_name,
        CALLER_INSTANCE_ID,
        core_node_target(&started.core_node_name),
        TopicId::Clock.name(),
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
        times.push(tick.time());
    }

    // Strict (not non-strict) so a publisher that re-emits the same payload
    // doesn't silently pass.
    assert!(
        times.windows(2).all(|w| w[0] < w[1]),
        "clock ticks should be strictly monotonic, got {times:?}",
    );
}
