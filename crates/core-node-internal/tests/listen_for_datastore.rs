//! Datastore service tests through `peppylib::core_node::transport::poll`.
//!
//! The key/value contract against the mock messenger: arbitrary byte values,
//! node-name-style keys, missing keys, overwrite semantics, and empty values.
//! Then `datastore_store_exclusive` against running instances: one instance
//! owns a key, every other instance's store and remove of it fail, and the key
//! goes when the owner's process ends, by a clean exit, a stop, a crash or a
//! restart, without a later process under the owner's id owning it.

mod common;

use common::{
    CALLER_INSTANCE_ID, StartedCoreNode, TEST_NODE_TAG, WAIT, add_and_build_process_node,
    datastore_get, datastore_list, datastore_remove, datastore_store, datastore_store_exclusive_as,
    fast_health_monitor, install_kill_on_shutdown_listener, mocks, name, never, on_failure,
    poll_core_node_as, poll_until, run, spawn_real_stuck_instance,
    start_core_node_with_mock_messenger, start_core_node_with_supervision, wait_for_state,
};
use config::node::RestartRequest;
use core_node::RestartBackoff;
use core_node::test_support::{
    EXIT_CLEANLY_ON_TERM, instant_restart_backoff, parked_restart_backoff, signal_process,
};
use core_node_api::encoding::{
    DatastoreRemoveRequest, DatastoreStoreExclusiveResponse, DatastoreStoreRequest, NodeStopRequest,
};
use core_node_api::{InstanceState, stop_command};
use peppylib::core_node::transport::poll_node_stop;
use std::time::Duration;
use tempfile::TempDir;

