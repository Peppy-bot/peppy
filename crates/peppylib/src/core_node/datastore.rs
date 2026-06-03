//! High-level wrappers around the `DATASTORE_STORE` / `DATASTORE_GET` services.
//!
//! Unlike [`crate::core_node::transport::poll_datastore_store`] /
//! [`poll_datastore_get`](crate::core_node::transport::poll_datastore_get),
//! which return the raw wire response and require the caller to thread routing
//! parameters through by hand, this layer takes a [`NodeRunner`] directly. The
//! get wrapper also folds the response's `found` flag into an `Option`, so a
//! missing key reads as `None` rather than a struct with an empty value.

use std::time::Duration;

use core_node_api::encoding::{DatastoreGetRequest, DatastoreStoreRequest};

use crate::core_node::transport::{poll_datastore_get, poll_datastore_store};
use crate::error::Result;
use crate::runtime::NodeRunner;

const DEFAULT_RESPONSE_TIMEOUT: Duration = Duration::from_secs(10);

/// A value retrieved from the datastore: the raw bytes plus the Zenoh-style
/// encoding tag they were stored with. Mirrors Zenoh's `(payload, encoding)`
/// value model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredValue {
    pub value: Vec<u8>,
    pub encoding: String,
}

/// Store `value` (arbitrary bytes) under `key`, tagged with `encoding`, on the
/// node's bound core node. Overwrites any existing value for `key`.
pub async fn datastore_store(
    node_runner: &NodeRunner,
    key: impl Into<String>,
    value: impl Into<Vec<u8>>,
    encoding: impl Into<String>,
    response_timeout: impl Into<Option<Duration>> + Send,
) -> Result<()> {
    let timeout = response_timeout.into().unwrap_or(DEFAULT_RESPONSE_TIMEOUT);
    let processor = node_runner.processor();
    let core_node = processor.bound_core_node();

    poll_datastore_store(
        &DatastoreStoreRequest::new(key, value, encoding),
        node_runner.messenger(),
        core_node,
        processor.bound_instance_id(),
        core_node,
        timeout,
    )
    .await?;

    Ok(())
}

/// Retrieve the value stored under `key` from the node's bound core node.
/// Returns `Ok(None)` when no value is stored for `key`.
pub async fn datastore_get(
    node_runner: &NodeRunner,
    key: impl Into<String>,
    response_timeout: impl Into<Option<Duration>> + Send,
) -> Result<Option<StoredValue>> {
    let timeout = response_timeout.into().unwrap_or(DEFAULT_RESPONSE_TIMEOUT);
    let processor = node_runner.processor();
    let core_node = processor.bound_core_node();

    let response = poll_datastore_get(
        &DatastoreGetRequest::new(key),
        node_runner.messenger(),
        core_node,
        processor.bound_instance_id(),
        core_node,
        timeout,
    )
    .await?;

    Ok(response.found.then_some(StoredValue {
        value: response.value,
        encoding: response.encoding,
    }))
}
