use std::time::Duration;

use config::node::QoSProfile;
use core_node_api::encoding::{ClockResponse, ClockTick};
use core_node_api::names;
use peppylib::clock;
use peppylib::messaging::{MessengerHandle, ServiceMessenger};
use pmi::ZenohdInstance;
use tempfile::TempDir;

use super::common::{
    CORE_NODE, SERVER_INSTANCE, publish_once, start_router_and_runner, test_node_target,
    wait_for_topic_subscriber, wait_until_reachable,
};

/// Spins up a single-shot `clock` service listener that returns `response`
/// verbatim. The handler decodes the inbound `ClockRequest` to assert wire
/// shape, even though it ignores the value.
async fn spawn_clock_stub_listener(server: MessengerHandle, response: ClockResponse) {
    let mut endpoint = ServiceMessenger::listen(
        &server,
        CORE_NODE,
        SERVER_INSTANCE,
        test_node_target(CORE_NODE),
        names::CLOCK,
    )
    .await
    .expect("listen should succeed");

    tokio::spawn(async move {
        endpoint
            .handle_next_request(|request| async move {
                let payload = request.message().payload();
                let _inbound = core_node_api::encoding::ClockRequest::decode(payload.as_ref())
                    .expect("decode ClockRequest");
                Ok(response.encode().expect("encode ClockResponse"))
            })
            .await
            .expect("handle_next_request should succeed");
    });
}

async fn setup_synchronize_stub(
    response: ClockResponse,
) -> (ZenohdInstance, TempDir, peppylib::runtime::NodeRunner) {
    let (router, temp_dir, node_runner, server) = start_router_and_runner().await;
    spawn_clock_stub_listener(server, response).await;
    wait_until_reachable(node_runner.messenger(), names::CLOCK).await;
    (router, temp_dir, node_runner)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn synchronize_returns_typed_clock_sync() {
    // Canned t1/t2 are far smaller than the live `t0` from SystemTime::now(),
    // so the local clock leads the server and the offset must come out negative.
    let response = ClockResponse::new(0, 2_000_000_000_000, 2_000_000_000_005);

    let (_router, _temp_dir, node_runner) = setup_synchronize_stub(response.clone()).await;

    let sync = clock::synchronize(&node_runner, Some(Duration::from_secs(3)))
        .await
        .expect("synchronize should succeed");

    assert_eq!(sync.raw.server_recv_time, 2_000_000_000_000);
    assert_eq!(sync.raw.server_send_time, 2_000_000_000_005);
    assert!(
        sync.offset_ns < 0,
        "expected local clock to lead canned server time, got offset {} ns",
        sync.offset_ns,
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn subscribe_clock_yields_typed_ticks() {
    let (_router, _temp_dir, node_runner, server) = start_router_and_runner().await;

    // Subscribe via the high-level helper *before* publishing — otherwise the
    // first tick can land before zenoh discovery routes the subscription, and
    // the test races against propagation. With the subscription up first, any
    // tick published after the await point is delivered.
    let mut sub = clock::subscribe(&node_runner)
        .await
        .expect("subscribe_clock should succeed");

    // Deterministically wait until the publisher's session sees the subscription
    // (peer-mode discovery is not instantaneous) instead of guessing a fixed
    // settle delay, so the emit below cannot be dropped before routing.
    wait_for_topic_subscriber(
        &server,
        CORE_NODE,
        SERVER_INSTANCE,
        test_node_target(CORE_NODE),
        names::CLOCK,
    )
    .await;

    let canned = ClockTick::new(1_700_000_000_123_456_789);
    publish_once(
        &server,
        CORE_NODE,
        SERVER_INSTANCE,
        test_node_target(CORE_NODE),
        names::CLOCK,
        QoSProfile::SensorData,
        canned.encode().expect("encode tick"),
    )
    .await
    .expect("emit should succeed");

    let tick = tokio::time::timeout(Duration::from_secs(2), sub.on_next_tick())
        .await
        .expect("tick should arrive within 2 s")
        .expect("on_next_tick should not error")
        .expect("subscription should not have closed");

    assert_eq!(tick, canned);
}
