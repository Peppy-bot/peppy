//! Cap'n Proto encoding utilities for datastore messages.
//!
//! Keys are arbitrary, non-empty strings (see [`DatastoreKey`]). They are
//! carried in the payload (not a Zenoh keyexpr), so any character is allowed:
//! slashes, dots, spaces and Unicode all round-trip. Values are arbitrary bytes
//! carried in a Cap'n Proto `Data` field, paired with a Zenoh-style encoding
//! tag, so any value type accepted by Zenoh round-trips faithfully.

use std::fmt;

use capnp::message::Builder;

use crate::datastore_capnp;
use crate::{Payload, Result};

use super::{capnp_list_len, decode_message, encode_message};

/// A datastore key: any non-empty string.
///
/// The datastore stores and retrieves values by exact key. Keys are carried in
/// the message payload (not a Zenoh keyexpr), so any character is allowed:
/// slashes, dots, spaces and Unicode all round-trip. The only requirement is
/// that the key is non-empty.
///
/// Construct one with [`DatastoreKey::try_from`]; an empty key yields a
/// [`DatastoreKeyError`]. Following the "parse, don't validate" pattern, once a
/// `DatastoreKey` exists it is guaranteed non-empty, so request types carry it
/// directly rather than a raw `String`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct DatastoreKey(String);

impl DatastoreKey {
    /// Borrow the key as a string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Consume the key, returning the owned validated string.
    pub fn into_string(self) -> String {
        self.0
    }
}

impl TryFrom<String> for DatastoreKey {
    type Error = DatastoreKeyError;

    fn try_from(key: String) -> core::result::Result<Self, Self::Error> {
        if key.is_empty() {
            return Err(DatastoreKeyError::Empty);
        }
        Ok(Self(key))
    }
}

impl TryFrom<&str> for DatastoreKey {
    type Error = DatastoreKeyError;

    fn try_from(key: &str) -> core::result::Result<Self, Self::Error> {
        Self::try_from(key.to_owned())
    }
}

impl From<DatastoreKey> for String {
    fn from(key: DatastoreKey) -> Self {
        key.0
    }
}

impl AsRef<str> for DatastoreKey {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for DatastoreKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Why a string was rejected as a [`DatastoreKey`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DatastoreKeyError {
    /// The key was the empty string.
    #[error("datastore key must not be empty")]
    Empty,
}

/// Store a `value` (arbitrary bytes) under `key`, tagged with `encoding`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DatastoreStoreRequest {
    pub key: DatastoreKey,
    pub value: Vec<u8>,
    pub encoding: String,
}

impl DatastoreStoreRequest {
    /// Builds a store request, validating `key` as a [`DatastoreKey`]. Returns
    /// [`Error::InvalidDatastoreKey`](crate::Error::InvalidDatastoreKey) if the
    /// key is empty.
    pub fn new(
        key: impl Into<String>,
        value: impl Into<Vec<u8>>,
        encoding: impl Into<String>,
    ) -> Result<Self> {
        Ok(Self {
            key: DatastoreKey::try_from(key.into())?,
            value: value.into(),
            encoding: encoding.into(),
        })
    }

    pub fn encode(&self) -> Result<Payload> {
        let mut builder = Builder::new_default();
        {
            let mut request =
                builder.init_root::<datastore_capnp::datastore_store_request::Builder>();
            request.set_key(self.key.as_str());
            request.set_value(&self.value);
            request.set_encoding(&self.encoding);
        }
        encode_message(&builder)
    }

