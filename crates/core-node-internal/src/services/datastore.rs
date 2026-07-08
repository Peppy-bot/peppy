use crate::Result;
use crate::services::response::into_service_response;
use core_node_api::ServiceId;
use core_node_api::encoding::{
    DatastoreGetRequest, DatastoreGetResponse, DatastoreListEntry, DatastoreListRequest,
    DatastoreListResponse, DatastoreRemoveRequest, DatastoreRemoveResponse, DatastoreStoreRequest,
    DatastoreStoreResponse,
};
use core_node_api::names;
use parking_lot::RwLock;
use peppylib::messaging::{SenderTarget, ServiceRequestContext};
use peppylib::types::Payload;
use peppylib::{MessengerHandle, ServiceMessenger};
use std::collections::HashMap;
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

/// Daemon-internal, in-memory key/value store shared by the datastore service
/// handlers (`store`, `get`, `list`, `remove`). Keys use the node-name
/// character set (ASCII letters, digits, `_` and `-`), validated when a request
/// is decoded (see [`core_node_api::encoding::DatastoreKey`]); values are
/// arbitrary bytes.
#[derive(Debug, Default)]
pub struct Datastore {
    map: RwLock<HashMap<String, StoredValue>>,
}

impl Datastore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Upserts `key` → `(value, encoding)`, recording `last_modified_by` as the
    /// writer. A later store under the same key overwrites the previous value.
    pub fn store(&self, key: String, value: Vec<u8>, encoding: String, last_modified_by: String) {
        self.map.write().insert(
            key,
            StoredValue {
                value,
                encoding,
                last_modified_by,
            },
        );
    }

    /// Returns the stored value for `key`, or `None` if absent.
    pub fn get(&self, key: &str) -> Option<StoredValue> {
        self.map.read().get(key).cloned()
    }

    /// Returns the metadata (key, encoding, last writer) of every stored key.
    /// Values are deliberately omitted to keep the list cheap. Order is
    /// unspecified: the store is a `HashMap`.
    pub fn list(&self) -> Vec<DatastoreListEntry> {
        self.map
            .read()
            .iter()
            .map(|(key, stored)| DatastoreListEntry {
                key: key.clone(),
                encoding: stored.encoding.clone(),
                last_modified_by: stored.last_modified_by.clone(),
            })
            .collect()
    }

    /// Removes `key`. Returns `true` if it existed, `false` if already absent.
    pub fn remove(&self, key: &str) -> bool {
        self.map.write().remove(key).is_some()
    }
}

/// Spawns a listener for one datastore service. Every datastore endpoint shares
/// the same wiring (listen on the core node's service channel, then answer each
/// request through `handler`), so they differ only by `service_name` and the
/// per-request `handler`. Failures from `handler` are wrapped by
/// [`into_service_response`].
async fn spawn_datastore_listener(
    messenger: &MessengerHandle,
    core_node_name: &str,
    instance_id: &str,
    node_name: &str,
    service_name: &str,
    store: Arc<Datastore>,
    handler: fn(&Datastore, &ServiceRequestContext) -> Result<Payload>,
) -> Result<JoinHandle<Result<()>>> {
    let mut endpoint = ServiceMessenger::listen(
        messenger,
        core_node_name,
        instance_id,
        SenderTarget::node(node_name, names::CORE_NODE_TAG)?,
        service_name,
    )
    .await?;

    let handle = tokio::spawn(async move {
        endpoint
            .handle_requests(move |context| {
                let store = Arc::clone(&store);
                async move { into_service_response(&context, handler(store.as_ref(), &context)) }
            })
            .await
            .map_err(Into::into)
    });

    Ok(handle)
}

pub async fn listen_for_datastore_store(
    messenger: &MessengerHandle,
    core_node_name: &str,
    instance_id: &str,
    node_name: &str,
    store: Arc<Datastore>,
) -> Result<JoinHandle<Result<()>>> {
    spawn_datastore_listener(
        messenger,
        core_node_name,
        instance_id,
        node_name,
        ServiceId::DatastoreStore.name(),
        store,
        handle_store_request,
    )
    .await
}

pub async fn listen_for_datastore_get(
    messenger: &MessengerHandle,
    core_node_name: &str,
    instance_id: &str,
    node_name: &str,
    store: Arc<Datastore>,
) -> Result<JoinHandle<Result<()>>> {
    spawn_datastore_listener(
        messenger,
        core_node_name,
        instance_id,
        node_name,
        ServiceId::DatastoreGet.name(),
        store,
        handle_get_request,
    )
    .await
}

