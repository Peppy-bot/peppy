//! Integration tests for the `datastore` client wrappers
//! ([`datastore_store`] / [`datastore_get`]).
//!
//! peppylib can't depend on the core-node daemon crate (that would be a
//! dependency cycle), so the stub here stands in for the daemon: it holds a
//! real in-memory map and serves both the `datastore_store` and
//! `datastore_get` endpoints, letting a genuine store→get round trip flow
//! through the client wrappers over a real ephemeral zenoh router.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use core_node_api::encoding::{
    DatastoreGetRequest, DatastoreGetResponse, DatastoreStoreRequest, DatastoreStoreResponse,
};
use core_node_api::names;
use peppylib::messaging::{MessengerHandle, ServiceMessenger};
use peppylib::runtime::NodeRunner;
use peppylib::{StoredValue, datastore_get, datastore_store};
use pmi::ZenohdInstance;
use tempfile::TempDir;

use super::common::{
    CORE_NODE, SERVER_INSTANCE, start_router_and_runner, test_node_target, wait_until_reachable,
};

/// Shared `key -> (value, encoding)` map behind the stub's two endpoints.
type StubStore = Arc<Mutex<HashMap<String, (Vec<u8>, String)>>>;

/// Spins up a stateful datastore stub: a `datastore_store` endpoint that
/// upserts into a shared map and a `datastore_get` endpoint that reads from
/// it. Both run for the lifetime of the test (aborted at teardown).
async fn spawn_datastore_stub(server: MessengerHandle) {
    let store: StubStore = Arc::new(Mutex::new(HashMap::new()));

    let mut store_endpoint = ServiceMessenger::listen(
        &server,
        CORE_NODE,
        SERVER_INSTANCE,
        test_node_target(CORE_NODE),
        names::DATASTORE_STORE,
    )
    .await
    .expect("listen datastore_store should succeed");
    let store_map = Arc::clone(&store);
    tokio::spawn(async move {
        store_endpoint
            .handle_requests(move |request| {
                let store_map = Arc::clone(&store_map);
                async move {
                    let payload = request.message().payload();
                    let req = DatastoreStoreRequest::decode(payload.as_ref())
                        .expect("decode DatastoreStoreRequest");
                    store_map
                        .lock()
                        .unwrap()
                        .insert(req.key, (req.value, req.encoding));
                    Ok(DatastoreStoreResponse::new()
                        .encode()
                        .expect("encode DatastoreStoreResponse"))
                }
            })
            .await
            .expect("handle datastore_store requests should succeed");
    });

    let mut get_endpoint = ServiceMessenger::listen(
        &server,
        CORE_NODE,
        SERVER_INSTANCE,
        test_node_target(CORE_NODE),
        names::DATASTORE_GET,
    )
    .await
    .expect("listen datastore_get should succeed");
    tokio::spawn(async move {
        get_endpoint
            .handle_requests(move |request| {
                let store = Arc::clone(&store);
                async move {
                    let payload = request.message().payload();
                    let req = DatastoreGetRequest::decode(payload.as_ref())
                        .expect("decode DatastoreGetRequest");
                    let response = match store.lock().unwrap().get(&req.key) {
                        Some((value, encoding)) => {
                            DatastoreGetResponse::found(value.clone(), encoding.clone())
                        }
                        None => DatastoreGetResponse::not_found(),
                    };
                    Ok(response.encode().expect("encode DatastoreGetResponse"))
                }
            })
            .await
            .expect("handle datastore_get requests should succeed");
    });
}

/// Brings up the router/runner, spawns the stub, and waits for both endpoints
/// to be reachable. The router and temp dir are returned so callers hold them
/// for the test's duration.
async fn setup_datastore_stub() -> (ZenohdInstance, TempDir, NodeRunner) {
    let (router, temp_dir, node_runner, server) = start_router_and_runner().await;
    spawn_datastore_stub(server).await;
    wait_until_reachable(node_runner.messenger(), names::DATASTORE_STORE).await;
    wait_until_reachable(node_runner.messenger(), names::DATASTORE_GET).await;
    (router, temp_dir, node_runner)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn store_then_get_round_trips_binary_value() {
    let (_router, _temp_dir, node_runner) = setup_datastore_stub().await;

    // A non-UTF-8 value under an arbitrary-character key proves the wrappers
    // carry raw bytes and don't constrain the key to a Zenoh keyexpr.
    let key = "robot/state**{1}";
    let value = vec![0u8, 255, 0x80, 0xFE, 0x13];

    datastore_store(
        &node_runner,
        key,
        value.clone(),
        "application/octet-stream",
        Duration::from_secs(3),
    )
    .await
    .expect("store should succeed");

    let got = datastore_get(&node_runner, key, Duration::from_secs(3))
        .await
        .expect("get should succeed");

    assert_eq!(
        got,
        Some(StoredValue {
            value,
            encoding: "application/octet-stream".to_string(),
        })
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn get_missing_key_returns_none() {
    let (_router, _temp_dir, node_runner) = setup_datastore_stub().await;

    let got = datastore_get(&node_runner, "never-stored", Duration::from_secs(3))
        .await
        .expect("get should succeed");

    assert_eq!(got, None, "absent key should map to None");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn store_overwrites_existing_key() {
    let (_router, _temp_dir, node_runner) = setup_datastore_stub().await;

    datastore_store(
        &node_runner,
        "k",
        b"first".to_vec(),
        "text/plain",
        Duration::from_secs(3),
    )
    .await
    .expect("first store should succeed");
    datastore_store(
        &node_runner,
        "k",
        b"second".to_vec(),
        "application/json",
        Duration::from_secs(3),
    )
    .await
    .expect("second store should succeed");

    let got = datastore_get(&node_runner, "k", Duration::from_secs(3))
        .await
        .expect("get should succeed")
        .expect("key should be present");

    assert_eq!(got.value, b"second", "later store should win");
    assert_eq!(got.encoding, "application/json");
}
