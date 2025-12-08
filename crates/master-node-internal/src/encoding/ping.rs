//! Cap'n Proto encoding utilities for ping messages.

use bytes::Bytes;
use capnp::message::Builder;

use crate::Result;
use crate::messages_capnp;

use super::encode_message;

/// Convenience wrapper for building and encoding a ping response.
pub fn build_ping_response(timestamp: u64, message: &str) -> Result<Bytes> {
    let mut builder = Builder::new_default();
    {
        let mut response = builder.init_root::<messages_capnp::ping_response::Builder>();
        response.set_timestamp(timestamp);
        response.set_message(message);
    }
    encode_message(&builder)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::encoding::decode_message;

    #[test]
    fn test_ping_response_roundtrip() {
        let bytes = build_ping_response(12345, "pong").unwrap();

        let reader = decode_message(&bytes).unwrap();
        let response = reader
            .get_root::<messages_capnp::ping_response::Reader>()
            .unwrap();

        assert_eq!(response.get_timestamp(), 12345);
        assert_eq!(response.get_message().unwrap(), "pong");
    }
}
