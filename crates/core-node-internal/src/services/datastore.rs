use crate::Result;
use crate::error::Error;
use crate::services::response::into_service_response;
use core_node_api::ServiceId;
use core_node_api::encoding::{
    DatastoreGetRequest, DatastoreGetResponse, DatastoreListEntry, DatastoreListRequest,
    DatastoreListResponse, DatastoreRemoveRequest, DatastoreRemoveResponse,
    DatastoreStoreExclusiveRequest, DatastoreStoreExclusiveResponse, DatastoreStoreRequest,
    DatastoreStoreResponse,
};
use core_node_api::names;
use node_stack::NodeStack;
use parking_lot::Mutex;
use peppylib::messaging::{SenderTarget, ServiceRequestContext};
use peppylib::types::Payload;
use peppylib::{MessengerHandle, ServiceMessenger};
use std::collections::BTreeMap;
use std::sync::Arc;
use tokio::task::JoinHandle;
use tracing::debug;

/// A stored value: the raw bytes, the Zenoh-style encoding tag the caller
/// supplied, and the `instance_id` of the node that last wrote it. Mirrors
/// Zenoh's `(payload, encoding)` value model so any value type round-trips
/// faithfully.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredValue {
    pub value: Vec<u8>,
    pub encoding: String,
    /// `instance_id` of the node that last wrote this key.
    pub last_modified_by: String,
}

/// One key: its stored value and, for a key `store_exclusive` wrote, the
/// incarnation of the process of its owner, which is the key's last writer.
struct Entry {
    stored: StoredValue,
    owner_incarnation: Option<u64>,
}

impl Entry {
    fn new(
        value: Vec<u8>,
        encoding: String,
        writer: String,
        owner_incarnation: Option<u64>,
    ) -> Self {
        Self {
            stored: StoredValue {
                value,
                encoding,
                last_modified_by: writer,
            },
            owner_incarnation,
        }
    }

    /// The owner of `key` in `entries`, if it has one.
    fn owner_in(entries: &BTreeMap<String, Entry>, key: &str) -> Option<Owner> {
        entries.get(key).and_then(Entry::owner)
    }

    /// The instance that owns this key and the incarnation of its process.
    fn owner(&self) -> Option<Owner> {
        self.owner_incarnation
            .map(|incarnation| (self.stored.last_modified_by.clone(), incarnation))
    }

    /// The owner of this key when it is an instance other than `writer`.
    fn owner_other_than(&self, writer: &str) -> Option<&str> {
        let owner = self.stored.last_modified_by.as_str();
        (self.owner_incarnation.is_some() && owner != writer).then_some(owner)
    }
}

/// The processes of the instances the core node runs, as the datastore reads
/// them to keep an owned key only while its owner's process lives.
pub(crate) trait InstanceLiveness: Send + Sync {
    /// The incarnation of the process `instance_id` runs, or starts once it is
    /// forked.
    fn live_incarnation(&self, instance_id: &str) -> Option<u64>;
}

impl InstanceLiveness for NodeStack {
    fn live_incarnation(&self, instance_id: &str) -> Option<u64> {
        NodeStack::live_incarnation(self, instance_id)
    }
}

/// Daemon-internal, in-memory key/value store shared by the datastore service
/// handlers (`store`, `store_exclusive`, `get`, `list`, `remove`). Keys are any
/// non-empty string, validated when a request is decoded (see
/// [`core_node_api::encoding::DatastoreKey`]); values are arbitrary bytes.
///
/// A key `store_exclusive` wrote has an owner, bound to the incarnation of the
/// owner's process: every other instance's store and remove of it fail. Every
/// access resolves the key against [`InstanceLiveness`], so once the owner's
/// process is gone, whichever way it ended, the key reads as absent and the
/// access that meets it drops it. The lookup runs with the entries unlocked, so
/// a lookup that waits on the node stack holds no lock another request needs. A
/// request carries its sender's instance id only, so a request still in flight
/// when its sender's process ends acts for whichever process then runs under
/// the id.
pub struct Datastore {
    entries: Mutex<BTreeMap<String, Entry>>,
    liveness: Arc<dyn InstanceLiveness>,
    /// The core node this datastore lives on, named in a refusal's remedy.
    core_node_name: String,
}

