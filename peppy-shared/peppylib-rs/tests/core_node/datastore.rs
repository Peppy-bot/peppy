//! Integration tests for the `datastore` client wrappers
//! (`datastore::store` / `datastore::store_exclusive` / `datastore::get` /
//! `datastore::list` / `datastore::remove`).
//!
//! peppylib can't depend on the core-node daemon crate (that would be a
//! dependency cycle), so the stub here stands in for the daemon: it holds a
//! real in-memory map and serves all five datastore endpoints, letting genuine
//! store→get/list/remove round trips flow through the client wrappers over a
//! real ephemeral zenoh router. The stub records the caller's `instance_id`
//! (from the wire envelope, exactly as the real daemon does) so the
//! `last_modified_by` contract is exercised too.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use core_node_api::ServiceId;
use core_node_api::encoding::{
    DatastoreGetRequest, DatastoreGetResponse, DatastoreListEntry, DatastoreListRequest,
    DatastoreListResponse, DatastoreRemoveRequest, DatastoreRemoveResponse,
    DatastoreStoreExclusiveRequest, DatastoreStoreExclusiveResponse, DatastoreStoreRequest,
    DatastoreStoreResponse,
};
use peppylib::datastore::{self, DatastoreEntry, Encoding, ExclusiveStore, StoredValue};
use peppylib::messaging::MessengerHandle;
use peppylib::runtime::NodeRunner;
use peppylib::testing::EphemeralRouter;
use tempfile::TempDir;

use super::common::{
    CLIENT_INSTANCE, listen_as_core_node, start_router_and_runner, wait_until_reachable,
};

/// Shared `key -> (value, encoding, last_modified_by)` map behind the stub's
/// endpoints. `last_modified_by` mirrors the real daemon recording the writer's
/// `instance_id`.
type StubStore = Arc<Mutex<HashMap<String, (Vec<u8>, String, String)>>>;

/// The key the stub's exclusive store reports another instance owns.
const OWNED_KEY: &str = "owned";
/// The instance the stub reports owns [`OWNED_KEY`], and the value it stored.
const OWNER: &str = "owner_inst";
const OWNER_VALUE: &[u8] = b"held";

