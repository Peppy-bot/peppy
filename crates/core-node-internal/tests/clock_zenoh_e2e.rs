//! End-to-end clock tests against a real `zenohd` router.
//!
//! The other `listen_for_clock` / `publish_clock` tests run against
//! [`MockAdapter`], which has unidirectional wildcard matching and
//! best-effort QoS that's just a label. Zenoh has full bidirectional
//! wildcard matching, real QoS-driven drop semantics, real discovery,
//! and cross-process serialization. Mock-only coverage was leaving a
//! gap on exactly the wire shape that publishers use (`*` literals in
//! caller-identity slots).
//!
//! These two tests close that gap with one ephemeral router each.

mod common;

use common::{CALLER_INSTANCE_ID, start_core_node_with_real_messenger};
use config::node::QoSProfile;
use core_node::names;
use core_node_api::encoding::{ClockRequest, ClockResponse, ClockSource, ClockTick};
use peppylib::ServiceMessenger;
use peppylib::messaging::TopicMessenger;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

fn now_ns() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn clock_service_round_trip_over_real_zenoh() {
    let started = start_core_node_with_real_messenger().await;

    let t0 = now_ns();
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
    .expect("clock service poll should succeed over zenoh");

    let t3 = now_ns();
    let clock_response = ClockResponse::decode(&response.payload()).expect("decode should succeed");

    assert_eq!(
        clock_response.client_send_time, t0,
        "server should echo client_send_time unchanged",
    );
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
    assert_eq!(clock_response.clock_source, ClockSource::Wall);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn clock_topic_publishes_ticks_over_real_zenoh() {
    let started = start_core_node_with_real_messenger().await;

    // Real caller identity here, not "*" — Zenoh's matcher is bidirectional,
    // so the publisher's wildcard literals intersect correctly with these
    // specific values. This is exactly the production code path; if it
    // diverges from the mock test it means we have a real bug.
    let mut subscription = TopicMessenger::subscribe(
        &started.caller_handle,
        &started.core_node_name,
        CALLER_INSTANCE_ID,
        &started.core_node_name,
        names::CLOCK,
        Some(&started.core_node_name),
        None,
        QoSProfile::SensorData,
    )
    .await
    .expect("clock topic subscription should succeed");

    let mut times = Vec::with_capacity(3);
    for _ in 0..3 {
        let message = tokio::time::timeout(Duration::from_secs(3), subscription.on_next_message())
            .await
            .expect("clock tick should arrive within 3 s on real zenoh")
            .expect("subscription should not close");

        let tick = ClockTick::decode(message.payload().as_ref())
            .expect("clock tick decode should succeed");
        assert_eq!(tick.clock_source, ClockSource::Wall);
        times.push(tick.time);
    }

    assert!(
        times.windows(2).all(|w| w[0] < w[1]),
        "clock ticks over zenoh should be strictly monotonic, got {times:?}",
    );
}
