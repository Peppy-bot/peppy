use super::super::error::Result;
use super::EncodingBackend;
use bytes::Bytes;

pub struct PlainEncoder;

impl PlainEncoder {
    pub fn new() -> Self {
        Self
    }
}

impl EncodingBackend for PlainEncoder {
    fn encode<T: serde::Serialize>(&self, _message: &T) -> Result<Bytes> {
        // TODO: Implement Protobuf encoding
        // This would use prost or protobuf crate for actual implementation
        todo!("Protobuf encoding not yet implemented")
    }

    fn decode<T: serde::de::DeserializeOwned>(&self, _data: &[u8]) -> Result<T> {
        // TODO: Implement Protobuf decoding
        todo!("Protobuf decoding not yet implemented")
    }

    fn format_name(&self) -> &str {
        "protobuf"
    }
}
