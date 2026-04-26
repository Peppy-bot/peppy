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

use common::{
    CALLER_INSTANCE_ID, assert_clock_round_trip, assert_clock_topic_emits_monotonic_ticks,
    start_core_node_with_real_messenger,
};
use std::time::Duration;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn clock_service_round_trip_over_real_zenoh() {
    let started = start_core_node_with_real_messenger().await;
    assert_clock_round_trip(&started).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn clock_topic_publishes_ticks_over_real_zenoh() {
    let started = start_core_node_with_real_messenger().await;
    // Real caller identity here, not "*" — Zenoh's matcher is bidirectional,
    // so the publisher's wildcard literals intersect correctly with these
    // specific values. This is exactly the production code path; if it
    // diverges from the mock test we have a real bug.
    assert_clock_topic_emits_monotonic_ticks(
        &started,
        &started.core_node_name,
        CALLER_INSTANCE_ID,
        Duration::from_secs(3),
    )
    .await;
}
