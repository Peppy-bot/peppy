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