/// The owner of a key: its instance id and the incarnation of its process.
type Owner = (String, u64);

impl Datastore {
    pub(crate) fn new(
        liveness: Arc<dyn InstanceLiveness>,
        core_node_name: impl Into<String>,
    ) -> Self {
        Self {
            entries: Mutex::new(BTreeMap::new()),
            liveness,
            core_node_name: core_node_name.into(),
        }
    }

    /// Upserts `key` → `(value, encoding)`, recording `writer` as its last
    /// writer. The owner's store of its own key keeps it owned. Fails on a key
    /// another instance owns.
    pub fn store(
        &self,
        key: String,
        value: Vec<u8>,
        encoding: String,
        writer: String,
    ) -> Result<()> {
        self.with_live_key(&key, |entries| {
            let owner_incarnation = match entries.get(&key) {
                Some(entry) => {
                    self.refuse_other_writer(entry, &key, &writer)?;
                    entry.owner_incarnation
                }
                None => None,
            };
            entries.insert(
                key.clone(),
                Entry::new(value, encoding, writer, owner_incarnation),
            );
            Ok(())
        })
    }

    /// Stores `key` → `(value, encoding)` and makes `writer` its owner, bound
    /// to the process `writer` runs, when the key is absent or already
    /// `writer`'s; when another instance owns the key, answers with the key as
    /// its owner wrote it. Fails when the key exists with no owner, and when
    /// the core node runs no process under `writer`.
    pub fn store_exclusive(
        &self,
        key: String,
        value: Vec<u8>,
        encoding: String,
        writer: String,
    ) -> Result<DatastoreStoreExclusiveResponse> {
        let incarnation = self.liveness.live_incarnation(&writer).ok_or_else(|| {
            Error::DatastoreOwnerNotRunning {
                instance_id: writer.clone(),
            }
        })?;
        self.with_live_key(&key, |entries| {
            if let Some(current) = entries.get(&key) {
                if current.owner_incarnation.is_none() {
                    return Err(Error::DatastoreKeyUnowned { key: key.clone() });
                }
                if current.owner_other_than(&writer).is_some() {
                    return Ok(DatastoreStoreExclusiveResponse::Refused {
                        value: current.stored.value.clone(),
                        encoding: current.stored.encoding.clone(),
                        last_modified_by: current.stored.last_modified_by.clone(),
                    });
                }
            }
            entries.insert(
                key.clone(),
                Entry::new(value, encoding, writer, Some(incarnation)),
            );
            Ok(DatastoreStoreExclusiveResponse::Stored)
        })
    }

    /// Returns the stored value for `key`, or `None` if absent.
    pub fn get(&self, key: &str) -> Option<StoredValue> {
        self.with_live_key(key, |entries| {
            entries.get(key).map(|entry| entry.stored.clone())
        })
    }

    /// Returns the metadata (key, encoding, last writer) of every stored key,
    /// in key order. Values are deliberately omitted to keep the list cheap.
    /// The owners are looked up with the entries unlocked, and a key is
    /// dropped only while the owner found dead still owns it.
    pub fn list(&self) -> Vec<DatastoreListEntry> {
        let owned: Vec<(String, Owner)> = self
            .entries
            .lock()
            .iter()
            .filter_map(|(key, entry)| entry.owner().map(|owner| (key.clone(), owner)))
            .collect();
        let dead: Vec<(String, Owner)> = owned
            .into_iter()
            .filter(|(_, owner)| !self.runs(owner))
            .collect();
        let mut entries = self.entries.lock();
        for (key, owner) in &dead {
            if Entry::owner_in(&entries, key).as_ref() == Some(owner) {
                entries.remove(key);
            }
        }
        entries
            .iter()
            .map(|(key, entry)| DatastoreListEntry {
                key: key.clone(),
                encoding: entry.stored.encoding.clone(),
                last_modified_by: entry.stored.last_modified_by.clone(),
            })
            .collect()
    }

