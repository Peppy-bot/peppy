//! High-level wrappers around the core node's datastore services: `store`,
//! `store_exclusive`, `get`, `list` and `remove`.
//!
//! Unlike a raw [`crate::core_node::transport::poll`], which returns the wire
//! response and requires the caller to thread routing parameters through by
//! hand, this layer takes a [`NodeRunner`] directly. The get wrapper also
//! folds the response's `found` flag into an `Option`, so a missing key reads
//! as `None` rather than a struct with an empty value.

use std::borrow::Cow;
use std::fmt;
use std::time::Duration;

use core_node_api::encoding::{
    DatastoreGetRequest, DatastoreListRequest, DatastoreRemoveRequest,
    DatastoreStoreExclusiveRequest, DatastoreStoreExclusiveResponse, DatastoreStoreRequest,
};

use crate::core_node::poll_bound_core_node;
use crate::error::Result;
use crate::runtime::NodeRunner;

/// A Zenoh-style content-type tag describing how a datastore value's bytes
/// should be interpreted (e.g. `text/plain`, `application/json`).
///
/// Like Zenoh's own `Encoding`, this is an **open** set: the associated
/// constants below cover the common cases, but any string is a valid tag:
/// build one with `Encoding::from("application/cbor")`. The datastore treats
/// the tag as an opaque label and never interprets it; it exists so a value's
/// content type round-trips faithfully alongside its bytes.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Encoding(Cow<'static, str>);

impl Encoding {
    /// UTF-8 text with no further structure (`text/plain`).
    pub const TEXT_PLAIN: Self = Encoding(Cow::Borrowed("text/plain"));
    /// A JSON document (`application/json`).
    pub const APPLICATION_JSON: Self = Encoding(Cow::Borrowed("application/json"));
    /// Opaque binary data (`application/octet-stream`).
    pub const APPLICATION_OCTET_STREAM: Self = Encoding(Cow::Borrowed("application/octet-stream"));

    /// Borrow the tag as a string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<String> for Encoding {
    fn from(tag: String) -> Self {
        Encoding(Cow::Owned(tag))
    }
}

impl From<&str> for Encoding {
    fn from(tag: &str) -> Self {
        Encoding(Cow::Owned(tag.to_owned()))
    }
}

impl From<Encoding> for String {
    fn from(encoding: Encoding) -> Self {
        encoding.0.into_owned()
    }
}

impl AsRef<str> for Encoding {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Encoding {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl PartialEq<str> for Encoding {
    fn eq(&self, other: &str) -> bool {
        self.as_str() == other
    }
}

impl PartialEq<&str> for Encoding {
    fn eq(&self, other: &&str) -> bool {
        self.as_str() == *other
    }
}

/// A value retrieved from the datastore: the raw bytes, the Zenoh-style
/// [`Encoding`] tag they were stored with, and the `instance_id` of the node
/// that last wrote the key. Mirrors Zenoh's `(payload, encoding)` value model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredValue {
    pub value: Vec<u8>,
    pub encoding: Encoding,
    /// `instance_id` of the node that last wrote this key.
    pub last_modified_by: String,
}

impl StoredValue {
    fn new(value: Vec<u8>, encoding: String, last_modified_by: String) -> Self {
        Self {
            value,
            encoding: encoding.into(),
            last_modified_by,
        }
    }
}

/// One key's metadata as returned by [`list`]: its key, the [`Encoding`] tag
/// of its value, and the `instance_id` of the node that last wrote it. The
/// value bytes are not included; fetch them with [`get`] when you need them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DatastoreEntry {
    pub key: String,
    pub encoding: Encoding,
    /// `instance_id` of the node that last wrote this key.
    pub last_modified_by: String,
}

/// The answer to [`store_exclusive`].
#[must_use = "a refused exclusive store leaves another instance the key's owner"]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExclusiveStore {
    /// The calling instance owns the key, which holds the value it stored.
    Stored,
    /// Another instance owns the key: the key as its owner stored it, the
    /// owner's `instance_id` in `last_modified_by`.
    Refused { current: StoredValue },
}

/// Store `value` (arbitrary bytes) under `key`, tagged with `encoding`, on the
/// node's bound core node. Overwrites any existing value for `key`. Fails when
/// another instance owns `key` (see [`store_exclusive`]); the owner's store
/// keeps the key its own.
///
/// `key` is any non-empty string; an empty key returns an error before any
/// request is sent.
/// `encoding` accepts any string or one of the [`Encoding`] constants (e.g.
/// [`Encoding::APPLICATION_JSON`]).
pub async fn store(
    node_runner: &NodeRunner,
    key: impl Into<String>,
    value: impl Into<Vec<u8>>,
    encoding: impl Into<Encoding>,
    response_timeout: impl Into<Option<Duration>> + Send,
) -> Result<()> {
    let encoding: Encoding = encoding.into();
    let request = DatastoreStoreRequest::new(key, value, encoding)?;
    poll_bound_core_node(node_runner, &request, response_timeout.into()).await?;

    Ok(())
}

