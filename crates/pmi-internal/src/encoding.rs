// TODO: Finish with the implementation of encoding, for the moment only plain messages are passed
mod capnproto;
mod plain;

use super::error::Result;
use bytes::Bytes;
use capnproto::CapnProtoEncoder;
use plain::PlainEncoder;

/// Defines the encoding interface for message serialization/deserialization
pub trait EncodingBackend: Send + Sync {
    /// Encode a message into bytes
    fn encode<T: serde::Serialize>(&self, message: &T) -> Result<Bytes>;

    /// Decode bytes into a message
    fn decode<T: serde::de::DeserializeOwned>(&self, data: &[u8]) -> Result<T>;

    /// Get the encoding format name
    fn format_name(&self) -> &str;
}

/// Main encoding implementation that abstracts over different encoding backends
pub struct Encoder {
    adapter: EncoderAdapter,
}

impl Encoder {
    /// Create a new encoder with the specified format
    pub fn new(format: EncodingFormat) -> Result<Self> {
        let adapter = match format {
            EncodingFormat::CapnProto => EncoderAdapter::CapnProto(CapnProtoEncoder::new()),
            EncodingFormat::Plain => EncoderAdapter::Plain(PlainEncoder::new()),
        };
        Ok(Self { adapter })
    }

    /// Create encoder from string format name
    pub fn from_format_name(format: &str) -> Result<Self> {
        let format = EncodingFormat::from_str(format)?;
        Self::new(format)
    }
}

/// Supported encoding formats
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EncodingFormat {
    CapnProto,
    Plain,
}

impl EncodingFormat {
    fn from_str(s: &str) -> Result<Self> {
        match s.to_lowercase().as_str() {
            "capnproto" | "capnp" => Ok(EncodingFormat::CapnProto),
            "protobuf" | "proto" => Ok(EncodingFormat::Plain),
            _ => Err(super::error::Error::UnsupportedEncoding(s.to_string())),
        }
    }
}

/// Dispatches encoder calls to the appropriate backend
enum EncoderAdapter {
    CapnProto(CapnProtoEncoder),
    Plain(PlainEncoder),
}

impl EncodingBackend for Encoder {
    fn encode<T: serde::Serialize>(&self, message: &T) -> Result<Bytes> {
        match &self.adapter {
            EncoderAdapter::CapnProto(encoder) => encoder.encode(message),
            EncoderAdapter::Plain(encoder) => encoder.encode(message),
        }
    }

    fn decode<T: serde::de::DeserializeOwned>(&self, data: &[u8]) -> Result<T> {
        match &self.adapter {
            EncoderAdapter::CapnProto(encoder) => encoder.decode(data),
            EncoderAdapter::Plain(encoder) => encoder.decode(data),
        }
    }

    fn format_name(&self) -> &str {
        match &self.adapter {
            EncoderAdapter::CapnProto(encoder) => encoder.format_name(),
            EncoderAdapter::Plain(encoder) => encoder.format_name(),
        }
    }
}