    /// Removes `key` for `remover`. Returns `true` if it existed, `false` if
    /// already absent. Fails on a key another instance owns.
    pub fn remove(&self, key: &str, remover: &str) -> Result<bool> {
        self.with_live_key(key, |entries| {
            let Some(entry) = entries.get(key) else {
                return Ok(false);
            };
            self.refuse_other_writer(entry, key, remover)?;
            entries.remove(key);
            Ok(true)
        })
    }

    /// Refuses `writer`'s store or remove of `key` when another instance owns
    /// it, naming the owner and the command that stops it.
    fn refuse_other_writer(&self, entry: &Entry, key: &str, writer: &str) -> Result<()> {
        let Some(owner) = entry.owner_other_than(writer) else {
            return Ok(());
        };
        Err(Error::DatastoreKeyOwned {
            key: key.to_owned(),
            owner: owner.to_owned(),
            stop: core_node_api::stop_command(owner, &self.core_node_name),
        })
    }

    /// Runs `act` on the entries, locked, with `key` dropped when its owner's
    /// process is gone. The owner is read under the lock and looked up with
    /// the entries unlocked; when the key's owner changed meanwhile, the
    /// lookup repeats for the new one.
    fn with_live_key<T>(
        &self,
        key: &str,
        act: impl FnOnce(&mut BTreeMap<String, Entry>) -> T,
    ) -> T {
        loop {
            let owner = Entry::owner_in(&self.entries.lock(), key);
            let gone = owner.as_ref().is_some_and(|owner| !self.runs(owner));
            let mut entries = self.entries.lock();
            if Entry::owner_in(&entries, key) != owner {
                continue;
            }
            if gone {
                entries.remove(key);
            }
            return act(&mut entries);
        }
    }

    /// Whether `owner`'s instance runs the process the key is bound to.
    fn runs(&self, (instance_id, incarnation): &Owner) -> bool {
        self.liveness.live_incarnation(instance_id) == Some(*incarnation)
    }
}

/// Declares the listener for one datastore endpoint.
///
/// The five endpoints differ only in which [`ServiceId`] they bind and which
/// handler they run; binding, spawning, and turning the handler's `Result` into
/// a service response are identical for all of them.
macro_rules! datastore_endpoint {
    ($listen:ident, $service:expr, $handler:ident) => {
        pub async fn $listen(
            messenger: &MessengerHandle,
            core_node_name: &str,
            instance_id: &str,
            node_name: &str,
            store: Arc<Datastore>,
        ) -> Result<JoinHandle<Result<()>>> {
            let mut endpoint = ServiceMessenger::listen(
                messenger,
                core_node_name,
                instance_id,
                SenderTarget::node(node_name, names::CORE_NODE_TAG)?,
                $service.name(),
            )
            .await?;

            Ok(tokio::spawn(async move {
                endpoint
                    .handle_requests(move |context| {
                        let store = Arc::clone(&store);
                        async move {
                            into_service_response(&context, $handler(store.as_ref(), &context))
                        }
                    })
                    .await
                    .map_err(Into::into)
            }))
        }
    };
}

datastore_endpoint!(
    listen_for_datastore_store,
    ServiceId::DatastoreStore,
    handle_store_request
);
datastore_endpoint!(
    listen_for_datastore_store_exclusive,
    ServiceId::DatastoreStoreExclusive,
    handle_store_exclusive_request
);
datastore_endpoint!(
    listen_for_datastore_get,
    ServiceId::DatastoreGet,
    handle_get_request
);
datastore_endpoint!(
    listen_for_datastore_list,
    ServiceId::DatastoreList,
    handle_list_request
);
datastore_endpoint!(
    listen_for_datastore_remove,
    ServiceId::DatastoreRemove,
    handle_remove_request
);

