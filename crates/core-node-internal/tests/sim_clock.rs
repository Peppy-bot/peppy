//! Sim-clock integration tests.
//!
//! In sim mode the daemon stops publishing wall ticks and instead subscribes
//! to the `clock` topic, caching the latest external tick. The clock service
//! reads from that cache, so requests before the first observed tick must
//! surface a "not ready" error rather than silently returning zero or
//! falling back to wall time.

mod common;

use common::{CALLER_INSTANCE_ID, start_core_node_with_sim_clock};
use config::node::QoSProfile;
use core_node_api::encoding::{ClockRequest, ClockResponse, ClockTick};
use core_node_api::{ServiceId, TopicId};
use peppylib::messaging::ServiceTarget;
use peppylib::{ServiceMessenger, TopicMessenger};
use std::time::{Duration, Instant};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sim_clock_service_returns_not_ready_until_first_tick() {
    let started = start_core_node_with_sim_clock().await;

    let request_payload = ClockRequest::new(0).encode().expect("encode succeeds");
    let response = ServiceMessenger::poll(
        &started.caller_handle,
        &started.core_node_name,
        CALLER_INSTANCE_ID,
        common::core_node_target(&started.core_node_name),
        ServiceId::Clock.name(),
        ServiceTarget::Any,
        request_payload,
        Duration::from_secs(2),
    )
    .await;

    let err = response.expect_err("empty cache must reject the synchronize request");
    let message = err.to_string();
    assert!(
        message.contains("clock not ready"),
        "expected 'clock not ready' surface; got: {message}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sim_clock_service_serves_external_tick_after_publish() {
    let started = start_core_node_with_sim_clock().await;

    let publisher = TopicMessenger::declare_publisher(
        &started.caller_handle,
        &started.core_node_name,
        CALLER_INSTANCE_ID,
        common::core_node_target(&started.core_node_name),
        None,
        TopicId::Clock.name(),
        QoSProfile::SensorData,
    )
    .await
    .expect("external publisher declares against the clock topic");

    // Drive a deterministic value so the response can be checked exactly.
    const SIM_NS: u64 = 42_000_000_000;
    publisher
        .publish(ClockTick::new(SIM_NS).encode().expect("encode tick"))
        .await
        .expect("publish external tick");

    // The daemon's subscriber needs a beat to update its cache before the
    // service handler picks up the new value. Poll until success, but bound
    // the loop so a stuck cache fails the test with diagnostics instead of
    // hanging the test runner. Each attempt's timeout is capped by the
    // remaining budget so a stuck poll cannot push wall time past the
    // deadline.
    let deadline = Instant::now() + Duration::from_secs(5);
    let response = loop {
        let attempt_timeout = deadline
            .saturating_duration_since(Instant::now())
            .min(Duration::from_secs(2));
        let request_payload = ClockRequest::new(0).encode().expect("encode request");
        let attempt = ServiceMessenger::poll(
            &started.caller_handle,
            &started.core_node_name,
            CALLER_INSTANCE_ID,
            common::core_node_target(&started.core_node_name),
            ServiceId::Clock.name(),
            ServiceTarget::Any,
            request_payload,
            attempt_timeout,
        )
        .await;
        match attempt {
            Ok(resp) => break resp,
            Err(e) => {
                if Instant::now() >= deadline {
                    panic!(
                        "sim clock service did not return a cached tick within 5s; last error: {e:?}"
                    );
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        }
    };

    let decoded = ClockResponse::decode(&response.payload()).expect("decode response");
    assert_eq!(
        decoded.server_recv_time, SIM_NS,
        "sim mode must answer from the cached external tick"
    );
    assert_eq!(
        decoded.server_send_time, SIM_NS,
        "both stamps come from the same cached value within a single request"
    );
}
