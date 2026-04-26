mod common;

use common::start_core_node_with_mock_messenger;
use config::node::QoSProfile;
use core_node::names;
use core_node_api::encoding::{ClockSource, ClockTick};
use peppylib::messaging::TopicMessenger;
use std::time::Duration;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn publish_clock_emits_monotonic_ticks() {
    let started = start_core_node_with_mock_messenger().await;

    // Topic subscribers must use "*" for their own core_node / instance_id —
    // emit_topic_message hard-codes "*" into those slots of the wire key, and
    // the mock matcher is unidirectional (subscriber pattern matches against
    // publisher topic as a literal). Same trick the action-feedback tests use.
    let mut subscription = TopicMessenger::subscribe(
        &started.caller_handle,
        "*",
        "*",
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
        let message = tokio::time::timeout(Duration::from_secs(2), subscription.on_next_message())
            .await
            .expect("clock tick should arrive within 2 s")
            .expect("subscription should not close");

        let tick = ClockTick::decode(message.payload().as_ref())
            .expect("clock tick decode should succeed");
        assert_eq!(tick.clock_source, ClockSource::Wall);
        times.push(tick.time);
    }

    // Strictly monotonic across consecutive ticks. A non-strict assertion
    // would silently accept a publisher that re-emits the same payload.
    assert!(
        times.windows(2).all(|w| w[0] < w[1]),
        "clock ticks should be strictly monotonic, got {times:?}",
    );
}
