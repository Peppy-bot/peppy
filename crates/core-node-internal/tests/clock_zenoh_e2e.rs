//! End-to-end clock tests against a real `zenohd` router.
//!
//! The other `listen_for_clock` / `publish_clock` tests run against
//! [`MockAdapter`], whose best-effort QoS is just a label. Zenoh has real
//! QoS-driven drop semantics, real discovery, and cross-process
//! serialization that the mock can't reproduce.
//!
//! These two tests close that gap with one ephemeral router each.

mod common;

use common::{
    CALLER_INSTANCE_ID, assert_clock_round_trip, assert_clock_topic_emits_monotonic_ticks,
    start_core_node_with_real_messenger_profile,
};
use config::peppy_config::TransportProfile;
use std::time::Duration;

#[rstest::rstest]
#[case::peer_shm(TransportProfile::PEER_SHM)]
#[case::router_shm(TransportProfile::ROUTER_SHM)]
#[case::peer_no_shm(TransportProfile::PEER_NO_SHM)]
#[case::router_no_shm(TransportProfile::ROUTER_NO_SHM)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn clock_service_round_trip_over_real_zenoh(#[case] profile: TransportProfile) {
    let started = start_core_node_with_real_messenger_profile(profile).await;
    assert_clock_round_trip(&started).await;
}

#[rstest::rstest]
#[case::peer_shm(TransportProfile::PEER_SHM)]
#[case::router_shm(TransportProfile::ROUTER_SHM)]
#[case::peer_no_shm(TransportProfile::PEER_NO_SHM)]
#[case::router_no_shm(TransportProfile::ROUTER_NO_SHM)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn clock_topic_publishes_ticks_over_real_zenoh(#[case] profile: TransportProfile) {
    let started = start_core_node_with_real_messenger_profile(profile).await;
    assert_clock_topic_emits_monotonic_ticks(
        &started,
        &started.core_node_name,
        CALLER_INSTANCE_ID,
        Duration::from_secs(3),
    )
    .await;
}
