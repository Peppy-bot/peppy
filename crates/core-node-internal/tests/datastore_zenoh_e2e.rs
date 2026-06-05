//! End-to-end datastore tests against a real `zenohd` router.
//!
//! The `listen_for_datastore` tests run against [`MockAdapter`], which does
//! not reproduce Zenoh's real cross-process serialization. This closes that
//! gap: a binary store/get round trip over one ephemeral router, proving the
//! Cap'n Proto `Data` field survives the wire intact.

mod common;

use common::{assert_datastore_binary_round_trip, start_core_node_with_real_messenger};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn datastore_round_trip_over_real_zenoh() {
    let started = start_core_node_with_real_messenger().await;
    assert_datastore_binary_round_trip(&started).await;
}
