//! End-to-end test for the daemon-liveness heartbeat the node watchdog relies
//! on. Starts a real core node and subscribes to the `daemon_heartbeat` topic
//! exactly the way `peppylib::services::daemon_watchdog` does, asserting the
//! daemon actually publishes beats on the agreed (core-node-keyed) topic. This
//! is the publisher half; the watchdog's timing logic is unit-tested in
//! `peppylib::services::daemon_watchdog`.

mod common;

use common::{CALLER_INSTANCE_ID, core_node_target, start_core_node_with_mock_messenger};
use config::node::QoSProfile;
use core_node::names;
use core_node_api::encoding::ClockTick;
use peppylib::messaging::{ConsumerFilter, TopicMessenger};
use std::time::Duration;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn daemon_publishes_liveness_heartbeats() {
    let started = start_core_node_with_mock_messenger().await;

    let mut subscription = TopicMessenger::subscribe(
        &started.caller_handle,
        &started.core_node_name,
        CALLER_INSTANCE_ID,
        Some(core_node_target(&started.core_node_name)),
        false,
        names::DAEMON_HEARTBEAT,
        Some(&started.core_node_name),
        &ConsumerFilter::Any,
        QoSProfile::SensorData,
    )
    .await
    .expect("daemon_heartbeat subscription should succeed");

    // The shared test heartbeat interval is 200ms (see default_node_arguments
    // in common.rs; production uses 5s), so 20s gives ample slack for two
    // beats even on slow CI machines.
    let beat_timeout = Duration::from_secs(20);
    for n in 0..2 {
        let message = tokio::time::timeout(beat_timeout, subscription.on_next_message())
            .await
            .unwrap_or_else(|_| panic!("heartbeat #{n} should arrive within {beat_timeout:?}"))
            .expect("subscription should not close");
        // Payload is a ClockTick used as a cheap carrier; it must decode.
        ClockTick::decode(message.payload().as_ref()).expect("heartbeat payload should decode");
    }
}
