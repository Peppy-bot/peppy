use std::time::{SystemTime, UNIX_EPOCH};

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
                    nsec: 1_000_000_000u32 - nanos,
                }
            }
        }
    }
}
