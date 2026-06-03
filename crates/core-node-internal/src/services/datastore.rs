use crate::Result;
use crate::names;
use core_node_api::encoding::{
    DatastoreGetRequest, DatastoreGetResponse, DatastoreStoreRequest, DatastoreStoreResponse,
};
use parking_lot::RwLock;
use peppylib::messaging::{SenderTarget, ServiceRequestContext};
use peppylib::types::Payload;
use peppylib::{MessengerHandle, PeppyError, PeppyResult, ServiceMessenger};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::task::JoinHandle;
use tracing::debug;

/// A stored value: the raw bytes plus the Zenoh-style encoding tag the caller
/// supplied. Mirrors Zenoh's `(payload, encoding)` value model so any value
/// type round-trips faithfully.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredValue {
    pub value: Vec<u8>,
    pub encoding: String,
}

/// Daemon-internal, in-memory key/value store shared by the `datastore_store`
/// and `datastore_get` service handlers. Keys are arbitrary strings (they
/// ride inside the request payload, never a Zenoh keyexpr), values are
/// arbitrary bytes.
#[derive(Debug, Default)]
pub struct Datastore {
    map: RwLock<HashMap<String, StoredValue>>,
}

impl Datastore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Upserts `key` → `(value, encoding)`. A later store under the same key
    /// overwrites the previous value.
    pub fn store(&self, key: String, value: Vec<u8>, encoding: String) {
        self.map
            .write()
            .insert(key, StoredValue { value, encoding });
    }

    /// Returns the stored value for `key`, or `None` if absent.
    pub fn get(&self, key: &str) -> Option<StoredValue> {
        self.map.read().get(key).cloned()
    }
}

pub async fn listen_for_datastore_store(
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
        names::DATASTORE_STORE,
    )
    .await?;

    let handle = tokio::spawn(async move {
        endpoint
            .handle_requests(move |context| {
                let store = Arc::clone(&store);
                async move { handle_store_request(store.as_ref(), context) }
            })
            .await
            .map_err(Into::into)
    });

    Ok(handle)
}

pub async fn listen_for_datastore_get(
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
        names::DATASTORE_GET,
    )
    .await?;

    let handle = tokio::spawn(async move {
        endpoint
            .handle_requests(move |context| {
                let store = Arc::clone(&store);
                async move { handle_get_request(store.as_ref(), context) }
            })
            .await
            .map_err(Into::into)
    });

    Ok(handle)
}

fn handle_store_request(store: &Datastore, context: ServiceRequestContext) -> PeppyResult<Payload> {
    let instance_id = context.message().instance_id().to_string();
    handle_store_request_inner(store, &context).map_err(|e| PeppyError::InvalidServiceRequest {
        identifier: instance_id,
        reason: e.to_string(),
    })
}

fn handle_store_request_inner(
    store: &Datastore,
    context: &ServiceRequestContext,
) -> Result<Payload> {
    let request = DatastoreStoreRequest::decode(context.message().payload().as_ref())?;

    debug!(
        "Received datastore store request from {} for key {:?} ({} bytes, encoding {:?})",
        context.message().instance_id(),
        request.key,
        request.value.len(),
        request.encoding,
    );

    store.store(request.key, request.value, request.encoding);

    DatastoreStoreResponse::new().encode().map_err(Into::into)
}

fn handle_get_request(store: &Datastore, context: ServiceRequestContext) -> PeppyResult<Payload> {
    let instance_id = context.message().instance_id().to_string();
    handle_get_request_inner(store, &context).map_err(|e| PeppyError::InvalidServiceRequest {
        identifier: instance_id,
        reason: e.to_string(),
    })
}

fn handle_get_request_inner(store: &Datastore, context: &ServiceRequestContext) -> Result<Payload> {
    let request = DatastoreGetRequest::decode(context.message().payload().as_ref())?;

    debug!(
        "Received datastore get request from {} for key {:?}",
        context.message().instance_id(),
        request.key,
    );

    let response = match store.get(&request.key) {
        Some(stored) => DatastoreGetResponse::found(stored.value, stored.encoding),
        None => DatastoreGetResponse::not_found(),
    };

    response.encode().map_err(Into::into)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn store_then_get_returns_value_and_encoding() {
        let store = Datastore::new();
        store.store("k".to_string(), b"hello".to_vec(), "text/plain".to_string());

        let got = store.get("k").expect("key present after store");
        assert_eq!(got.value, b"hello");
        assert_eq!(got.encoding, "text/plain");
    }

    #[test]
    fn get_missing_key_returns_none() {
        let store = Datastore::new();
        assert!(store.get("absent").is_none());
    }

    #[test]
    fn store_overwrites_existing_key() {
        let store = Datastore::new();
        store.store("k".to_string(), b"first".to_vec(), "text/plain".to_string());
        store.store(
            "k".to_string(),
            b"second".to_vec(),
            "application/json".to_string(),
        );

        let got = store.get("k").expect("key present");
        assert_eq!(got.value, b"second");
        assert_eq!(got.encoding, "application/json");
    }

    #[test]
    fn stores_and_returns_empty_value() {
        let store = Datastore::new();
        store.store("empty".to_string(), Vec::new(), String::new());

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
        );

        let got = store.get("blob").expect("key present");
        assert_eq!(got.value, value);
    }
}