fn handle_store_request(store: &Datastore, context: &ServiceRequestContext) -> Result<Payload> {
    let request = DatastoreStoreRequest::decode(context.message().payload_bytes().as_ref())?;
    let last_modified_by = context.message().instance_id().to_owned();

    debug!(
        "Received datastore store request from {} for key {:?} ({} bytes, encoding {:?})",
        last_modified_by,
        request.key.as_str(),
        request.value.len(),
        request.encoding,
    );

    store.store(
        request.key.into_string(),
        request.value,
        request.encoding,
        last_modified_by,
    )?;

    DatastoreStoreResponse::new().encode().map_err(Into::into)
}

fn handle_get_request(store: &Datastore, context: &ServiceRequestContext) -> Result<Payload> {
    let request = DatastoreGetRequest::decode(context.message().payload_bytes().as_ref())?;

    debug!(
        "Received datastore get request from {} for key {:?}",
        context.message().instance_id(),
        request.key.as_str(),
    );

    let response = match store.get(request.key.as_str()) {
        Some(stored) => {
            DatastoreGetResponse::found(stored.value, stored.encoding, stored.last_modified_by)
        }
        None => DatastoreGetResponse::not_found(),
    };

    response.encode().map_err(Into::into)
}

fn handle_list_request(store: &Datastore, context: &ServiceRequestContext) -> Result<Payload> {
    // Decode to validate the (empty) request shape before answering.
    DatastoreListRequest::decode(context.message().payload_bytes().as_ref())?;

    let entries = store.list();

    debug!(
        "Received datastore list request from {} ({} keys)",
        context.message().instance_id(),
        entries.len(),
    );

    DatastoreListResponse::new(entries)
        .encode()
        .map_err(Into::into)
}

fn handle_remove_request(store: &Datastore, context: &ServiceRequestContext) -> Result<Payload> {
    let request = DatastoreRemoveRequest::decode(context.message().payload_bytes().as_ref())?;

    let removed = store.remove(request.key.as_str(), context.message().instance_id())?;

    debug!(
        "Received datastore remove request from {} for key {:?} (removed: {})",
        context.message().instance_id(),
        request.key.as_str(),
        removed,
    );

    DatastoreRemoveResponse::new(removed)
        .encode()
        .map_err(Into::into)
}

