//! Integration tests for the `datastore` client wrappers
//! (`datastore::store` / `datastore::get` / `datastore::list` /
//! `datastore::remove`).
//!
//! peppylib can't depend on the core-node daemon crate (that would be a
//! dependency cycle), so the stub here stands in for the daemon: it holds a
//! real in-memory map and serves all four datastore endpoints, letting genuine
//! store→get/list/remove round trips flow through the client wrappers over a
//! real ephemeral zenoh router. The stub records the caller's `instance_id`
//! (from the wire envelope, exactly as the real daemon does) so the
//! `last_modified_by` contract is exercised too.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use core_node_api::encoding::{
    DatastoreGetRequest, DatastoreGetResponse, DatastoreListEntry, DatastoreListRequest,
    DatastoreListResponse, DatastoreRemoveRequest, DatastoreRemoveResponse, DatastoreStoreRequest,
    DatastoreStoreResponse,
};
use core_node_api::names;
use peppylib::datastore::{self, DatastoreEntry, Encoding, StoredValue};
use peppylib::messaging::{MessengerHandle, ServiceMessenger};
use peppylib::runtime::NodeRunner;
use pmi::ZenohdInstance;
use tempfile::TempDir;

use super::common::{
    CLIENT_INSTANCE, CORE_NODE, SERVER_INSTANCE, start_router_and_runner, test_node_target,
    wait_until_reachable,
};

/// Shared `key -> (value, encoding, last_modified_by)` map behind the stub's
/// endpoints. `last_modified_by` mirrors the real daemon recording the writer's
/// `instance_id`.
type StubStore = Arc<Mutex<HashMap<String, (Vec<u8>, String, String)>>>;

/// Spins up a stateful datastore stub serving the four endpoints (store, get,
/// list, remove) against a single shared map. All run for the lifetime of the
/// test (aborted at teardown).
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
                    // The writer's instance_id rides in the wire envelope, not
                    // the payload — read it the same way the real daemon does.
                    let last_modified_by = request.message().instance_id().to_owned();
                    let payload = request.message().payload();
                    let req = DatastoreStoreRequest::decode(payload.as_ref())
                        .expect("decode DatastoreStoreRequest");
                    store_map.lock().unwrap().insert(
                        req.key.into_string(),
                        (req.value, req.encoding, last_modified_by),
                    );
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
    let get_map = Arc::clone(&store);
    tokio::spawn(async move {
        get_endpoint
            .handle_requests(move |request| {
                let store = Arc::clone(&get_map);
                async move {
                    let payload = request.message().payload();
                    let req = DatastoreGetRequest::decode(payload.as_ref())
                        .expect("decode DatastoreGetRequest");
                    let response = match store.lock().unwrap().get(req.key.as_str()) {
                        Some((value, encoding, last_modified_by)) => DatastoreGetResponse::found(
                            value.clone(),
                            encoding.clone(),
                            last_modified_by.clone(),
                        ),
                        None => DatastoreGetResponse::not_found(),
                    };
                    Ok(response.encode().expect("encode DatastoreGetResponse"))
                }
            })
            .await
            .expect("handle datastore_get requests should succeed");
    });

    let mut list_endpoint = ServiceMessenger::listen(
        &server,
        CORE_NODE,
        SERVER_INSTANCE,
        test_node_target(CORE_NODE),
        names::DATASTORE_LIST,
    )
    .await
    .expect("listen datastore_list should succeed");
    let list_map = Arc::clone(&store);
    tokio::spawn(async move {
        list_endpoint
            .handle_requests(move |request| {
                let store = Arc::clone(&list_map);
                async move {
                    let payload = request.message().payload();
                    DatastoreListRequest::decode(payload.as_ref())
                        .expect("decode DatastoreListRequest");
                    let entries = store
                        .lock()
                        .unwrap()
                        .iter()
                        .map(
                            |(key, (_value, encoding, last_modified_by))| DatastoreListEntry {
                                key: key.clone(),
                                encoding: encoding.clone(),
                                last_modified_by: last_modified_by.clone(),
                            },
                        )
                        .collect();
                    Ok(DatastoreListResponse::new(entries)
                        .encode()
                        .expect("encode DatastoreListResponse"))
                }
            })
            .await
            .expect("handle datastore_list requests should succeed");
    });

    let mut remove_endpoint = ServiceMessenger::listen(
        &server,
        CORE_NODE,
        SERVER_INSTANCE,
        test_node_target(CORE_NODE),
        names::DATASTORE_REMOVE,
    )
    .await
    .expect("listen datastore_remove should succeed");
    let remove_map = Arc::clone(&store);
    tokio::spawn(async move {
        remove_endpoint
            .handle_requests(move |request| {
                let store = Arc::clone(&remove_map);
                async move {
                    let payload = request.message().payload();
                    let req = DatastoreRemoveRequest::decode(payload.as_ref())
                        .expect("decode DatastoreRemoveRequest");
                    let removed = store.lock().unwrap().remove(req.key.as_str()).is_some();
                    Ok(DatastoreRemoveResponse::new(removed)
                        .encode()
                        .expect("encode DatastoreRemoveResponse"))
                }
            })
            .await
            .expect("handle datastore_remove requests should succeed");
    });
}