/// The timeout of a request the daemon answers without long work.
const REQUEST: Duration = Duration::from_secs(5);

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
async fn store_then_get_round_trips_node_name_key() {
    let started = start_core_node_with_mock_messenger().await;

    // A key in the node-name character set: letters, digits, `_` and `-`.
    let key = "robot_state-1";
    datastore_store(&started, key, b"value", "text/plain").await;
    let response = datastore_get(&started, key).await;

    assert!(response.found, "node-name key should be found");
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

/// The node whose instances own keys in the exclusive-store tests.
const OWNER_NODE: &str = "owner_node";
/// The key the exclusive-store tests contend for.
const DRIVER_KEY: &str = "arm/left/driver";

/// Adds and builds [`OWNER_NODE`], whose process runs until a signal ends it
/// and exits cleanly on SIGTERM.
async fn add_owner_node(started: &StartedCoreNode) -> TempDir {
    add_and_build_process_node(
        started,
        OWNER_NODE,
        TEST_NODE_TAG,
        &["sh", "-c", EXIT_CLEANLY_ON_TERM],
    )
    .await
}

/// The daemon's answer to an exclusive store of [`DRIVER_KEY`] as `writer`.
async fn store_driver_key_exclusively(
    started: &StartedCoreNode,
    writer: &str,
) -> DatastoreStoreExclusiveResponse {
    datastore_store_exclusive_as(started, writer, DRIVER_KEY, writer.as_bytes())
        .await
        .unwrap_or_else(|e| panic!("the exclusive store of `{writer}` is answered: {e}"))
}

fn assert_refused_naming(err: peppylib::PeppyError, started: &StartedCoreNode, owner: &str) {
    let message = err.to_string();
    assert!(
        message.contains(&format!("is owned by instance '{owner}'")),
        "{message}"
    );
    assert!(
        message.contains(&stop_command(owner, &started.core_node_name)),
        "the remedy names the owner's core node: {message}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_exclusive_store_from_an_instance_the_daemon_does_not_run_fails() {
    let started = start_core_node_with_mock_messenger().await;

    let message = datastore_store_exclusive_as(&started, "unrun_inst", DRIVER_KEY, b"v")
        .await
        .expect_err("no process runs under the id")
        .to_string();
    assert!(
        message.contains("runs no process under that id") && message.contains("peppy node run"),
        "{message}"
    );
    assert!(!datastore_get(&started, DRIVER_KEY).await.found);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_exclusive_store_on_a_key_no_instance_owns_fails_naming_the_remedy() {
    let started = start_core_node_with_mock_messenger().await;
    let _dir = add_owner_node(&started).await;
    let _arm =
        spawn_real_stuck_instance(&started, OWNER_NODE, TEST_NODE_TAG, &name("arm_inst")).await;
    datastore_store(&started, DRIVER_KEY, b"v", "text/plain").await;

    let message = datastore_store_exclusive_as(&started, "arm_inst", DRIVER_KEY, b"arm")
        .await
        .expect_err("the key exists with no owner")
        .to_string();
    assert!(
        message.contains("exists with no owner") && message.contains("`remove`"),
        "{message}"
    );
    assert_eq!(datastore_get(&started, DRIVER_KEY).await.value, b"v");
}

/// Two running instances race for one key: one owns it, the other's
/// exclusive store is refused naming the owner, its store and remove of the
/// key fail, and the owner's remove deletes the key.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn one_running_instance_owns_a_key_and_another_cannot_write_it() {
    let started = start_core_node_with_mock_messenger().await;
    let _dir = add_owner_node(&started).await;
    let _left =
        spawn_real_stuck_instance(&started, OWNER_NODE, TEST_NODE_TAG, &name("left_inst")).await;
    let _right =
        spawn_real_stuck_instance(&started, OWNER_NODE, TEST_NODE_TAG, &name("right_inst")).await;

    let outcomes = tokio::join!(
        store_driver_key_exclusively(&started, "left_inst"),
        store_driver_key_exclusively(&started, "right_inst")
    );
    let (owner, other) = match &outcomes {
        (
            DatastoreStoreExclusiveResponse::Stored,
            DatastoreStoreExclusiveResponse::Refused {
                last_modified_by, ..
            },
        ) if last_modified_by == "left_inst" => ("left_inst", "right_inst"),
        (
            DatastoreStoreExclusiveResponse::Refused {
                last_modified_by, ..
            },
            DatastoreStoreExclusiveResponse::Stored,
        ) if last_modified_by == "right_inst" => ("right_inst", "left_inst"),
        outcomes => panic!("one owner, the other refused naming it: {outcomes:?}"),
    };

    let store =
        DatastoreStoreRequest::new(DRIVER_KEY, b"other".to_vec(), "text/plain").expect("valid key");
    assert_refused_naming(
        poll_core_node_as(&started, other, &store, REQUEST)
            .await
            .expect_err("another instance owns the key"),
        &started,
        owner,
    );
    let remove = DatastoreRemoveRequest::new(DRIVER_KEY).expect("valid key");
    assert_refused_naming(
        poll_core_node_as(&started, other, &remove, REQUEST)
            .await
            .expect_err("another instance owns the key"),
        &started,
        owner,
    );
    let stored = datastore_get(&started, DRIVER_KEY).await;
    assert_eq!(stored.last_modified_by, owner);
    assert_eq!(stored.value, owner.as_bytes());

    let removed = poll_core_node_as(&started, owner, &remove, REQUEST)
        .await
        .expect("the owner removes its key");
    assert!(removed.removed);
    assert!(!datastore_get(&started, DRIVER_KEY).await.found);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stopping_the_owner_removes_its_owned_key_and_keeps_its_unowned_keys() {
    const OWNER: &str = "owner_inst";
    let started = start_core_node_with_mock_messenger().await;
    let _dir = add_owner_node(&started).await;
    let owner = spawn_real_stuck_instance(&started, OWNER_NODE, TEST_NODE_TAG, &name(OWNER)).await;
    let _other =
        spawn_real_stuck_instance(&started, OWNER_NODE, TEST_NODE_TAG, &name("other_inst")).await;
    assert_eq!(
        store_driver_key_exclusively(&started, OWNER).await,
        DatastoreStoreExclusiveResponse::Stored
    );
    let calibration =
        DatastoreStoreRequest::new("calibration", b"c".to_vec(), "text/plain").expect("key");
    poll_core_node_as(&started, OWNER, &calibration, REQUEST)
        .await
        .expect("the owner stores a key no instance owns");

    // `node_stop` reaps the process, so the guard's own stop is not needed.
    let pid = owner.pid;
    std::mem::forget(owner);
    let _kill_on_shutdown =
        install_kill_on_shutdown_listener(&started, OWNER_NODE, &name(OWNER), pid).await;
    poll_node_stop(
        &NodeStopRequest::new(OWNER),
        &started.caller_handle,
        &started.core_node_name,
        CALLER_INSTANCE_ID,
        common::core_node_target(&started.core_node_name),
        &started.core_node_name,
        Duration::from_secs(30),
    )
    .await
    .expect("node_stop completes");

    assert_eq!(
        store_driver_key_exclusively(&started, "other_inst").await,
        DatastoreStoreExclusiveResponse::Stored,
        "the stopped owner's key is gone"
    );
    assert!(
        datastore_get(&started, "calibration").await.found,
        "its key no instance owns stays"
    );
}

/// A new instance started under the id of an owner that stopped runs another
/// process, so the key the first process owned is gone, not handed on.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_new_instance_under_the_owners_id_does_not_inherit_its_key() {
    const OWNER: &str = "reused_inst";
    let started = start_core_node_with_mock_messenger().await;
    let _dir = add_owner_node(&started).await;
    let first = spawn_real_stuck_instance(&started, OWNER_NODE, TEST_NODE_TAG, &name(OWNER)).await;
    assert_eq!(
        store_driver_key_exclusively(&started, OWNER).await,
        DatastoreStoreExclusiveResponse::Stored
    );
    drop(first);
    let _second =
        spawn_real_stuck_instance(&started, OWNER_NODE, TEST_NODE_TAG, &name(OWNER)).await;

    datastore_store(&started, DRIVER_KEY, b"anyone", "text/plain").await;
}

/// Runs an owner under `restart` on a daemon restarting with `backoff`, has it
/// store [`DRIVER_KEY`] exclusively, ends its process with `signal`, waits for
/// the instance to reach `ended`, then stores the key as another writer: the
/// store fails while the owner still owns the key.
async fn assert_the_owners_key_goes(
    owner: &str,
    backoff: RestartBackoff,
    restart: RestartRequest,
    signal: &str,
    ended: InstanceState,
) {
    let started = start_core_node_with_supervision(fast_health_monitor(3), backoff).await;
    let _dir = add_owner_node(&started).await;
    let _mocks = mocks(&started, OWNER_NODE, owner).await;
    let pid = run(&started, OWNER_NODE, owner, restart).await;
    assert_eq!(
        store_driver_key_exclusively(&started, owner).await,
        DatastoreStoreExclusiveResponse::Stored
    );

    signal_process(pid, signal);
    wait_for_state(&started, owner, ended).await;

    datastore_store(&started, DRIVER_KEY, b"anyone", "text/plain").await;
}

/// An owner whose process exits cleanly, as a one-shot node does, finishes,
/// and its key goes with the process.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_clean_exit_removes_the_owners_key() {
    assert_the_owners_key_goes(
        "finishing_inst",
        instant_restart_backoff(),
        never(),
        "-TERM",
        InstanceState::Finished,
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_crash_removes_the_owners_key() {
    assert_the_owners_key_goes(
        "crashing_inst",
        instant_restart_backoff(),
        never(),
        "-9",
        InstanceState::Failed,
    )
    .await;
}

/// An owner that crashed and waits out its restart backoff runs no process,
/// so it owns no key while it waits.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_owner_waiting_to_restart_owns_no_key() {
    assert_the_owners_key_goes(
        "waiting_inst",
        parked_restart_backoff(),
        on_failure(),
        "-9",
        InstanceState::Restarting,
    )
    .await;
}

/// The process a restart starts is not the one that stored the key: once it
/// runs, another instance takes the key, and the restarted owner is refused.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_restarted_owner_does_not_own_its_previous_processs_key() {
    const OWNER: &str = "restarting_inst";
    const OTHER: &str = "other_inst";
    let started =
        start_core_node_with_supervision(fast_health_monitor(3), instant_restart_backoff()).await;
    let _dir = add_owner_node(&started).await;
    let _owner_mocks = mocks(&started, OWNER_NODE, OWNER).await;
    let _other_mocks = mocks(&started, OWNER_NODE, OTHER).await;
    let pid = run(&started, OWNER_NODE, OWNER, on_failure()).await;
    run(&started, OWNER_NODE, OTHER, never()).await;
    assert_eq!(
        store_driver_key_exclusively(&started, OWNER).await,
        DatastoreStoreExclusiveResponse::Stored
    );

    signal_process(pid, "-9");
    poll_until(WAIT, "the owner runs a restarted process", || {
        started
            .node_stack
            .find_by_instance_id(&name(OWNER))
            .and_then(|slot| slot.pid())
            .filter(|restarted| *restarted != pid)
    })
    .await;

    assert_eq!(
        store_driver_key_exclusively(&started, OTHER).await,
        DatastoreStoreExclusiveResponse::Stored,
        "the first process's key is gone"
    );
    assert!(
        matches!(
            store_driver_key_exclusively(&started, OWNER).await,
            DatastoreStoreExclusiveResponse::Refused { last_modified_by, .. } if last_modified_by == OTHER
        ),
        "the restarted owner does not own the key"
    );
}
