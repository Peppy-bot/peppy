//! The whole units of a [`Duration`] as a `u64`, the width in which the
//! configuration documents and the wire formats hold a count of time.

use std::time::Duration;

/// The whole milliseconds of `duration`. A count that does not fit stops at
/// `u64::MAX`.
#[must_use]
pub fn saturating_millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

/// The whole nanoseconds of `duration`. A count that does not fit stops at
/// `u64::MAX`.
#[must_use]
pub fn saturating_nanos(duration: Duration) -> u64 {
    u64::try_from(duration.as_nanos()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn saturating_millis_drops_the_part_of_a_millisecond() {
        assert_eq!(saturating_millis(Duration::from_micros(1_999)), 1);
        assert_eq!(saturating_millis(Duration::ZERO), 0);
    }

    #[test]
    fn saturating_millis_stops_at_the_largest_count() {
        assert_eq!(saturating_millis(Duration::from_millis(u64::MAX)), u64::MAX);
        assert_eq!(saturating_millis(Duration::MAX), u64::MAX);
    }

    #[test]
    fn saturating_nanos_counts_each_nanosecond() {
        assert_eq!(saturating_nanos(Duration::new(2, 5)), 2_000_000_005);
        assert_eq!(saturating_nanos(Duration::ZERO), 0);
    }

    #[test]
    fn saturating_nanos_stops_at_the_largest_count() {
        assert_eq!(saturating_nanos(Duration::from_nanos(u64::MAX)), u64::MAX);
        assert_eq!(saturating_nanos(Duration::MAX), u64::MAX);
    }
}