pub async fn listen_for_datastore_list(
    messenger: &MessengerHandle,
    core_node_name: &str,
    instance_id: &str,
    node_name: &str,
    store: Arc<Datastore>,
) -> Result<JoinHandle<Result<()>>> {
    spawn_datastore_listener(
        messenger,
        core_node_name,
        instance_id,
        node_name,
        ServiceId::DatastoreList.name(),
        store,
        handle_list_request,
    )
    .await
}

pub async fn listen_for_datastore_remove(
    messenger: &MessengerHandle,
    core_node_name: &str,
    instance_id: &str,
    node_name: &str,
    store: Arc<Datastore>,
) -> Result<JoinHandle<Result<()>>> {
    spawn_datastore_listener(
        messenger,
        core_node_name,
        instance_id,
        node_name,
        ServiceId::DatastoreRemove.name(),
        store,
        handle_remove_request,
    )
    .await
}

fn handle_store_request(store: &Datastore, context: &ServiceRequestContext) -> Result<Payload> {
    let request = DatastoreStoreRequest::decode(context.message().payload().as_ref())?;
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
    );

    DatastoreStoreResponse::new().encode().map_err(Into::into)
}

fn handle_get_request(store: &Datastore, context: &ServiceRequestContext) -> Result<Payload> {
    let request = DatastoreGetRequest::decode(context.message().payload().as_ref())?;

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
    DatastoreListRequest::decode(context.message().payload().as_ref())?;

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
    let request = DatastoreRemoveRequest::decode(context.message().payload().as_ref())?;

    let removed = store.remove(request.key.as_str());

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn store_then_get_returns_value_encoding_and_writer() {
        let store = Datastore::new();
        store.store(
            "k".to_string(),
            b"hello".to_vec(),
            "text/plain".to_string(),
            "writer_node".to_string(),
        );

        let got = store.get("k").expect("key present after store");
        assert_eq!(got.value, b"hello");
        assert_eq!(got.encoding, "text/plain");
        assert_eq!(got.last_modified_by, "writer_node");
    }

    #[test]
    fn get_missing_key_returns_none() {
        let store = Datastore::new();
        assert!(store.get("absent").is_none());
    }

    #[test]
    fn store_overwrites_existing_key_and_updates_writer() {
        let store = Datastore::new();
        store.store(
            "k".to_string(),
            b"first".to_vec(),
            "text/plain".to_string(),
            "first_writer".to_string(),
        );
        store.store(
            "k".to_string(),
            b"second".to_vec(),
            "application/json".to_string(),
            "second_writer".to_string(),
        );

        let got = store.get("k").expect("key present");
        assert_eq!(got.value, b"second");
        assert_eq!(got.encoding, "application/json");
        assert_eq!(got.last_modified_by, "second_writer", "later writer wins");
    }

    #[test]
    fn stores_and_returns_empty_value() {
        let store = Datastore::new();
        store.store(
            "empty".to_string(),
            Vec::new(),
            String::new(),
            "w".to_string(),
        );

        let got = store.get("empty").expect("key present");
        assert!(got.value.is_empty());
        assert!(got.encoding.is_empty());
    }

    #[test]
    fn stores_arbitrary_binary_value() {
        let store = Datastore::new();
        let value = vec![0u8, 255, 0x80, 0xFE];
        store.store(
            "blob".to_string(),
            value.clone(),
            "application/octet-stream".to_string(),
            "w".to_string(),
        );

        let got = store.get("blob").expect("key present");
        assert_eq!(got.value, value);
    }

    #[test]
    fn list_returns_metadata_for_all_keys() {
        let store = Datastore::new();
        store.store(
            "a".to_string(),
            b"one".to_vec(),
            "text/plain".to_string(),
            "node_a".to_string(),
        );
        store.store(
            "b".to_string(),
            b"two".to_vec(),
            "application/json".to_string(),
            "node_b".to_string(),
        );

        let mut entries = store.list();
        entries.sort_by(|l, r| l.key.cmp(&r.key));

        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].key, "a");
        assert_eq!(entries[0].encoding, "text/plain");
        assert_eq!(entries[0].last_modified_by, "node_a");
        assert_eq!(entries[1].key, "b");
        assert_eq!(entries[1].encoding, "application/json");
        assert_eq!(entries[1].last_modified_by, "node_b");
    }

    #[test]
    fn list_is_empty_for_fresh_store() {
        let store = Datastore::new();
        assert!(store.list().is_empty());
    }

    #[test]
    fn remove_reports_existence_and_deletes_the_key() {
        let store = Datastore::new();
        store.store(
            "k".to_string(),
            b"v".to_vec(),
            "text/plain".to_string(),
            "w".to_string(),
        );

        assert!(store.remove("k"), "removing an existing key returns true");
        assert!(store.get("k").is_none(), "key is gone after remove");
        assert!(
            !store.remove("k"),
            "removing an already-absent key returns false"
        );
    }
}
