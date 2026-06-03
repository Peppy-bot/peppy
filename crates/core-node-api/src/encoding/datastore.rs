//! Cap'n Proto encoding utilities for datastore messages.
//!
//! Keys are arbitrary strings (any character) and values are arbitrary bytes
//! carried in a Cap'n Proto `Data` field, paired with a Zenoh-style encoding
//! tag — so any value type accepted by Zenoh round-trips faithfully.

use capnp::message::Builder;

use crate::datastore_capnp;
use crate::{Payload, Result};

use super::{capnp_list_len, decode_message, encode_message};

/// Store a `value` (arbitrary bytes) under `key`, tagged with `encoding`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DatastoreStoreRequest {
    pub key: String,
    pub value: Vec<u8>,
    pub encoding: String,
}

impl DatastoreStoreRequest {
    pub fn new(
        key: impl Into<String>,
        value: impl Into<Vec<u8>>,
        encoding: impl Into<String>,
    ) -> Self {
        Self {
            key: key.into(),
            value: value.into(),
            encoding: encoding.into(),
        }
    }

    pub fn encode(&self) -> Result<Payload> {
        let mut builder = Builder::new_default();
        {
            let mut request =
                builder.init_root::<datastore_capnp::datastore_store_request::Builder>();
            request.set_key(&self.key);
            request.set_value(&self.value);
            request.set_encoding(&self.encoding);
        }
        encode_message(&builder)
    }

    pub fn decode(data: &[u8]) -> Result<Self> {
        let reader = decode_message(data)?;
        let request = reader.get_root::<datastore_capnp::datastore_store_request::Reader>()?;
        Ok(Self {
            key: request.get_key()?.to_str()?.to_owned(),
            value: request.get_value()?.to_vec(),
            encoding: request.get_encoding()?.to_str()?.to_owned(),
        })
    }
}

/// Acknowledges a successful store. Carries no fields.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DatastoreStoreResponse;

impl DatastoreStoreResponse {
    pub fn new() -> Self {
        Self
    }

    pub fn encode(&self) -> Result<Payload> {
        let mut builder = Builder::new_default();
        {
            builder.init_root::<datastore_capnp::datastore_store_response::Builder>();
        }
        encode_message(&builder)
    }

    pub fn decode(data: &[u8]) -> Result<Self> {
        let reader = decode_message(data)?;
        reader.get_root::<datastore_capnp::datastore_store_response::Reader>()?;
        Ok(Self)
    }
}

/// Look up the value stored under `key`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DatastoreGetRequest {
    pub key: String,
}

impl DatastoreGetRequest {
    pub fn new(key: impl Into<String>) -> Self {
        Self { key: key.into() }
    }

    pub fn encode(&self) -> Result<Payload> {
        let mut builder = Builder::new_default();
        {
            let mut request =
                builder.init_root::<datastore_capnp::datastore_get_request::Builder>();
            request.set_key(&self.key);
        }
        encode_message(&builder)
    }

    pub fn decode(data: &[u8]) -> Result<Self> {
        let reader = decode_message(data)?;
        let request = reader.get_root::<datastore_capnp::datastore_get_request::Reader>()?;
        Ok(Self {
            key: request.get_key()?.to_str()?.to_owned(),
        })
    }
}

/// Result of a get. When `found` is false, `value`, `encoding` and
/// `last_modified_by` are empty.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DatastoreGetResponse {
    pub found: bool,
    pub value: Vec<u8>,
    pub encoding: String,
    /// `instance_id` of the node that last wrote this key (empty when not found).
    pub last_modified_by: String,
}

impl DatastoreGetResponse {
    pub fn found(
        value: impl Into<Vec<u8>>,
        encoding: impl Into<String>,
        last_modified_by: impl Into<String>,
    ) -> Self {
        Self {
            found: true,
            value: value.into(),
            encoding: encoding.into(),
            last_modified_by: last_modified_by.into(),
        }
    }

    pub fn not_found() -> Self {
        Self {
            found: false,
            value: Vec::new(),
            encoding: String::new(),
            last_modified_by: String::new(),
        }
    }

    pub fn encode(&self) -> Result<Payload> {
        let mut builder = Builder::new_default();
        {
            let mut response =
                builder.init_root::<datastore_capnp::datastore_get_response::Builder>();
            response.set_found(self.found);
            response.set_value(&self.value);
            response.set_encoding(&self.encoding);
            response.set_last_modified_by(&self.last_modified_by);
        }
        encode_message(&builder)
    }

