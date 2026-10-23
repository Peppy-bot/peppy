//! End-to-end test for the daemon-liveness heartbeat the node watchdog relies
//! on. Starts a real core node and subscribes to the `daemon_heartbeat` topic
//! with the core node's target, asserting the daemon actually publishes beats
//! on the agreed (core-node-keyed) topic, under the per-boot instance id of its
//! presence token. This is the publisher half; the watchdog's logic is
//! unit-tested in `peppylib::services::daemon_watchdog`.

mod common;

use common::{CALLER_INSTANCE_ID, core_node_target, start_core_node_with_mock_messenger};
use config::node::QoSProfile;
use core_node_api::TopicId;
use core_node_api::encoding::ClockTick;
use peppylib::messaging::{CoreNodePresenceMessenger, TopicMessenger};
use std::time::Duration;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn daemon_publishes_liveness_heartbeats() {
    let started = start_core_node_with_mock_messenger().await;

    let mut subscription = TopicMessenger::subscribe_target_scoped(
        &started.caller_handle,
        &started.core_node_name,
        CALLER_INSTANCE_ID,
        core_node_target(&started.core_node_name),
        TopicId::DaemonHeartbeat.name(),
        QoSProfile::SensorData,
    )
    .await
    .expect("daemon_heartbeat subscription should succeed");

    // The daemon's presence token carries its per-boot instance id; the mock
    // answers the read from its live tokens at once.
    let claims = CoreNodePresenceMessenger::list_live(
        &started.caller_handle,
        Some(&started.core_node_name),
        CoreNodePresenceMessenger::LIST_TIMEOUT,
    )
    .await
    .expect("the presence read should succeed");
    let [claim] = claims.as_slice() else {
        panic!("the daemon should hold exactly one presence token, got {claims:?}");
    };

    // The shared test heartbeat interval is 200ms (see default_node_arguments
    // in common/daemon.rs; production uses 5s), so 20s gives ample slack for two
    // beats even on slow CI machines.
    let beat_timeout = Duration::from_secs(20);
    for n in 0..2 {
        let message = tokio::time::timeout(beat_timeout, subscription.on_next_message())
            .await
            .unwrap_or_else(|_| panic!("heartbeat #{n} should arrive within {beat_timeout:?}"))
            .expect("subscription should not close");
        // Payload is a ClockTick used as a cheap carrier; it must decode.
        ClockTick::decode(message.payload().as_ref()).expect("heartbeat payload should decode");
        // A node's watchdog tells its own daemon from a new one by this id.
        assert_eq!(
            message.instance_id(),
            claim.instance_id,
            "heartbeat #{n} should carry the per-boot instance id of the daemon's presence token"
        );
    }
}