/// Brings up the router/runner, spawns the stub, and waits for all endpoints
/// to be reachable. The router and temp dir are returned so callers hold them
/// for the test's duration.
async fn setup_datastore_stub() -> (ZenohdInstance, TempDir, NodeRunner) {
    let (router, temp_dir, node_runner, server) = start_router_and_runner().await;
    spawn_datastore_stub(server).await;
    wait_until_reachable(node_runner.messenger(), names::DATASTORE_STORE).await;
    wait_until_reachable(node_runner.messenger(), names::DATASTORE_GET).await;
    wait_until_reachable(node_runner.messenger(), names::DATASTORE_LIST).await;
    wait_until_reachable(node_runner.messenger(), names::DATASTORE_REMOVE).await;
    (router, temp_dir, node_runner)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn store_then_get_round_trips_binary_value() {
    let (_router, _temp_dir, node_runner) = setup_datastore_stub().await;

    // A non-UTF-8 value under a node-name-style key proves the wrappers carry
    // raw bytes intact while still requiring a valid key.
    let key = "robot_state-1";
    let value = vec![0u8, 255, 0x80, 0xFE, 0x13];

    datastore::store(
        &node_runner,
        key,
        value.clone(),
        Encoding::APPLICATION_OCTET_STREAM,
        Duration::from_secs(3),
    )
    .await
    .expect("store should succeed");

    let got = datastore::get(&node_runner, key, Duration::from_secs(3))
        .await
        .expect("get should succeed");

    assert_eq!(
        got,
        Some(StoredValue {
            value,
            encoding: Encoding::APPLICATION_OCTET_STREAM,
            // The stub records the caller's instance_id, just like the daemon.
            last_modified_by: CLIENT_INSTANCE.to_owned(),
        })
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn store_rejects_invalid_key() {
    let (_router, _temp_dir, node_runner) = setup_datastore_stub().await;

    // A slash is outside the node-name character set, so the wrapper rejects
    // the key locally, before any request reaches the core node.
    let err = datastore::store(
        &node_runner,
        "robot/state",
        b"value".to_vec(),
        Encoding::TEXT_PLAIN,
        Duration::from_secs(3),
    )
    .await
    .expect_err("a key with a slash should be rejected");

    assert!(
        matches!(
            err,
            peppylib::PeppyError::CoreNodeApi(core_node_api::Error::InvalidDatastoreKey(
                core_node_api::encoding::DatastoreKeyError::ForbiddenCharacter {
                    character: '/',
                    ..
                }
            ))
        ),
        "expected a forbidden-character (`/`) datastore key error, got: {err:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn get_missing_key_returns_none() {
    let (_router, _temp_dir, node_runner) = setup_datastore_stub().await;

    let got = datastore::get(&node_runner, "never-stored", Duration::from_secs(3))
        .await
        .expect("get should succeed");

    assert_eq!(got, None, "absent key should map to None");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn store_overwrites_existing_key() {
    let (_router, _temp_dir, node_runner) = setup_datastore_stub().await;

    datastore::store(
        &node_runner,
        "k",
        b"first".to_vec(),
        "text/plain",
        Duration::from_secs(3),
    )
    .await
    .expect("first store should succeed");
    datastore::store(
        &node_runner,
        "k",
        b"second".to_vec(),
        "application/json",
        Duration::from_secs(3),
    )
    .await
    .expect("second store should succeed");

    let got = datastore::get(&node_runner, "k", Duration::from_secs(3))
        .await
        .expect("get should succeed")
        .expect("key should be present");

    assert_eq!(got.value, b"second", "later store should win");
    assert_eq!(got.encoding, "application/json");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn list_returns_entries_with_modifier() {
    let (_router, _temp_dir, node_runner) = setup_datastore_stub().await;

    datastore::store(
        &node_runner,
        "alpha",
        b"one".to_vec(),
        Encoding::TEXT_PLAIN,
        Duration::from_secs(3),
    )
    .await
    .expect("store alpha should succeed");
    datastore::store(
        &node_runner,
        "beta",
        b"two".to_vec(),
        Encoding::APPLICATION_JSON,
        Duration::from_secs(3),
    )
    .await
    .expect("store beta should succeed");

    let mut entries = datastore::list(&node_runner, Duration::from_secs(3))
        .await
        .expect("list should succeed");
    entries.sort_by(|l, r| l.key.cmp(&r.key));

    assert_eq!(
        entries,
        vec![
            DatastoreEntry {
                key: "alpha".to_owned(),
                encoding: Encoding::TEXT_PLAIN,
                last_modified_by: CLIENT_INSTANCE.to_owned(),
            },
            DatastoreEntry {
                key: "beta".to_owned(),
                encoding: Encoding::APPLICATION_JSON,
                last_modified_by: CLIENT_INSTANCE.to_owned(),
            },
        ]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn remove_deletes_key_and_reports_existence() {
    let (_router, _temp_dir, node_runner) = setup_datastore_stub().await;

    datastore::store(
        &node_runner,
        "doomed",
        b"bye".to_vec(),
        Encoding::TEXT_PLAIN,
        Duration::from_secs(3),
    )
    .await
    .expect("store should succeed");

    let removed = datastore::remove(&node_runner, "doomed", Duration::from_secs(3))
        .await
        .expect("remove should succeed");
    assert!(removed, "removing an existing key returns true");

    let got = datastore::get(&node_runner, "doomed", Duration::from_secs(3))
        .await
        .expect("get should succeed");
    assert_eq!(got, None, "removed key should be gone");

    let removed_again = datastore::remove(&node_runner, "doomed", Duration::from_secs(3))
        .await
        .expect("remove should succeed");
    assert!(!removed_again, "removing an absent key returns false");
}
