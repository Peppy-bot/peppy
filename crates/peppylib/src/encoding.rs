pub mod health;
pub mod ready;

use crate::error::Result;
use crate::types::Payload;

/// Generates an empty Cap'n Proto message struct with `new()`, `encode()`, `decode()`,
/// and `Default` implementations.
macro_rules! capnp_empty_message {
    ($name:ident, $builder:path, $reader:path) => {
        #[derive(Debug, Clone, PartialEq, Eq)]
        pub struct $name {}

        impl $name {
            pub fn new() -> Self {
                Self {}
            }

            pub fn encode(&self) -> $crate::error::Result<$crate::types::Payload> {
                let mut builder = ::capnp::message::Builder::new_default();
                {
                    let _ = builder.init_root::<$builder>();
                }
                super::encode_message(&builder)
            }

            pub fn decode(data: &[u8]) -> $crate::error::Result<Self> {
                let reader = super::decode_message(data)?;
                let _ = reader
                    .get_root::<$reader>()
                    .map_err(|e| $crate::error::Error::Deserialization(e.to_string()))?;
                Ok(Self {})
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }
    };
}

use capnp::message::{Builder, HeapAllocator, ReaderOptions};
use capnp::serialize;
pub(crate) use capnp_empty_message;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const NANOS_PER_SEC: u32 = 1_000_000_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CapnpTimestamp {
    pub sec: i64,
    pub nsec: u32,
}

pub fn convert_time(timestamp: SystemTime) -> CapnpTimestamp {
    match timestamp.duration_since(UNIX_EPOCH) {
        Ok(duration) => CapnpTimestamp {
            sec: duration.as_secs() as i64,
            nsec: duration.subsec_nanos(),
        },
        Err(err) => {
            let duration = err.duration();
            let secs = duration.as_secs() as i64;
            let nanos = duration.subsec_nanos();

            if nanos == 0 {
                CapnpTimestamp {
                    sec: -secs,
                    nsec: 0,
                }
            } else {
                CapnpTimestamp {
                    sec: -secs - 1,
                    nsec: NANOS_PER_SEC - nanos,
                }
            }
        }
    }
}

pub fn convert_time_from_capnp(timestamp: CapnpTimestamp) -> SystemTime {
    debug_assert!(timestamp.nsec < NANOS_PER_SEC);

    if timestamp.sec >= 0 {
        UNIX_EPOCH + Duration::new(timestamp.sec as u64, timestamp.nsec)
    } else if timestamp.nsec == 0 {
        let secs_to_epoch = (-i128::from(timestamp.sec)) as u64;

        UNIX_EPOCH - Duration::new(secs_to_epoch, 0)
    } else {
        let secs_to_epoch = (-(i128::from(timestamp.sec) + 1)) as u64;
        let nanos_to_epoch = (i128::from(NANOS_PER_SEC) - i128::from(timestamp.nsec)) as u32;

        UNIX_EPOCH - Duration::new(secs_to_epoch, nanos_to_epoch)
    }
}

/// Encode a Cap'n Proto message builder into bytes.
///
/// # Example
/// ```ignore
/// use peppylib::encoding::encode_message;
///
/// let mut message = capnp::message::Builder::new_default();
/// message.init_root::<my_capnp::my_request::Builder>();
///
/// let payload = encode_message(&message).unwrap();
/// assert!(!payload.is_empty());
/// ```
pub fn encode_message(message: &Builder<HeapAllocator>) -> Result<Payload> {
    let mut buffer = Vec::new();
    serialize::write_message(&mut buffer, message)
        .map_err(|e| crate::error::Error::Serialization(e.to_string()))?;
    Ok(Payload::from(buffer))
}

/// Decode bytes into a Cap'n Proto message reader.
///
/// Returns an owned segments reader that can be used to read the message.
///
/// # Example
/// ```ignore
/// use peppylib::encoding::{decode_message, encode_message};
///
/// let mut message = capnp::message::Builder::new_default();
/// message.init_root::<my_capnp::my_request::Builder>();
/// let bytes = encode_message(&message).unwrap();
///
/// let reader = decode_message(&bytes).unwrap();
/// let _request = reader.get_root::<my_capnp::my_request::Reader>().unwrap();
/// ```
pub fn decode_message(
    data: &[u8],
) -> Result<capnp::message::Reader<capnp::serialize::OwnedSegments>> {
    serialize::read_message(data, ReaderOptions::default())
        .map_err(|e| crate::error::Error::Deserialization(e.to_string()))
}