/// Spins up a stateful datastore stub serving the five endpoints (store,
/// store_exclusive, get, list, remove) against a single shared map. Its
/// exclusive store refuses [`OWNED_KEY`], naming [`OWNER`], and stores any
/// other key. All run for the lifetime of the test (aborted at teardown).
async fn spawn_datastore_stub(server: MessengerHandle) {
    let store: StubStore = Arc::new(Mutex::new(HashMap::new()));

    let mut store_endpoint = listen_as_core_node(&server, ServiceId::DatastoreStore).await;
    let store_map = Arc::clone(&store);
    tokio::spawn(async move {
        store_endpoint
            .handle_requests(move |request| {
                let store_map = Arc::clone(&store_map);
                async move {
                    // The writer's instance_id rides in the wire envelope, not
                    // the payload: read it the same way the real daemon does.
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

    let mut get_endpoint = listen_as_core_node(&server, ServiceId::DatastoreGet).await;
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

    let mut list_endpoint = listen_as_core_node(&server, ServiceId::DatastoreList).await;
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

    let mut remove_endpoint = listen_as_core_node(&server, ServiceId::DatastoreRemove).await;
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

    let mut store_exclusive_endpoint =
        listen_as_core_node(&server, ServiceId::DatastoreStoreExclusive).await;
    let store_exclusive_map = Arc::clone(&store);
    tokio::spawn(async move {
        store_exclusive_endpoint
            .handle_requests(move |request| {
                let store = Arc::clone(&store_exclusive_map);
                async move {
                    let writer = request.message().instance_id().to_owned();
                    let payload = request.message().payload();
                    let req = DatastoreStoreExclusiveRequest::decode(payload.as_ref())
                        .expect("decode DatastoreStoreExclusiveRequest")
                        .into_store();
                    let response = if req.key.as_str() == OWNED_KEY {
                        DatastoreStoreExclusiveResponse::Refused {
                            value: OWNER_VALUE.to_vec(),
                            encoding: Encoding::TEXT_PLAIN.to_string(),
                            last_modified_by: OWNER.to_owned(),
                        }
                    } else {
                        store
                            .lock()
                            .unwrap()
                            .insert(req.key.into_string(), (req.value, req.encoding, writer));
                        DatastoreStoreExclusiveResponse::Stored
                    };
                    Ok(response
                        .encode()
                        .expect("encode DatastoreStoreExclusiveResponse"))
                }
            })
            .await
            .expect("handle datastore_store_exclusive requests should succeed");
    });
}

/// Brings up the router/runner, spawns the stub, and waits for all endpoints
/// to be reachable. The router and temp dir are returned so callers hold them
/// for the test's duration.
async fn setup_datastore_stub() -> (EphemeralRouter, TempDir, NodeRunner) {
    let (router, temp_dir, node_runner, server) = start_router_and_runner().await;
    spawn_datastore_stub(server).await;
    wait_until_reachable(node_runner.messenger(), ServiceId::DatastoreStore.name()).await;
    wait_until_reachable(node_runner.messenger(), ServiceId::DatastoreGet.name()).await;
    wait_until_reachable(node_runner.messenger(), ServiceId::DatastoreList.name()).await;
    wait_until_reachable(node_runner.messenger(), ServiceId::DatastoreRemove.name()).await;
    wait_until_reachable(
        node_runner.messenger(),
        ServiceId::DatastoreStoreExclusive.name(),
    )
    .await;
    (router, temp_dir, node_runner)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn store_exclusive_stores_an_absent_key() {
    let (_router, _temp_dir, node_runner) = setup_datastore_stub().await;

    let answer = datastore::store_exclusive(
        &node_runner,
        "arm/left/driver",
        b"mine".to_vec(),
        Encoding::TEXT_PLAIN,
        Duration::from_secs(3),
    )
    .await
    .expect("store_exclusive should succeed");
    assert_eq!(answer, ExclusiveStore::Stored);

    let got = datastore::get(&node_runner, "arm/left/driver", Duration::from_secs(3))
        .await
        .expect("get should succeed");
    assert_eq!(
        got,
        Some(StoredValue {
            value: b"mine".to_vec(),
            encoding: Encoding::TEXT_PLAIN,
            last_modified_by: CLIENT_INSTANCE.to_owned(),
        })
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn store_exclusive_on_an_owned_key_answers_with_the_owners_value() {
    let (_router, _temp_dir, node_runner) = setup_datastore_stub().await;

    let answer = datastore::store_exclusive(
        &node_runner,
        OWNED_KEY,
        b"mine".to_vec(),
        Encoding::TEXT_PLAIN,
        Duration::from_secs(3),
    )
    .await
    .expect("store_exclusive should succeed");

    assert_eq!(
        answer,
        ExclusiveStore::Refused {
            current: StoredValue {
                value: OWNER_VALUE.to_vec(),
                encoding: Encoding::TEXT_PLAIN,
                last_modified_by: OWNER.to_owned(),
            }
        }
    );
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

    // Datastore keys are arbitrary non-empty strings (slashes, dots, spaces and
    // Unicode are all valid), so the only key the wrapper rejects locally,
    // before any request reaches the core node, is the empty string.
    let err = datastore::store(
        &node_runner,
        "",
        b"value".to_vec(),
        Encoding::TEXT_PLAIN,
        Duration::from_secs(3),
    )
    .await
    .expect_err("an empty key should be rejected");

    assert!(
        matches!(
            err,
            peppylib::PeppyError::CoreNodeApi(core_node_api::Error::InvalidDatastoreKey(
                core_node_api::encoding::DatastoreKeyError::Empty
            ))
        ),
        "expected an empty-key datastore error, got: {err:?}"
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
