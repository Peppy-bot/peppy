//! Cap'n Proto encoding utilities for datastore messages.
//!
//! Keys are restricted to the same character set as a node name (see
//! [`DatastoreKey`]): a non-empty string of ASCII letters, digits, `_` and `-`.
//! Values are arbitrary bytes carried in a Cap'n Proto `Data` field, paired
//! with a Zenoh-style encoding tag, so any value type accepted by Zenoh
//! round-trips faithfully.

use std::fmt;

use capnp::message::Builder;

use crate::datastore_capnp;
use crate::{Payload, Result};

use super::{capnp_list_len, decode_message, encode_message};

/// A datastore key, validated to use the same characters allowed in a node
/// name.
///
/// The datastore stores and retrieves values by exact key. To keep keys simple
/// and free of special characters, a key must be non-empty and contain only the
/// characters permitted in a node name: ASCII letters, digits, `_` and `-`, as
/// defined by the shared `config::consts::ALLOWED_CONFIG_CHARS` constant.
/// Slashes, whitespace, other punctuation and non-ASCII text are all rejected.
///
/// Construct one with [`DatastoreKey::try_from`]; an invalid key yields a
/// [`DatastoreKeyError`]. Following the "parse, don't validate" pattern, once a
/// `DatastoreKey` exists it is guaranteed valid, so request types carry it
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
        validate_datastore_key(&key)?;
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

/// Why a string was rejected as a [`DatastoreKey`]. Each variant repeats the
/// offending key so the caller can see exactly what was passed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DatastoreKeyError {
    /// The key was the empty string.
    #[error("datastore key must not be empty")]
    Empty,

    /// The key contained a character outside the node-name character set
    /// (`config::consts::ALLOWED_CONFIG_CHARS`), such as a slash, whitespace,
    /// other punctuation or non-ASCII text.
    #[error(
        "datastore key {key:?} contains the disallowed character {character:?} at byte {index}: datastore keys may only use the characters allowed in a node name ({allowed})",
        allowed = config::consts::ALLOWED_CONFIG_CHARS,
    )]
    ForbiddenCharacter {
        key: String,
        index: usize,
        character: char,
    },
}

