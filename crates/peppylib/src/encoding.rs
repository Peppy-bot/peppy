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

/// Encode a Cap'n Proto message builder into bytes. Crate-internal: used by the
/// [`capnp_empty_message!`] macro and the action cancel-ack codec.
pub(crate) fn encode_message(message: &Builder<HeapAllocator>) -> Result<Payload> {
    let mut buffer = Vec::new();
    serialize::write_message(&mut buffer, message)
        .map_err(|e| crate::error::Error::Serialization(e.to_string()))?;
    Ok(Payload::from(buffer))
}

/// Decode bytes into a Cap'n Proto message reader. Returns an owned segments
/// reader. Crate-internal: used by the [`capnp_empty_message!`] macro and the
/// action cancel-ack codec.
pub(crate) fn decode_message(
    data: &[u8],
) -> Result<capnp::message::Reader<capnp::serialize::OwnedSegments>> {
    serialize::read_message(data, ReaderOptions::default())
        .map_err(|e| crate::error::Error::Deserialization(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::{CapnpTimestamp, convert_time, convert_time_from_capnp};
    use std::time::{Duration, UNIX_EPOCH};

    /// Every timestamped wire message round-trips through these two functions, so
    /// the conversion must be exactly invertible across the epoch boundary. The
    /// table is deterministic (no `SystemTime::now()`), and each case names the
    /// branch it exercises.
    #[test]
    fn convert_time_round_trips_across_the_epoch_boundary() {
        let cases = [
            ("epoch", UNIX_EPOCH),
            ("epoch + sub-second", UNIX_EPOCH + Duration::new(0, 500)),
            (
                "realistic 2023 instant",
                UNIX_EPOCH + Duration::new(1_700_000_000, 123_456_789),
            ),
            (
                "pre-epoch, sub-second",
                UNIX_EPOCH - Duration::new(0, 500_000_000),
            ),
            ("pre-epoch, exact second", UNIX_EPOCH - Duration::new(3, 0)),
            (
                "pre-epoch, second + nanos",
                UNIX_EPOCH - Duration::new(2, 250_000_000),
            ),
        ];

        for (label, original) in cases {
            let restored = convert_time_from_capnp(convert_time(original));
            assert_eq!(restored, original, "round-trip failed for {label}");
        }
    }

    #[test]
    fn convert_time_encodes_post_epoch_fields_directly() {
        let ts = convert_time(UNIX_EPOCH + Duration::new(10, 250));
        assert_eq!(ts, CapnpTimestamp { sec: 10, nsec: 250 });
    }

    #[test]
    fn convert_time_borrows_into_the_previous_second_for_sub_second_pre_epoch() {
        // 0.5s before the epoch: the seconds field rolls back one extra second
        // and the nanos carry the complement, so the pair still sums to -0.5s.
        let ts = convert_time(UNIX_EPOCH - Duration::new(0, 500_000_000));
        assert_eq!(
            ts,
            CapnpTimestamp {
                sec: -1,
                nsec: 500_000_000
            }
        );
    }

    #[test]
    fn convert_time_keeps_whole_seconds_for_exact_pre_epoch_second() {
        // An exact second before the epoch has no nanos to borrow, so the
        // nsec==0 branch keeps the seconds field at the negated whole second.
        let ts = convert_time(UNIX_EPOCH - Duration::new(3, 0));
        assert_eq!(ts, CapnpTimestamp { sec: -3, nsec: 0 });
    }
}
