//! End-to-end datastore tests against a real `zenohd` router.
//!
//! The `listen_for_datastore` tests run against [`MockAdapter`], which does
//! not reproduce Zenoh's real cross-process serialization. This closes that
//! gap: a binary store/get round trip over one ephemeral router, proving the
//! Cap'n Proto `Data` field survives the wire intact.

mod common;

use common::{assert_datastore_binary_round_trip, start_core_node_with_real_messenger_topology};
use daemon_config::peppy_config::Topology;

#[rstest::rstest]
#[case::peer(Topology::Peer)]
#[case::router(Topology::Router)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn datastore_round_trip_over_real_zenoh(#[case] topology: Topology) {
    let started = start_core_node_with_real_messenger_topology(topology).await;
    assert_datastore_binary_round_trip(&started).await;
}