    pub fn decode(data: &[u8]) -> Result<Self> {
        let reader = decode_message(data)?;
        let request = reader.get_root::<datastore_capnp::datastore_store_request::Reader>()?;
        Ok(Self {
            key: DatastoreKey::try_from(request.get_key()?.to_str()?)?,
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
    pub key: DatastoreKey,
}

impl DatastoreGetRequest {
    /// Builds a get request, validating `key` as a [`DatastoreKey`]. Returns
    /// [`Error::InvalidDatastoreKey`](crate::Error::InvalidDatastoreKey) if the
    /// key is empty.
    pub fn new(key: impl Into<String>) -> Result<Self> {
        Ok(Self {
            key: DatastoreKey::try_from(key.into())?,
        })
    }

    pub fn encode(&self) -> Result<Payload> {
        let mut builder = Builder::new_default();
        {
            let mut request =
                builder.init_root::<datastore_capnp::datastore_get_request::Builder>();
            request.set_key(self.key.as_str());
        }
        encode_message(&builder)
    }

    pub fn decode(data: &[u8]) -> Result<Self> {
        let reader = decode_message(data)?;
        let request = reader.get_root::<datastore_capnp::datastore_get_request::Reader>()?;
        Ok(Self {
            key: DatastoreKey::try_from(request.get_key()?.to_str()?)?,
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

/// List every key currently in the store. Carries no fields: the whole
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
/// intentionally omitted so a list stays cheap no matter how large the stored
/// values are; fetch the bytes with a [`DatastoreGetRequest`] when needed.
///
/// The key here echoes one already validated and stored by the daemon, so it
/// is carried as a plain `String` and not re-validated on the way back out.
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
    pub key: DatastoreKey,
}

impl DatastoreRemoveRequest {
    /// Builds a remove request, validating `key` as a [`DatastoreKey`]. Returns
    /// [`Error::InvalidDatastoreKey`](crate::Error::InvalidDatastoreKey) if the
    /// key is empty.
    pub fn new(key: impl Into<String>) -> Result<Self> {
        Ok(Self {
            key: DatastoreKey::try_from(key.into())?,
        })
    }

    pub fn encode(&self) -> Result<Payload> {
        let mut builder = Builder::new_default();
        {
            let mut request =
                builder.init_root::<datastore_capnp::datastore_remove_request::Builder>();
            request.set_key(self.key.as_str());
        }
        encode_message(&builder)
    }

    pub fn decode(data: &[u8]) -> Result<Self> {
        let reader = decode_message(data)?;
        let request = reader.get_root::<datastore_capnp::datastore_remove_request::Reader>()?;
        Ok(Self {
            key: DatastoreKey::try_from(request.get_key()?.to_str()?)?,
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

/// Store `value` under `key`, tagged with `encoding`, and make the calling
/// instance the key's only writer.
///
/// It carries a [`DatastoreStoreRequest`] and encodes as one: its own service,
/// `datastore_store_exclusive`, is what makes the caller the owner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DatastoreStoreExclusiveRequest(DatastoreStoreRequest);

impl DatastoreStoreExclusiveRequest {
    /// Builds an exclusive store request, validating `key` as a
    /// [`DatastoreKey`]. Returns
    /// [`Error::InvalidDatastoreKey`](crate::Error::InvalidDatastoreKey) if the
    /// key is empty.
    pub fn new(
        key: impl Into<String>,
        value: impl Into<Vec<u8>>,
        encoding: impl Into<String>,
    ) -> Result<Self> {
        DatastoreStoreRequest::new(key, value, encoding).map(Self)
    }

    /// The store this request makes exclusive.
    pub fn into_store(self) -> DatastoreStoreRequest {
        self.0
    }

    pub fn encode(&self) -> Result<Payload> {
        self.0.encode()
    }

    pub fn decode(data: &[u8]) -> Result<Self> {
        DatastoreStoreRequest::decode(data).map(Self)
    }
}

/// Result of an exclusive store. A refusal rides in the response as `Refused`,
/// carrying the key as its owner wrote it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DatastoreStoreExclusiveResponse {
    /// The caller owns the key, which holds the value it sent.
    Stored,
    /// Another instance owns the key: the key as its owner stored it.
    Refused {
        value: Vec<u8>,
        encoding: String,
        /// `instance_id` of the instance that owns the key.
        last_modified_by: String,
    },
}

impl DatastoreStoreExclusiveResponse {
    pub fn encode(&self) -> Result<Payload> {
        let mut builder = Builder::new_default();
        {
            let mut response =
                builder.init_root::<datastore_capnp::datastore_store_exclusive_response::Builder>();
            match self {
                DatastoreStoreExclusiveResponse::Stored => response.set_stored(()),
                DatastoreStoreExclusiveResponse::Refused {
                    value,
                    encoding,
                    last_modified_by,
                } => {
                    let mut refused = response.init_refused();
                    refused.set_value(value);
                    refused.set_encoding(encoding);
                    refused.set_last_modified_by(last_modified_by);
                }
            }
        }
        encode_message(&builder)
    }

    pub fn decode(data: &[u8]) -> Result<Self> {
        let reader = decode_message(data)?;
        let response =
            reader.get_root::<datastore_capnp::datastore_store_exclusive_response::Reader>()?;
        Ok(match response.which()? {
            datastore_capnp::datastore_store_exclusive_response::Which::Unset(()) => {
                return Err(crate::Error::Decoding(
                    "DatastoreStoreExclusiveResponse names no outcome".to_owned(),
                ));
            }
            datastore_capnp::datastore_store_exclusive_response::Which::Stored(()) => {
                DatastoreStoreExclusiveResponse::Stored
            }
            datastore_capnp::datastore_store_exclusive_response::Which::Refused(refused) => {
                DatastoreStoreExclusiveResponse::Refused {
                    value: refused.get_value()?.to_vec(),
                    encoding: refused.get_encoding()?.to_str()?.to_owned(),
                    last_modified_by: refused.get_last_modified_by()?.to_str()?.to_owned(),
                }
            }
        })
    }
}

impl crate::encoding::Wire for DatastoreStoreRequest {
    type Root = crate::datastore_capnp::datastore_store_request::Owned;
}

impl crate::encoding::Wire for DatastoreStoreResponse {
    type Root = crate::datastore_capnp::datastore_store_response::Owned;
}

impl crate::encoding::Wire for DatastoreGetRequest {
    type Root = crate::datastore_capnp::datastore_get_request::Owned;
}

impl crate::encoding::Wire for DatastoreGetResponse {
    type Root = crate::datastore_capnp::datastore_get_response::Owned;
}

impl crate::encoding::Wire for DatastoreListRequest {
    type Root = crate::datastore_capnp::datastore_list_request::Owned;
}

impl crate::encoding::Wire for DatastoreListResponse {
    type Root = crate::datastore_capnp::datastore_list_response::Owned;
}

impl crate::encoding::Wire for DatastoreRemoveRequest {
    type Root = crate::datastore_capnp::datastore_remove_request::Owned;
}

impl crate::encoding::Wire for DatastoreRemoveResponse {
    type Root = crate::datastore_capnp::datastore_remove_response::Owned;
}

impl crate::encoding::Wire for DatastoreStoreExclusiveRequest {
    type Root = crate::datastore_capnp::datastore_store_request::Owned;
}

impl crate::encoding::Wire for DatastoreStoreExclusiveResponse {
    type Root = crate::datastore_capnp::datastore_store_exclusive_response::Owned;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn store_request_round_trips_text_value() {
        let request = DatastoreStoreRequest::new("greeting", b"hello".to_vec(), "text/plain")
            .expect("valid key");
        let payload = request.encode().expect("encode");
        let decoded = DatastoreStoreRequest::decode(payload.as_ref()).expect("decode");
        assert_eq!(decoded, request);
    }

    #[test]
    fn store_request_round_trips_binary_value() {
        // Non-UTF-8 bytes prove the value rides in a `Data` field, not `Text`.
        let value = vec![0u8, 255, 0x80, 0xFE, 0x00, 0x01];
        let request = DatastoreStoreRequest::new("blob", value.clone(), "application/octet-stream")
            .expect("valid key");
        let payload = request.encode().expect("encode");
        let decoded = DatastoreStoreRequest::decode(payload.as_ref()).expect("decode");
        assert_eq!(decoded.value, value);
        assert_eq!(decoded, request);
    }

    #[test]
    fn store_request_round_trips_empty_value() {
        let request = DatastoreStoreRequest::new("empty", Vec::new(), "").expect("valid key");
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
    fn get_request_round_trips_node_name_key() {
        // A simple key of letters, digits, `_` and `-` round-trips unchanged.
        let key = "robot_state-1";
        let request = DatastoreGetRequest::new(key).expect("valid key");
        let payload = request.encode().expect("encode");
        let decoded = DatastoreGetRequest::decode(payload.as_ref()).expect("decode");
        assert_eq!(decoded.key.as_str(), key);
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
                key: "a-b-1".to_owned(),
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
    fn remove_request_round_trips_node_name_key() {
        let key = "robot_state-1";
        let request = DatastoreRemoveRequest::new(key).expect("valid key");
        let payload = request.encode().expect("encode");
        let decoded = DatastoreRemoveRequest::decode(payload.as_ref()).expect("decode");
        assert_eq!(decoded.key.as_str(), key);
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

    #[test]
    fn datastore_key_accepts_arbitrary_non_empty_keys() {
        // Keys are arbitrary strings carried in the payload (not a Zenoh
        // keyexpr), so any character is allowed: node-name-style keys plus
        // slashes, dots, spaces, other punctuation and non-ASCII text.
        for key in [
            "a",
            "robot_state",
            "sensors_temp_01",
            "Node",
            "a-b-c",
            "node-1_v2",
            "robot/state",
            "a.b.c",
            "a b",
            "a*b",
            "a$b",
            "a#b",
            "a?b",
            "a@b",
            "café",
            "日本語",
        ] {
            DatastoreKey::try_from(key)
                .unwrap_or_else(|e| panic!("`{key}` should be a valid datastore key: {e}"));
        }
    }

    #[test]
    fn datastore_key_rejects_empty() {
        assert_eq!(
            DatastoreKey::try_from("").expect_err("empty key should be rejected"),
            DatastoreKeyError::Empty
        );
    }

    /// Asserts that building or decoding the `request` failed on its
    /// datastore key.
    fn assert_invalid_datastore_key<T: std::fmt::Debug>(request: &str, result: crate::Result<T>) {
        let err = result.expect_err(&format!("the {request} request should refuse an empty key"));
        assert!(
            matches!(err, crate::Error::InvalidDatastoreKey(_)),
            "{request} request: expected InvalidDatastoreKey, got {err:?}"
        );
    }

    #[test]
    fn requests_new_reject_an_empty_key() {
        for (request, result) in [
            (
                "store",
                DatastoreStoreRequest::new("", b"v".to_vec(), "text/plain").map(drop),
            ),
            ("get", DatastoreGetRequest::new("").map(drop)),
            ("remove", DatastoreRemoveRequest::new("").map(drop)),
            (
                "store_exclusive",
                DatastoreStoreExclusiveRequest::new("", b"v".to_vec(), "text/plain").map(drop),
            ),
        ] {
            assert_invalid_datastore_key(request, result);
        }
    }

    #[test]
    fn store_request_round_trips_key_with_special_characters() {
        // Keys are arbitrary strings, so slashes and other punctuation that a
        // Zenoh keyexpr would reserve must round-trip unchanged.
        let key = "robot/state.last?v=2";
        let request =
            DatastoreStoreRequest::new(key, b"v".to_vec(), "text/plain").expect("valid key");
        let payload = request.encode().expect("encode");
        let decoded = DatastoreStoreRequest::decode(payload.as_ref()).expect("decode");
        assert_eq!(decoded.key.as_str(), key);
    }

    #[test]
    fn get_request_round_trips_key_with_special_characters() {
        let key = "robot/state.last?v=2";
        let request = DatastoreGetRequest::new(key).expect("valid key");
        let payload = request.encode().expect("encode");
        let decoded = DatastoreGetRequest::decode(payload.as_ref()).expect("decode");
        assert_eq!(decoded.key.as_str(), key);
    }

    #[test]
    fn remove_request_round_trips_key_with_special_characters() {
        let key = "robot/state.last?v=2";
        let request = DatastoreRemoveRequest::new(key).expect("valid key");
        let payload = request.encode().expect("encode");
        let decoded = DatastoreRemoveRequest::decode(payload.as_ref()).expect("decode");
        assert_eq!(decoded.key.as_str(), key);
    }

    #[test]
    fn store_request_decode_rejects_empty_wire_key() {
        // An empty key placed straight on the wire is still rejected at decode
        // time; non-empty keys are accepted regardless of their characters.
        let mut builder = Builder::new_default();
        {
            let mut request =
                builder.init_root::<datastore_capnp::datastore_store_request::Builder>();
            request.set_key("");
            request.set_value(b"v");
            request.set_encoding("text/plain");
        }
        let payload = encode_message(&builder).expect("encode raw request");

        assert_invalid_datastore_key("store", DatastoreStoreRequest::decode(payload.as_ref()));
    }

    #[test]
    fn get_request_decode_rejects_empty_wire_key() {
        let mut builder = Builder::new_default();
        {
            let mut request =
                builder.init_root::<datastore_capnp::datastore_get_request::Builder>();
            request.set_key("");
        }
        let payload = encode_message(&builder).expect("encode raw request");

        assert_invalid_datastore_key("get", DatastoreGetRequest::decode(payload.as_ref()));
    }

    #[test]
    fn remove_request_decode_rejects_empty_wire_key() {
        let mut builder = Builder::new_default();
        {
            let mut request =
                builder.init_root::<datastore_capnp::datastore_remove_request::Builder>();
            request.set_key("");
        }
        let payload = encode_message(&builder).expect("encode raw request");

        assert_invalid_datastore_key("remove", DatastoreRemoveRequest::decode(payload.as_ref()));
    }

    #[test]
    fn store_exclusive_response_decode_refuses_a_message_with_no_outcome() {
        let mut builder = Builder::new_default();
        builder.init_root::<datastore_capnp::datastore_store_exclusive_response::Builder>();
        let payload = encode_message(&builder).expect("encode raw response");

        let err = DatastoreStoreExclusiveResponse::decode(payload.as_ref())
            .expect_err("an unset outcome is refused");
        assert!(matches!(err, crate::Error::Decoding(_)), "{err:?}");
    }

    #[test]
    fn store_exclusive_response_round_trips_each_outcome() {
        for response in [
            DatastoreStoreExclusiveResponse::Stored,
            DatastoreStoreExclusiveResponse::Refused {
                value: vec![0u8, 1, 250, 255],
                encoding: "application/octet-stream".to_owned(),
                last_modified_by: "owner_node".to_owned(),
            },
        ] {
            let payload = response.encode().expect("encode");
            let decoded =
                DatastoreStoreExclusiveResponse::decode(payload.as_ref()).expect("decode");
            assert_eq!(decoded, response);
        }
    }
}
