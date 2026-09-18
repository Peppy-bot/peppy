//! High-level wrappers around the `DATASTORE_STORE` / `DATASTORE_GET` services.
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
    DatastoreGetRequest, DatastoreListRequest, DatastoreRemoveRequest, DatastoreStoreRequest,
};

use crate::core_node::transport::poll;
use crate::error::Result;
use crate::runtime::NodeRunner;

const DEFAULT_RESPONSE_TIMEOUT: Duration = Duration::from_secs(10);

/// A Zenoh-style content-type tag describing how a datastore value's bytes
/// should be interpreted (e.g. `text/plain`, `application/json`).
///
/// Like Zenoh's own `Encoding`, this is an **open** set: the associated
/// constants below cover the common cases, but any string is a valid tag —
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

/// Store `value` (arbitrary bytes) under `key`, tagged with `encoding`, on the
/// node's bound core node. Overwrites any existing value for `key`.
///
/// `key` must use the node-name character set (ASCII letters, digits, `_` and
/// `-`); an invalid key returns an error before any request is sent.
/// `encoding` accepts any string or one of the [`Encoding`] constants (e.g.
/// [`Encoding::APPLICATION_JSON`]).
pub async fn store(
    node_runner: &NodeRunner,
    key: impl Into<String>,
    value: impl Into<Vec<u8>>,
    encoding: impl Into<Encoding>,
    response_timeout: impl Into<Option<Duration>> + Send,
) -> Result<()> {
    let timeout = response_timeout.into().unwrap_or(DEFAULT_RESPONSE_TIMEOUT);
    let encoding: Encoding = encoding.into();
    let request = DatastoreStoreRequest::new(key, value, encoding)?;
    let processor = node_runner.processor();
    let core_node = processor.bound_core_node();

    poll(
        &request,
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
///
/// `key` must use the node-name character set (ASCII letters, digits, `_` and
/// `-`); an invalid key returns an error before any request is sent.
pub async fn get(
    node_runner: &NodeRunner,
    key: impl Into<String>,
    response_timeout: impl Into<Option<Duration>> + Send,
) -> Result<Option<StoredValue>> {
    let timeout = response_timeout.into().unwrap_or(DEFAULT_RESPONSE_TIMEOUT);
    let request = DatastoreGetRequest::new(key)?;
    let processor = node_runner.processor();
    let core_node = processor.bound_core_node();

    let response = poll(
        &request,
        node_runner.messenger(),
        core_node,
        processor.bound_instance_id(),
        core_node,
        timeout,
    )
    .await?;

    Ok(response.found.then_some(StoredValue {
        value: response.value,
        encoding: response.encoding.into(),
        last_modified_by: response.last_modified_by,
    }))
}

/// List the metadata of every key currently in the datastore on the node's
/// bound core node. Each [`DatastoreEntry`] carries the key, its encoding tag,
/// and the `instance_id` of the node that last wrote it — but **not** the value
/// bytes; fetch those with [`get`]. Order is unspecified.
pub async fn list(
    node_runner: &NodeRunner,
    response_timeout: impl Into<Option<Duration>> + Send,
) -> Result<Vec<DatastoreEntry>> {
    let timeout = response_timeout.into().unwrap_or(DEFAULT_RESPONSE_TIMEOUT);
    let processor = node_runner.processor();
    let core_node = processor.bound_core_node();

    let response = poll(
        &DatastoreListRequest::new(),
        node_runner.messenger(),
        core_node,
        processor.bound_instance_id(),
        core_node,
        timeout,
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
///
/// `key` must use the node-name character set (ASCII letters, digits, `_` and
/// `-`); an invalid key returns an error before any request is sent.
pub async fn remove(
    node_runner: &NodeRunner,
    key: impl Into<String>,
    response_timeout: impl Into<Option<Duration>> + Send,
) -> Result<bool> {
    let timeout = response_timeout.into().unwrap_or(DEFAULT_RESPONSE_TIMEOUT);
    let request = DatastoreRemoveRequest::new(key)?;
    let processor = node_runner.processor();
    let core_node = processor.bound_core_node();

    let response = poll(
        &request,
        node_runner.messenger(),
        core_node,
        processor.bound_instance_id(),
        core_node,
        timeout,
    )
    .await?;

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