/// Checks that `key` uses only the characters allowed in a node name.
///
/// A key must be non-empty and contain only the characters in
/// `config::consts::ALLOWED_CONFIG_CHARS` (ASCII letters, digits, `_` and `-`).
/// Reusing that constant keeps datastore keys aligned with node names, so they
/// stay free of slashes, whitespace and other special characters.
fn validate_datastore_key(key: &str) -> core::result::Result<(), DatastoreKeyError> {
    if key.is_empty() {
        return Err(DatastoreKeyError::Empty);
    }

    if let Some((index, character)) = key
        .char_indices()
        .find(|(_, c)| !config::consts::ALLOWED_CONFIG_CHARS.contains(*c))
    {
        return Err(DatastoreKeyError::ForbiddenCharacter {
            key: key.to_owned(),
            index,
            character,
        });
    }

    Ok(())
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
    /// key uses characters outside the node-name set (ASCII letters, digits,
    /// `_` and `-`).
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
    /// key uses characters outside the node-name set (ASCII letters, digits,
    /// `_` and `-`).
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
    /// key uses characters outside the node-name set (ASCII letters, digits,
    /// `_` and `-`).
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
        // Keys use the node-name character set: letters, digits, `_` and `-`.
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
    fn datastore_key_accepts_node_name_keys() {
        // The allowed set is the node-name character set: ASCII letters (either
        // case), digits, `_` and `-`.
        for key in [
            "a",
            "robot_state",
            "sensors_temp_01",
            "Node",
            "a-b-c",
            "node-1_v2",
        ] {
            DatastoreKey::try_from(key)
                .unwrap_or_else(|e| panic!("`{key}` should be a valid datastore key: {e}"));
        }
    }

    #[test]
    fn datastore_key_rejects_disallowed_characters() {
        // Anything outside the node-name set is rejected, and the error names
        // the first offending character: slashes, dots, whitespace, other
        // punctuation, the old Zenoh wildcards/reserved chars, and non-ASCII.
        let cases = [
            ("a/b", '/'),
            ("a.b", '.'),
            ("a b", ' '),
            ("a*b", '*'),
            ("a$b", '$'),
            ("a#b", '#'),
            ("a?b", '?'),
            ("a@b", '@'),
            ("日本語", '日'),
        ];
        for (key, expected) in cases {
            let err =
                DatastoreKey::try_from(key).expect_err(&format!("`{key}` should be rejected"));
            assert!(
                matches!(err, DatastoreKeyError::ForbiddenCharacter { character, .. } if character == expected),
                "`{key}` should fail with ForbiddenCharacter({expected:?}), got {err:?}"
            );
        }
    }

    #[test]
    fn datastore_key_reports_the_offending_character_and_position() {
        let err = DatastoreKey::try_from("ok-bad/key").expect_err("`/` should be rejected");
        assert_eq!(
            err,
            DatastoreKeyError::ForbiddenCharacter {
                key: "ok-bad/key".to_owned(),
                index: 6,
                character: '/',
            }
        );

        // The reported index is a byte offset, so it must account for the
        // single-byte ASCII text preceding the first non-ASCII (and therefore
        // disallowed) character (`café` puts `é` at byte 3).
        let err = DatastoreKey::try_from("café").expect_err("non-ASCII should be rejected");
        assert_eq!(
            err,
            DatastoreKeyError::ForbiddenCharacter {
                key: "café".to_owned(),
                index: 3,
                character: 'é',
            }
        );
    }

    #[test]
    fn datastore_key_rejects_empty() {
        assert_eq!(
            DatastoreKey::try_from("").expect_err("empty key should be rejected"),
            DatastoreKeyError::Empty
        );
    }

    #[test]
    fn store_request_new_rejects_invalid_key() {
        let err = DatastoreStoreRequest::new("a/*", b"v".to_vec(), "text/plain")
            .expect_err("wildcard key should be rejected");
        assert!(
            matches!(err, crate::Error::InvalidDatastoreKey(_)),
            "expected InvalidDatastoreKey, got {err:?}"
        );
    }

    #[test]
    fn get_request_new_rejects_invalid_key() {
        let err = DatastoreGetRequest::new("a/b?c")
            .expect_err("reserved-character key should be rejected");
        assert!(
            matches!(err, crate::Error::InvalidDatastoreKey(_)),
            "expected InvalidDatastoreKey, got {err:?}"
        );
    }

    #[test]
    fn remove_request_new_rejects_invalid_key() {
        let err = DatastoreRemoveRequest::new("a$b").expect_err("`$` key should be rejected");
        assert!(
            matches!(err, crate::Error::InvalidDatastoreKey(_)),
            "expected InvalidDatastoreKey, got {err:?}"
        );
    }

    #[test]
    fn store_request_decode_rejects_invalid_wire_key() {
        // Defense in depth: a peer that bypasses `new` and puts a wildcard key
        // straight on the wire is still rejected at decode time.
        let mut builder = Builder::new_default();
        {
            let mut request =
                builder.init_root::<datastore_capnp::datastore_store_request::Builder>();
            request.set_key("a/**");
            request.set_value(b"v");
            request.set_encoding("text/plain");
        }
        let payload = encode_message(&builder).expect("encode raw request");

        let err = DatastoreStoreRequest::decode(payload.as_ref())
            .expect_err("invalid wire key should be rejected at decode");
        assert!(
            matches!(err, crate::Error::InvalidDatastoreKey(_)),
            "expected InvalidDatastoreKey, got {err:?}"
        );
    }

    #[test]
    fn get_request_decode_rejects_invalid_wire_key() {
        // Defense in depth for the get path: a reserved character placed
        // straight on the wire is rejected at decode time.
        let mut builder = Builder::new_default();
        {
            let mut request =
                builder.init_root::<datastore_capnp::datastore_get_request::Builder>();
            request.set_key("a/b?c");
        }
        let payload = encode_message(&builder).expect("encode raw request");

        let err = DatastoreGetRequest::decode(payload.as_ref())
            .expect_err("invalid wire key should be rejected at decode");
        assert!(
            matches!(err, crate::Error::InvalidDatastoreKey(_)),
            "expected InvalidDatastoreKey, got {err:?}"
        );
    }

    #[test]
    fn remove_request_decode_rejects_invalid_wire_key() {
        // Defense in depth for the remove path: a bare `$` placed straight on
        // the wire is rejected at decode time.
        let mut builder = Builder::new_default();
        {
            let mut request =
                builder.init_root::<datastore_capnp::datastore_remove_request::Builder>();
            request.set_key("a$b");
        }
        let payload = encode_message(&builder).expect("encode raw request");

        let err = DatastoreRemoveRequest::decode(payload.as_ref())
            .expect_err("invalid wire key should be rejected at decode");
        assert!(
            matches!(err, crate::Error::InvalidDatastoreKey(_)),
            "expected InvalidDatastoreKey, got {err:?}"
        );
    }
}