    pub fn decode(data: &[u8]) -> Result<Self> {
        let reader = decode_message(data)?;
        let response = reader.get_root::<datastore_capnp::datastore_get_response::Reader>()?;
        Ok(Self {
            found: response.get_found(),
            value: response.get_value()?.to_vec(),
            encoding: response.get_encoding()?.to_str()?.to_owned(),
            last_modified_by: response.get_last_modified_by()?.to_str()?.to_owned(),
        })
    }
}

/// List every key currently in the store. Carries no fields — the whole
/// keyspace is returned.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DatastoreListRequest;

impl DatastoreListRequest {
    pub fn new() -> Self {
        Self
    }

    pub fn encode(&self) -> Result<Payload> {
        let mut builder = Builder::new_default();
        {
            builder.init_root::<datastore_capnp::datastore_list_request::Builder>();
        }
        encode_message(&builder)
    }

    pub fn decode(data: &[u8]) -> Result<Self> {
        let reader = decode_message(data)?;
        reader.get_root::<datastore_capnp::datastore_list_request::Reader>()?;
        Ok(Self)
    }
}

/// A single key's metadata in a [`DatastoreListResponse`]. The value bytes are
/// intentionally omitted — a list stays cheap no matter how large the stored
/// values are; fetch the bytes with a [`DatastoreGetRequest`] when needed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DatastoreListEntry {
    pub key: String,
    pub encoding: String,
    /// `instance_id` of the node that last wrote this key.
    pub last_modified_by: String,
}

/// The metadata of every key currently in the store.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DatastoreListResponse {
    pub entries: Vec<DatastoreListEntry>,
}

impl DatastoreListResponse {
    pub fn new(entries: Vec<DatastoreListEntry>) -> Self {
        Self { entries }
    }

    pub fn encode(&self) -> Result<Payload> {
        let mut builder = Builder::new_default();
        {
            let mut response =
                builder.init_root::<datastore_capnp::datastore_list_response::Builder>();
            let entry_count = capnp_list_len(self.entries.len(), "DatastoreListResponse.entries")?;
            let mut entries = response.reborrow().init_entries(entry_count);
            for (i, entry) in self.entries.iter().enumerate() {
                let mut e = entries.reborrow().get(i as u32);
                e.set_key(&entry.key);
                e.set_encoding(&entry.encoding);
                e.set_last_modified_by(&entry.last_modified_by);
            }
        }
        encode_message(&builder)
    }

    pub fn decode(data: &[u8]) -> Result<Self> {
        let reader = decode_message(data)?;
        let response = reader.get_root::<datastore_capnp::datastore_list_response::Reader>()?;
        let entries_reader = response.get_entries()?;
        let mut entries = Vec::with_capacity(entries_reader.len() as usize);
        for i in 0..entries_reader.len() {
            let e = entries_reader.get(i);
            entries.push(DatastoreListEntry {
                key: e.get_key()?.to_str()?.to_owned(),
                encoding: e.get_encoding()?.to_str()?.to_owned(),
                last_modified_by: e.get_last_modified_by()?.to_str()?.to_owned(),
            });
        }
        Ok(Self { entries })
    }
}

/// Remove (unset) a single key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DatastoreRemoveRequest {
    pub key: String,
}

impl DatastoreRemoveRequest {
    pub fn new(key: impl Into<String>) -> Self {
        Self { key: key.into() }
    }

    pub fn encode(&self) -> Result<Payload> {
        let mut builder = Builder::new_default();
        {
            let mut request =
                builder.init_root::<datastore_capnp::datastore_remove_request::Builder>();
            request.set_key(&self.key);
        }
        encode_message(&builder)
    }

    pub fn decode(data: &[u8]) -> Result<Self> {
        let reader = decode_message(data)?;
        let request = reader.get_root::<datastore_capnp::datastore_remove_request::Reader>()?;
        Ok(Self {
            key: request.get_key()?.to_str()?.to_owned(),
        })
    }
}

/// Result of a remove: whether the key existed before it was removed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DatastoreRemoveResponse {
    pub removed: bool,
}

impl DatastoreRemoveResponse {
    pub fn new(removed: bool) -> Self {
        Self { removed }
    }

    pub fn encode(&self) -> Result<Payload> {
        let mut builder = Builder::new_default();
        {
            let mut response =
                builder.init_root::<datastore_capnp::datastore_remove_response::Builder>();
            response.set_removed(self.removed);
        }
        encode_message(&builder)
    }

