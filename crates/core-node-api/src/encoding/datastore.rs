//! Cap'n Proto encoding utilities for datastore messages.
//!
//! Keys are arbitrary strings (any character) and values are arbitrary bytes
//! carried in a Cap'n Proto `Data` field, paired with a Zenoh-style encoding
//! tag — so any value type accepted by Zenoh round-trips faithfully.

use capnp::message::Builder;

use crate::datastore_capnp;
use crate::{Payload, Result};

use super::{decode_message, encode_message};

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

/// Result of a get. When `found` is false, `value` and `encoding` are empty.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DatastoreGetResponse {
    pub found: bool,
    pub value: Vec<u8>,
    pub encoding: String,
}

impl DatastoreGetResponse {
    pub fn found(value: impl Into<Vec<u8>>, encoding: impl Into<String>) -> Self {
        Self {
            found: true,
            value: value.into(),
            encoding: encoding.into(),
        }
    }

    pub fn not_found() -> Self {
        Self {
            found: false,
            value: Vec::new(),
            encoding: String::new(),
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
        let response = DatastoreGetResponse::found(value.clone(), "application/octet-stream");
        let payload = response.encode().expect("encode");
        let decoded = DatastoreGetResponse::decode(payload.as_ref()).expect("decode");
        assert_eq!(decoded, response);
        assert!(decoded.found);
        assert_eq!(decoded.value, value);
    }

    #[test]
    fn get_response_round_trips_not_found() {
        let response = DatastoreGetResponse::not_found();
        let payload = response.encode().expect("encode");
        let decoded = DatastoreGetResponse::decode(payload.as_ref()).expect("decode");
        assert!(!decoded.found);
        assert!(decoded.value.is_empty());
        assert!(decoded.encoding.is_empty());
    }
}