fn handle_store_exclusive_request(
    store: &Datastore,
    context: &ServiceRequestContext,
) -> Result<Payload> {
    let request =
        DatastoreStoreExclusiveRequest::decode(context.message().payload_bytes().as_ref())?
            .into_store();
    let writer = context.message().instance_id().to_owned();

    debug!(
        "Received datastore exclusive store request from {} for key {:?} ({} bytes, encoding {:?})",
        writer,
        request.key.as_str(),
        request.value.len(),
        request.encoding,
    );

    store
        .store_exclusive(
            request.key.into_string(),
            request.value,
            request.encoding,
            writer,
        )?
        .encode()
        .map_err(Into::into)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Barrier;
    use std::sync::atomic::{AtomicBool, Ordering};

    /// The core node the test datastores live on.
    const CORE_NODE: &str = "cn-test";

    /// Instance liveness a test sets by hand: each running instance id maps
    /// to the incarnation of its process.
    #[derive(Default)]
    struct FakeLiveness(Mutex<BTreeMap<String, u64>>);

    impl FakeLiveness {
        fn run(&self, instance_id: &str, incarnation: u64) {
            self.0.lock().insert(instance_id.to_owned(), incarnation);
        }

        fn end(&self, instance_id: &str) {
            self.0.lock().remove(instance_id);
        }
    }

    impl InstanceLiveness for FakeLiveness {
        fn live_incarnation(&self, instance_id: &str) -> Option<u64> {
            self.0.lock().get(instance_id).copied()
        }
    }

    /// [`FakeLiveness`] whose next lookup, once [`Self::gate`] is set, stops
    /// between two barriers: it reaches the first, then waits at the second
    /// until the test lets it through.
    struct GatedLiveness {
        processes: FakeLiveness,
        gate: AtomicBool,
        reached: Barrier,
        release: Barrier,
        /// Every instance id looked up, in order.
        looked_up: Mutex<Vec<String>>,
    }

    impl GatedLiveness {
        fn new() -> Self {
            Self {
                processes: FakeLiveness::default(),
                gate: AtomicBool::new(false),
                reached: Barrier::new(2),
                release: Barrier::new(2),
                looked_up: Mutex::new(Vec::new()),
            }
        }
    }

    impl InstanceLiveness for GatedLiveness {
        fn live_incarnation(&self, instance_id: &str) -> Option<u64> {
            self.looked_up.lock().push(instance_id.to_owned());
            if self.gate.swap(false, Ordering::SeqCst) {
                self.reached.wait();
                self.release.wait();
            }
            self.processes.live_incarnation(instance_id)
        }
    }

    /// A datastore whose `arm` instance owns `bus`, with no lookup recorded and
    /// the gate set for the next lookup.
    fn gated_datastore() -> (Arc<Datastore>, Arc<GatedLiveness>) {
        let liveness = Arc::new(GatedLiveness::new());
        liveness.processes.run("arm", 1);
        let datastore = Arc::new(Datastore::new(Arc::clone(&liveness) as _, CORE_NODE));
        store_exclusive(&datastore, "bus", b"arm", "arm");
        liveness.looked_up.lock().clear();
        liveness.gate.store(true, Ordering::SeqCst);
        (datastore, liveness)
    }

    /// While the old owner's lookup runs, `arm` ends and `gripper` takes
    /// `bus`, as a stop and a new `store_exclusive` do.
    fn reown_bus_during_the_lookup(datastore: &Datastore, liveness: &GatedLiveness) {
        liveness.processes.end("arm");
        liveness.processes.run("gripper", 2);
        datastore
            .entries
            .try_lock()
            .expect("the lookup holds no lock on the entries")
            .insert(
                "bus".to_owned(),
                Entry::new(
                    b"gripper".to_vec(),
                    "text/plain".to_owned(),
                    "gripper".to_owned(),
                    Some(2),
                ),
            );
    }

    /// The key changes owner while the old owner's lookup runs: the stale
    /// answer is dropped and the new owner is looked up.
    #[test]
    fn an_owner_changed_during_its_lookup_is_looked_up_again() {
        let (datastore, liveness) = gated_datastore();
        let reader = {
            let datastore = Arc::clone(&datastore);
            std::thread::spawn(move || datastore.get("bus"))
        };
        liveness.reached.wait();
        reown_bus_during_the_lookup(&datastore, &liveness);
        liveness.release.wait();

        let got = reader.join().expect("reader").expect("the new owner runs");
        assert_eq!(got.last_modified_by, "gripper");
        assert_eq!(*liveness.looked_up.lock(), ["arm", "gripper"]);
    }

    /// `list` keeps a key whose owner changed while the old owner's lookup
    /// ran, though the old owner is gone.
    #[test]
    fn list_keeps_a_key_reowned_during_its_lookups() {
        let (datastore, liveness) = gated_datastore();
        let lister = {
            let datastore = Arc::clone(&datastore);
            std::thread::spawn(move || datastore.list())
        };
        liveness.reached.wait();
        reown_bus_during_the_lookup(&datastore, &liveness);
        liveness.release.wait();

        let listed = lister.join().expect("lister");
        assert_eq!(
            listed
                .iter()
                .map(|entry| (entry.key.as_str(), entry.last_modified_by.as_str()))
                .collect::<Vec<_>>(),
            [("bus", "gripper")]
        );
    }

    fn datastore() -> (Datastore, Arc<FakeLiveness>) {
        let liveness = Arc::new(FakeLiveness::default());
        (
            Datastore::new(Arc::clone(&liveness) as _, CORE_NODE),
            liveness,
        )
    }

    /// `store` of a key no other instance owns.
    fn store(datastore: &Datastore, key: &str, value: &[u8], encoding: &str, writer: &str) {
        datastore
            .store(
                key.to_owned(),
                value.to_vec(),
                encoding.to_owned(),
                writer.to_owned(),
            )
            .expect("no other instance owns the key");
    }

    /// `store_exclusive` from an instance that runs.
    fn store_exclusive(
        datastore: &Datastore,
        key: &str,
        value: &[u8],
        writer: &str,
    ) -> DatastoreStoreExclusiveResponse {
        datastore
            .store_exclusive(
                key.to_owned(),
                value.to_vec(),
                "text/plain".to_owned(),
                writer.to_owned(),
            )
            .expect("the writer runs")
    }

    fn assert_owned_by(result: Result<impl std::fmt::Debug>, key: &str, expected_owner: &str) {
        let err = result.expect_err("another instance owns the key");
        let Error::DatastoreKeyOwned {
            key: refused_key,
            owner,
            stop,
        } = &err
        else {
            panic!("expected DatastoreKeyOwned, got {err:?}");
        };
        assert_eq!(refused_key, key);
        assert_eq!(owner, expected_owner);
        assert_eq!(
            stop,
            &core_node_api::stop_command(expected_owner, CORE_NODE),
            "the remedy names the owner's core node"
        );
    }

    #[test]
    fn store_then_get_returns_value_encoding_and_writer() {
        let (datastore, _liveness) = datastore();
        store(&datastore, "k", b"hello", "text/plain", "writer_node");

        let got = datastore.get("k").expect("key present after store");
        assert_eq!(got.value, b"hello");
        assert_eq!(got.encoding, "text/plain");
        assert_eq!(got.last_modified_by, "writer_node");
    }

    #[test]
    fn get_missing_key_returns_none() {
        let (datastore, _liveness) = datastore();
        assert!(datastore.get("absent").is_none());
    }

    #[test]
    fn store_overwrites_existing_key_and_updates_writer() {
        let (datastore, _liveness) = datastore();
        store(&datastore, "k", b"first", "text/plain", "first_writer");
        store(
            &datastore,
            "k",
            b"second",
            "application/json",
            "second_writer",
        );

        let got = datastore.get("k").expect("key present");
        assert_eq!(got.value, b"second");
        assert_eq!(got.encoding, "application/json");
        assert_eq!(got.last_modified_by, "second_writer", "later writer wins");
    }

    #[test]
    fn stores_and_returns_empty_value() {
        let (datastore, _liveness) = datastore();
        store(&datastore, "empty", b"", "", "w");

        let got = datastore.get("empty").expect("key present");
        assert!(got.value.is_empty());
        assert!(got.encoding.is_empty());
    }

    #[test]
    fn stores_arbitrary_binary_value() {
        let (datastore, _liveness) = datastore();
        let value = [0u8, 255, 0x80, 0xFE];
        store(&datastore, "blob", &value, "application/octet-stream", "w");

        assert_eq!(datastore.get("blob").expect("key present").value, value);
    }

    #[test]
    fn list_returns_metadata_for_all_keys_in_key_order() {
        let (datastore, _liveness) = datastore();
        store(&datastore, "b", b"two", "application/json", "node_b");
        store(&datastore, "a", b"one", "text/plain", "node_a");

        assert_eq!(
            datastore.list(),
            [
                DatastoreListEntry {
                    key: "a".to_owned(),
                    encoding: "text/plain".to_owned(),
                    last_modified_by: "node_a".to_owned(),
                },
                DatastoreListEntry {
                    key: "b".to_owned(),
                    encoding: "application/json".to_owned(),
                    last_modified_by: "node_b".to_owned(),
                },
            ]
        );
    }

    #[test]
    fn list_is_empty_for_fresh_store() {
        let (datastore, _liveness) = datastore();
        assert!(datastore.list().is_empty());
    }

    #[test]
    fn remove_reports_existence_and_deletes_the_key() {
        let (datastore, _liveness) = datastore();
        store(&datastore, "k", b"v", "text/plain", "w");

        assert!(datastore.remove("k", "other").expect("a key with no owner"));
        assert!(datastore.get("k").is_none(), "key is gone after remove");
        assert!(
            !datastore.remove("k", "other").expect("an absent key"),
            "removing an already-absent key returns false"
        );
    }

    #[test]
    fn an_exclusive_store_makes_the_writer_the_owner() {
        let (datastore, liveness) = datastore();
        liveness.run("arm", 1);

        assert_eq!(
            store_exclusive(&datastore, "bus", b"arm", "arm"),
            DatastoreStoreExclusiveResponse::Stored
        );
        let got = datastore.get("bus").expect("the key is stored");
        assert_eq!(got.value, b"arm");
        assert_eq!(got.last_modified_by, "arm");
    }

    #[test]
    fn an_exclusive_store_on_a_key_another_instance_owns_is_refused() {
        let (datastore, liveness) = datastore();
        liveness.run("first", 1);
        liveness.run("second", 2);
        store_exclusive(&datastore, "bus", b"first", "first");

        assert_eq!(
            store_exclusive(&datastore, "bus", b"second", "second"),
            DatastoreStoreExclusiveResponse::Refused {
                value: b"first".to_vec(),
                encoding: "text/plain".to_owned(),
                last_modified_by: "first".to_owned(),
            }
        );
        assert_eq!(datastore.get("bus").expect("owned").value, b"first");
    }

    #[test]
    fn the_owner_stores_exclusively_again_to_update_its_value() {
        let (datastore, liveness) = datastore();
        liveness.run("arm", 1);
        store_exclusive(&datastore, "bus", b"one", "arm");

        assert_eq!(
            store_exclusive(&datastore, "bus", b"two", "arm"),
            DatastoreStoreExclusiveResponse::Stored
        );
        assert_eq!(datastore.get("bus").expect("owned").value, b"two");
    }

    /// A key's kind is fixed when it is created: an exclusive store of a key
    /// `store` wrote is an error, whoever wrote it, and the key stays as it is.
    #[test]
    fn an_exclusive_store_on_a_key_no_instance_owns_is_an_error() {
        let (datastore, liveness) = datastore();
        liveness.run("arm", 1);
        store(&datastore, "bus", b"v", "text/plain", "writer");
        store(&datastore, "own", b"mine", "text/plain", "arm");

        for key in ["bus", "own"] {
            let err = datastore
                .store_exclusive(
                    key.to_owned(),
                    b"arm".to_vec(),
                    "text/plain".to_owned(),
                    "arm".to_owned(),
                )
                .expect_err("the key exists with no owner");
            assert!(
                matches!(&err, Error::DatastoreKeyUnowned { key: k } if k == key),
                "{err:?}"
            );
        }
        assert_eq!(datastore.get("bus").expect("kept").value, b"v");
        assert_eq!(datastore.get("own").expect("kept").value, b"mine");
        store(&datastore, "bus", b"w", "text/plain", "writer");
        store(&datastore, "own", b"theirs", "text/plain", "writer");
    }

    #[test]
    fn an_exclusive_store_from_an_instance_with_no_process_is_an_error() {
        let (datastore, _liveness) = datastore();

        let err = datastore
            .store_exclusive(
                "bus".to_owned(),
                b"v".to_vec(),
                "text/plain".to_owned(),
                "unrun_inst".to_owned(),
            )
            .expect_err("no process runs under the id");
        assert!(
            matches!(&err, Error::DatastoreOwnerNotRunning { instance_id } if instance_id == "unrun_inst"),
            "{err:?}"
        );
        assert!(datastore.get("bus").is_none(), "nothing was written");
    }

    #[test]
    fn another_instances_store_and_remove_of_an_owned_key_fail() {
        let (datastore, liveness) = datastore();
        liveness.run("arm", 1);
        store_exclusive(&datastore, "bus", b"arm", "arm");

        assert_owned_by(
            datastore.store(
                "bus".to_owned(),
                b"other".to_vec(),
                "text/plain".to_owned(),
                "other".to_owned(),
            ),
            "bus",
            "arm",
        );
        assert_owned_by(datastore.remove("bus", "other"), "bus", "arm");
        assert_eq!(datastore.get("bus").expect("still owned").value, b"arm");
    }

    #[test]
    fn the_owners_store_keeps_the_key_owned() {
        let (datastore, liveness) = datastore();
        liveness.run("arm", 1);
        store_exclusive(&datastore, "bus", b"one", "arm");
        store(&datastore, "bus", b"two", "text/plain", "arm");

        assert_owned_by(datastore.remove("bus", "other"), "bus", "arm");
        liveness.end("arm");
        assert!(
            datastore.get("bus").is_none(),
            "the key goes with its owner"
        );
    }

    #[test]
    fn the_owners_remove_releases_the_key() {
        let (datastore, liveness) = datastore();
        liveness.run("arm", 1);
        liveness.run("other", 2);
        store_exclusive(&datastore, "bus", b"arm", "arm");

        assert!(datastore.remove("bus", "arm").expect("the owner removes"));
        assert_eq!(
            store_exclusive(&datastore, "bus", b"other", "other"),
            DatastoreStoreExclusiveResponse::Stored
        );
    }

    #[test]
    fn a_dead_owners_key_reads_absent_and_is_not_listed() {
        let (datastore, liveness) = datastore();
        liveness.run("arm", 1);
        store_exclusive(&datastore, "bus", b"arm", "arm");
        liveness.end("arm");

        assert!(datastore.list().is_empty(), "the key is not listed");
        assert!(datastore.get("bus").is_none(), "the key reads as absent");
    }

    /// A write named for its failure message, and the check of its outcome on
    /// a dead owner's key.
    type DeadOwnerWrite = (&'static str, fn(&Datastore));

    /// Each write meets a dead owner's key first, with no read before it, and
    /// treats the key as absent.
    #[test]
    fn each_write_drops_a_dead_owners_key_on_its_own() {
        let writes: [DeadOwnerWrite; 3] = [
            ("store_exclusive", |datastore| {
                assert_eq!(
                    store_exclusive(datastore, "bus", b"other", "other"),
                    DatastoreStoreExclusiveResponse::Stored
                );
            }),
            ("store", |datastore| {
                store(datastore, "bus", b"other", "text/plain", "other");
            }),
            ("remove", |datastore| {
                assert!(
                    !datastore.remove("bus", "other").expect("nothing owns it"),
                    "the dead owner's key is already gone"
                );
            }),
        ];
        for (write, check) in writes {
            let (datastore, liveness) = datastore();
            liveness.run("arm", 1);
            liveness.run("other", 2);
            store_exclusive(&datastore, "bus", b"arm", "arm");
            liveness.end("arm");

            check(&datastore);
            assert_ne!(
                datastore.get("bus").map(|stored| stored.last_modified_by),
                Some("arm".to_owned()),
                "{write}: the dead owner's value is gone"
            );
        }
    }

    #[test]
    fn a_later_process_under_the_same_id_does_not_inherit_an_owned_key() {
        let (datastore, liveness) = datastore();
        liveness.run("arm", 1);
        store_exclusive(&datastore, "bus", b"first run", "arm");
        liveness.run("arm", 2);
        liveness.run("other", 3);

        assert_eq!(
            store_exclusive(&datastore, "bus", b"other", "other"),
            DatastoreStoreExclusiveResponse::Stored,
            "the first process's key is gone"
        );
    }

    #[test]
    fn a_gone_owner_leaves_its_unowned_keys_and_other_owners_keys() {
        let (datastore, liveness) = datastore();
        liveness.run("arm", 1);
        liveness.run("gripper", 2);
        store_exclusive(&datastore, "bus", b"arm", "arm");
        store_exclusive(&datastore, "hand", b"gripper", "gripper");
        store(&datastore, "calibration", b"c", "text/plain", "arm");
        liveness.end("arm");

        let keys: Vec<String> = datastore
            .list()
            .into_iter()
            .map(|entry| entry.key)
            .collect();
        assert_eq!(keys, ["calibration", "hand"]);
    }
}
