use super::super::error::Result;
use super::EncodingBackend;
use bytes::Bytes;

pub struct CapnProtoEncoder;

impl CapnProtoEncoder {
    pub fn new() -> Self {
        Self
    }
}

impl EncodingBackend for CapnProtoEncoder {
    fn encode<T: serde::Serialize>(&self, _message: &T) -> Result<Bytes> {
        // TODO: Implement Cap'n Proto encoding
        // This would use capnp crate for actual implementation
        todo!("Cap'n Proto encoding not yet implemented")
    }

    fn decode<T: serde::de::DeserializeOwned>(&self, _data: &[u8]) -> Result<T> {
        // TODO: Implement Cap'n Proto decoding
        todo!("Cap'n Proto decoding not yet implemented")
    }

    fn format_name(&self) -> &str {
        "capnproto"
    }
}
