mod health;
mod shutdown;

use crate::error::Result;
use bytes::Bytes;
use capnp::message::{Builder, HeapAllocator, ReaderOptions};
use capnp::serialize;
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
/// use master_node::encoding::encode_message;
/// use master_node::messages_capnp;
///
/// let mut message = capnp::message::Builder::new_default();
/// let mut ping = message.init_root::<messages_capnp::ping_response::Builder>();
/// ping.set_message("pong");
/// ping.set_timestamp(12345);
///
/// let bytes = encode_message(&message)?;
/// ```
pub fn encode_message(message: &Builder<HeapAllocator>) -> Result<Bytes> {
    let mut buffer = Vec::new();
    serialize::write_message(&mut buffer, message)?;
    Ok(Bytes::from(buffer))
}

/// Decode bytes into a Cap'n Proto message reader.
///
/// Returns an owned segments reader that can be used to read the message.
///
/// # Example
/// ```ignore
/// use master_node::encoding::decode_message;
/// use master_node::messages_capnp;
///
/// let reader = decode_message(&bytes)?;
/// let ping = reader.get_root::<messages_capnp::ping_request::Reader>()?;
/// let timestamp = ping.get_timestamp();
/// ```
pub fn decode_message(
    data: &[u8],
) -> Result<capnp::message::Reader<capnp::serialize::OwnedSegments>> {
    Ok(serialize::read_message(data, ReaderOptions::default())?)
}
