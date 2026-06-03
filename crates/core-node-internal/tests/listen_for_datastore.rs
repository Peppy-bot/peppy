//! Datastore service round-trip tests against the mock messenger.
//!
//! These exercise the `datastore_store` / `datastore_get` endpoints end to end
//! through `ServiceMessenger::poll`, covering the contract that matters for a
//! key/value store: arbitrary byte values, arbitrary-character keys, missing
//! keys, overwrite semantics, and empty values.

mod common;

use common::{
    CALLER_INSTANCE_ID, datastore_get, datastore_list, datastore_remove, datastore_store,
    start_core_node_with_mock_messenger,
};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn store_then_get_returns_text_value() {
    let started = start_core_node_with_mock_messenger().await;

    datastore_store(&started, "greeting", b"hello world", "text/plain").await;
    let response = datastore_get(&started, "greeting").await;

    assert!(response.found, "stored key should be found");
    assert_eq!(response.value, b"hello world");
    assert_eq!(response.encoding, "text/plain");
    assert_eq!(
        response.last_modified_by, CALLER_INSTANCE_ID,
        "get should report the writer's instance_id"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn store_then_get_round_trips_binary_value() {
    let started = start_core_node_with_mock_messenger().await;

    // Non-UTF-8 bytes prove the value is carried as raw `Data`, not text.
    let value = vec![0u8, 255, 0x80, 0xFE, 0x13, 0x37];
    datastore_store(&started, "blob", &value, "application/octet-stream").await;
    let response = datastore_get(&started, "blob").await;

    assert!(response.found);
    assert_eq!(response.value, value);
    assert_eq!(response.encoding, "application/octet-stream");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn store_then_get_round_trips_special_character_key() {
    let started = start_core_node_with_mock_messenger().await;

    // Keys ride inside the request payload (not a Zenoh keyexpr), so slashes,
    // wildcards, whitespace and unicode are all valid.
    let key = "a/b**c?{x} y\nz/日本語/*";
    datastore_store(&started, key, b"value", "text/plain").await;
    let response = datastore_get(&started, key).await;

    assert!(response.found, "special-character key should be found");
    assert_eq!(response.value, b"value");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn get_missing_key_reports_not_found() {
    let started = start_core_node_with_mock_messenger().await;

    let response = datastore_get(&started, "never-stored").await;

    assert!(!response.found, "absent key should report not found");
    assert!(response.value.is_empty(), "absent value should be empty");
    assert!(
        response.encoding.is_empty(),
        "absent encoding should be empty"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn store_overwrites_existing_key() {
    let started = start_core_node_with_mock_messenger().await;

    datastore_store(&started, "k", b"first", "text/plain").await;
    datastore_store(&started, "k", b"second", "application/json").await;
    let response = datastore_get(&started, "k").await;

    assert!(response.found);
    assert_eq!(response.value, b"second", "later store should win");
    assert_eq!(response.encoding, "application/json");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn store_then_get_round_trips_empty_value() {
    let started = start_core_node_with_mock_messenger().await;

    datastore_store(&started, "empty", b"", "").await;
    let response = datastore_get(&started, "empty").await;

    assert!(response.found, "empty value is still a stored value");
    assert!(response.value.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn list_returns_metadata_for_every_key() {
    let started = start_core_node_with_mock_messenger().await;

    assert!(
        datastore_list(&started).await.entries.is_empty(),
        "a fresh store lists no keys"
    );

    datastore_store(&started, "alpha", b"one", "text/plain").await;
    datastore_store(&started, "beta", b"two", "application/json").await;

    let mut entries = datastore_list(&started).await.entries;
    entries.sort_by(|l, r| l.key.cmp(&r.key));

    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].key, "alpha");
    assert_eq!(entries[0].encoding, "text/plain");
    assert_eq!(entries[0].last_modified_by, CALLER_INSTANCE_ID);
    assert_eq!(entries[1].key, "beta");
    assert_eq!(entries[1].encoding, "application/json");
    assert_eq!(entries[1].last_modified_by, CALLER_INSTANCE_ID);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn remove_deletes_key_and_reports_existence() {
    let started = start_core_node_with_mock_messenger().await;

    datastore_store(&started, "doomed", b"bye", "text/plain").await;

    assert!(
        datastore_remove(&started, "doomed").await,
        "removing an existing key returns true"
    );
    assert!(
        !datastore_get(&started, "doomed").await.found,
        "removed key should no longer be found"
    );
    assert!(
        !datastore_remove(&started, "doomed").await,
        "removing an absent key returns false"
    );
}