/// Store `value` under `key`, tagged with `encoding`, and make the calling
/// instance the key's owner: every other instance's [`store`] and [`remove`] of
/// it fail, another instance of the same node included, until the owner
/// removes it or its process ends, which removes the key.
/// When another instance owns `key`, nothing is written and the answer is
/// [`ExclusiveStore::Refused`] with the key as its owner stored it.
///
/// Fails when `key` exists with no owner, as [`store`] writes it, and when the
/// core node runs no process under the calling instance's id.
pub async fn store_exclusive(
    node_runner: &NodeRunner,
    key: impl Into<String>,
    value: impl Into<Vec<u8>>,
    encoding: impl Into<Encoding>,
    response_timeout: impl Into<Option<Duration>> + Send,
) -> Result<ExclusiveStore> {
    let encoding: Encoding = encoding.into();
    let request = DatastoreStoreExclusiveRequest::new(key, value, encoding)?;
    let response = poll_bound_core_node(node_runner, &request, response_timeout.into()).await?;

    Ok(match response {
        DatastoreStoreExclusiveResponse::Stored => ExclusiveStore::Stored,
        DatastoreStoreExclusiveResponse::Refused {
            value,
            encoding,
            last_modified_by,
        } => ExclusiveStore::Refused {
            current: StoredValue::new(value, encoding, last_modified_by),
        },
    })
}

/// Retrieve the value stored under `key` from the node's bound core node.
/// Returns `Ok(None)` when no value is stored for `key`.
///
/// `key` is any non-empty string; an empty key returns an error before any
/// request is sent.
pub async fn get(
    node_runner: &NodeRunner,
    key: impl Into<String>,
    response_timeout: impl Into<Option<Duration>> + Send,
) -> Result<Option<StoredValue>> {
    let request = DatastoreGetRequest::new(key)?;
    let response = poll_bound_core_node(node_runner, &request, response_timeout.into()).await?;

    Ok(response
        .found
        .then(|| StoredValue::new(response.value, response.encoding, response.last_modified_by)))
}

/// List the metadata of every key currently in the datastore on the node's
/// bound core node. Each [`DatastoreEntry`] carries the key, its encoding tag,
/// and the `instance_id` of the node that last wrote it, but **not** the value
/// bytes; fetch those with [`get`]. Entries come in key order.
pub async fn list(
    node_runner: &NodeRunner,
    response_timeout: impl Into<Option<Duration>> + Send,
) -> Result<Vec<DatastoreEntry>> {
    let response = poll_bound_core_node(
        node_runner,
        &DatastoreListRequest::new(),
        response_timeout.into(),
    )
    .await?;

    Ok(response
        .entries
        .into_iter()
        .map(|entry| DatastoreEntry {
            key: entry.key,
            encoding: entry.encoding.into(),
            last_modified_by: entry.last_modified_by,
        })
        .collect())
}

/// Remove (unset) `key` from the node's bound core node. Returns `Ok(true)` if
/// the key existed and was removed, `Ok(false)` if it was already absent.
/// Fails when another instance owns `key` (see [`store_exclusive`]).
///
/// `key` is any non-empty string; an empty key returns an error before any
/// request is sent.
pub async fn remove(
    node_runner: &NodeRunner,
    key: impl Into<String>,
    response_timeout: impl Into<Option<Duration>> + Send,
) -> Result<bool> {
    let request = DatastoreRemoveRequest::new(key)?;
    let response = poll_bound_core_node(node_runner, &request, response_timeout.into()).await?;

    Ok(response.removed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn well_known_constants_carry_their_mime_tags() {
        assert_eq!(Encoding::TEXT_PLAIN.as_str(), "text/plain");
        assert_eq!(Encoding::APPLICATION_JSON.as_str(), "application/json");
        assert_eq!(
            Encoding::APPLICATION_OCTET_STREAM.as_str(),
            "application/octet-stream"
        );
    }

    #[test]
    fn arbitrary_tags_round_trip_through_string() {
        // The set is open: a custom tag is just as valid as a constant.
        let custom = Encoding::from("application/cbor");
        assert_eq!(custom.as_str(), "application/cbor");
        assert_eq!(String::from(custom), "application/cbor");
    }

    #[test]
    fn constructs_from_owned_and_borrowed_strings() {
        assert_eq!(Encoding::from("text/plain"), Encoding::TEXT_PLAIN);
        assert_eq!(
            Encoding::from("text/plain".to_string()),
            Encoding::TEXT_PLAIN
        );
    }

    #[test]
    fn compares_against_string_slices() {
        assert_eq!(Encoding::APPLICATION_JSON, "application/json");
        assert_ne!(Encoding::APPLICATION_JSON, "text/plain");
    }
}