    pub fn decode(data: &[u8]) -> Result<Self> {
        let reader = decode_message(data)?;
        let response = reader.get_root::<datastore_capnp::datastore_remove_response::Reader>()?;
        Ok(Self {
            removed: response.get_removed(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn store_request_round_trips_text_value() {
        let request = DatastoreStoreRequest::new("greeting", b"hello".to_vec(), "text/plain");
        let payload = request.encode().expect("encode");
        let decoded = DatastoreStoreRequest::decode(payload.as_ref()).expect("decode");
        assert_eq!(decoded, request);
    }

    #[test]
    fn store_request_round_trips_binary_value() {
        // Non-UTF-8 bytes prove the value rides in a `Data` field, not `Text`.
        let value = vec![0u8, 255, 0x80, 0xFE, 0x00, 0x01];
        let request = DatastoreStoreRequest::new("blob", value.clone(), "application/octet-stream");
        let payload = request.encode().expect("encode");
        let decoded = DatastoreStoreRequest::decode(payload.as_ref()).expect("decode");
        assert_eq!(decoded.value, value);
        assert_eq!(decoded, request);
    }

    #[test]
    fn store_request_round_trips_empty_value() {
        let request = DatastoreStoreRequest::new("empty", Vec::new(), "");
        let payload = request.encode().expect("encode");
        let decoded = DatastoreStoreRequest::decode(payload.as_ref()).expect("decode");
        assert!(decoded.value.is_empty());
        assert_eq!(decoded, request);
    }

    #[test]
    fn store_response_round_trips() {
        let payload = DatastoreStoreResponse::new().encode().expect("encode");
        DatastoreStoreResponse::decode(payload.as_ref()).expect("decode");
    }

    #[test]
    fn get_request_round_trips_special_character_key() {
        // Keys travel inside the payload, not a Zenoh keyexpr, so wildcards,
        // slashes, whitespace and unicode are all valid.
        let key = "a/b**c?{x} y\nz/日本語/*";
        let request = DatastoreGetRequest::new(key);
        let payload = request.encode().expect("encode");
        let decoded = DatastoreGetRequest::decode(payload.as_ref()).expect("decode");
        assert_eq!(decoded.key, key);
    }

    #[test]
    fn get_response_round_trips_found() {
        let value = vec![0u8, 1, 2, 250, 255];
        let response =
            DatastoreGetResponse::found(value.clone(), "application/octet-stream", "writer_node");
        let payload = response.encode().expect("encode");
        let decoded = DatastoreGetResponse::decode(payload.as_ref()).expect("decode");
        assert_eq!(decoded, response);
        assert!(decoded.found);
        assert_eq!(decoded.value, value);
        assert_eq!(decoded.last_modified_by, "writer_node");
    }

    #[test]
    fn get_response_round_trips_not_found() {
        let response = DatastoreGetResponse::not_found();
        let payload = response.encode().expect("encode");
        let decoded = DatastoreGetResponse::decode(payload.as_ref()).expect("decode");
        assert!(!decoded.found);
        assert!(decoded.value.is_empty());
        assert!(decoded.encoding.is_empty());
        assert!(decoded.last_modified_by.is_empty());
    }

    #[test]
    fn list_request_round_trips() {
        let payload = DatastoreListRequest::new().encode().expect("encode");
        DatastoreListRequest::decode(payload.as_ref()).expect("decode");
    }

    #[test]
    fn list_response_round_trips_empty() {
        let response = DatastoreListResponse::default();
        let payload = response.encode().expect("encode");
        let decoded = DatastoreListResponse::decode(payload.as_ref()).expect("decode");
        assert!(decoded.entries.is_empty());
    }

    #[test]
    fn list_response_round_trips_multiple_entries() {
        let response = DatastoreListResponse::new(vec![
            DatastoreListEntry {
                key: "a/b**{1}".to_owned(),
                encoding: "text/plain".to_owned(),
                last_modified_by: "node_one".to_owned(),
            },
            DatastoreListEntry {
                key: "mode".to_owned(),
                encoding: "application/json".to_owned(),
                last_modified_by: "node_two".to_owned(),
            },
        ]);
        let payload = response.encode().expect("encode");
        let decoded = DatastoreListResponse::decode(payload.as_ref()).expect("decode");
        assert_eq!(decoded, response);
    }

    #[test]
    fn remove_request_round_trips_special_character_key() {
        let key = "a/b**c?{x} y\nz/日本語/*";
        let request = DatastoreRemoveRequest::new(key);
        let payload = request.encode().expect("encode");
        let decoded = DatastoreRemoveRequest::decode(payload.as_ref()).expect("decode");
        assert_eq!(decoded.key, key);
    }

    #[test]
    fn remove_response_round_trips() {
        for removed in [true, false] {
            let response = DatastoreRemoveResponse::new(removed);
            let payload = response.encode().expect("encode");
            let decoded = DatastoreRemoveResponse::decode(payload.as_ref()).expect("decode");
            assert_eq!(decoded.removed, removed);
        }
    }
}
