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
                $crate::encoding::encode_message(&builder)
            }

            pub fn decode(data: &[u8]) -> $crate::error::Result<Self> {
                let reader = $crate::encoding::decode_message(data)?;
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

/// Decode bytes into a Cap'n Proto reader that borrows `data` IN PLACE — no
/// copy into owned segments. Pair it with the borrowed
/// [`PayloadView`](crate::types::PayloadView) from `Message::payload()` and a
/// shared-memory delivery, and the consumer parses typed fields directly out
/// of the producer's buffer.
///
/// Requires `data` to be 8-byte aligned (Cap'n Proto words). Loaned publish
/// buffers are allocated 8-byte aligned for exactly this, and shared-memory
/// deliveries preserve that alignment end to end; payloads that traveled the
/// network path may land unaligned, in which case this returns a
/// deserialization error — fall back to [`decode_message`], which copies into
/// aligned owned segments. (The check is done eagerly here: capnp itself
/// would defer the misalignment failure to the first typed access.)
pub fn decode_message_in_place(
    mut data: &[u8],
) -> Result<capnp::message::Reader<capnp::serialize::BufferSegments<&[u8]>>> {
    if !(data.as_ptr() as usize).is_multiple_of(8) {
        return Err(crate::error::Error::Deserialization(
            "payload is not 8-byte aligned; fall back to decode_message".to_string(),
        ));
    }
    serialize::read_message_from_flat_slice(&mut data, ReaderOptions::default())
        .map_err(|e| crate::error::Error::Deserialization(e.to_string()))
}

/// Encode a Cap'n Proto builder straight into a loaned publish buffer from
/// `publisher`, sized exactly (the serialized size of a finished builder is
/// known up front). With shared memory on and the message at or above the
/// publish threshold, the serialized bytes are *born* in the shared-memory
/// segment and are never copied again:
///
/// ```ignore
/// let loan = encode_message_to_loan(&publisher, &builder)?;
/// publisher.publish_loaned(loan).await?;
/// ```
///
/// With shared memory off (or a sub-threshold message — most typed control
/// messages), the loan is a plain heap buffer and this is equivalent to
/// [`encode_message`] + `publish`.
pub fn encode_message_to_loan<A: capnp::message::Allocator>(
    publisher: &crate::messaging::TopicPublisher,
    message: &Builder<A>,
) -> Result<crate::messaging::LoanedPayload> {
    let size_bytes = serialize::compute_serialized_size_in_words(message) * 8;
    let mut loan = publisher.loan(size_bytes);
    let mut cursor = &mut loan[..];
    serialize::write_message(&mut cursor, message)
        .map_err(|e| crate::error::Error::Serialization(e.to_string()))?;
    let remaining = cursor.len();
    debug_assert_eq!(remaining, 0, "capnp serialized size is exact");
    // Defensive: if capnp ever writes less than computed, ship the prefix.
    let written = size_bytes - remaining;
    loan.truncate(written);
    Ok(loan)
}
