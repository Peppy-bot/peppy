//! Durations a loop can actually sleep for.
//!
//! A configured rate or a seconds value is a number; a tick period is a
//! `Duration` the loop waits on. These convert one into the other or refuse
//! it. What the number is called, where it came from, and what a caller does
//! with a refusal are the caller's concerns: each failure here describes only
//! the value.

use std::time::Duration;
use thiserror::Error;

/// Nanoseconds in a second: the resolution a period is expressed in.
const NANOS_PER_SECOND: u64 = 1_000_000_000;

/// A rate outside the range its caller accepts, or with no period to express.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
#[error("must be in 1..={max_hz} Hz, got {got}")]
pub struct RateOutOfRange {
    pub got: u32,
    pub max_hz: u32,
}

/// A seconds value with no duration this can express.
#[derive(Debug, Clone, Copy, Error)]
pub enum DurationError {
    #[error("must be a positive finite number of seconds, got {0}")]
    NotPositiveFinite(f64),

    #[error("{0:e} s is longer than a Duration holds")]
    TooLarge(f64),

    #[error("{0:e} s rounds to no time at all")]
    Underflow(f64),
}

/// The tick period for a whole-hertz rate, refused above `max_hz`.
///
/// `max_hz` is the caller's ceiling: the fastest the thing being paced can
/// actually run. Only the caller knows it, so only the caller can state it,
/// and the refusal quotes the bound it was given rather than one invented here.
///
/// A rate that does not divide a second evenly truncates to the nanosecond
/// below, pacing marginally fast rather than slow. The shortfall is under
/// `rate_hz / 1e9` of the period, so a caller pacing below 1 MHz loses at most
/// 0.1%. Above 1e9 Hz no non-zero period exists and every rate is refused,
/// whatever `max_hz` says.
pub fn period_from_hz(rate_hz: u32, max_hz: u32) -> Result<Duration, RateOutOfRange> {
    (1..=max_hz)
        .contains(&rate_hz)
        .then(|| Duration::from_nanos(NANOS_PER_SECOND / u64::from(rate_hz)))
        .filter(|period| !period.is_zero())
        .ok_or(RateOutOfRange {
            got: rate_hz,
            max_hz,
        })
}

/// A duration from a seconds value.
///
/// Covers every input `Duration::from_secs_f64` panics on, plus the positive
/// values below half a nanosecond that `try_from_secs_f64` rounds to zero and
/// reports as success: a zero duration is a timeout that has always expired.
pub fn duration_from_secs(secs: f64) -> Result<Duration, DurationError> {
    if !(secs.is_finite() && secs > 0.0) {
        return Err(DurationError::NotPositiveFinite(secs));
    }
    let period = Duration::try_from_secs_f64(secs).map_err(|_| DurationError::TooLarge(secs))?;
    (!period.is_zero())
        .then_some(period)
        .ok_or(DurationError::Underflow(secs))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The refusal quotes the caller's ceiling, not a bound this module chose.
    #[test]
    fn a_rate_past_the_callers_ceiling_is_refused_against_that_ceiling() {
        for (got, max_hz) in [(0, 1_000), (1_001, 1_000), (u32::MAX, 1_000), (2, 1)] {
            assert_eq!(
                period_from_hz(got, max_hz),
                Err(RateOutOfRange { got, max_hz })
            );
        }
        assert_eq!(
            period_from_hz(1_000, 1_000_000).unwrap(),
            Duration::from_millis(1)
        );
    }

    /// No `max_hz` can talk this into a zero period, which would spin a loop
    /// with no delay between ticks.
    #[test]
    fn a_rate_with_no_expressible_period_is_refused_however_high_the_ceiling() {
        for rate_hz in [1_000_000_001, u32::MAX] {
            assert!(period_from_hz(rate_hz, u32::MAX).is_err(), "{rate_hz} Hz");
        }
    }

    #[test]
    fn a_rate_in_range_yields_its_period() {
        assert_eq!(
            period_from_hz(100, 1_000).unwrap(),
            Duration::from_millis(10)
        );
        assert_eq!(period_from_hz(1, 1_000).unwrap(), Duration::from_secs(1));
    }

    /// Checked across the range rather than argued: a period is never longer
    /// than the rate asked for (so a loop never runs slow), and never more
    /// than 0.1% shorter.
    #[test]
    fn no_rate_below_a_megahertz_truncates_into_a_meaningfully_faster_loop() {
        for rate_hz in 1..=1_000_000u32 {
            let period = period_from_hz(rate_hz, 1_000_000).unwrap().as_secs_f64();
            let asked = 1.0 / f64::from(rate_hz);
            assert!(period <= asked, "{rate_hz} Hz paces slower than asked");
            assert!(
                (asked - period) / asked <= 0.001,
                "{rate_hz} Hz runs {:.3}% fast",
                (asked - period) / asked * 100.0
            );
        }
    }

    #[test]
    fn every_seconds_value_from_secs_f64_would_panic_on_is_refused() {
        for secs in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, -1.0, 0.0] {
            assert!(
                matches!(
                    duration_from_secs(secs),
                    Err(DurationError::NotPositiveFinite(_))
                ),
                "{secs} must be refused"
            );
        }
        assert!(matches!(
            duration_from_secs(f64::MAX),
            Err(DurationError::TooLarge(_))
        ));
    }

    /// `try_from_secs_f64` rounds anything under half a nanosecond to zero and
    /// reports success. A zero timeout counts every sample as already expired.
    #[test]
    fn a_positive_seconds_value_that_rounds_to_nothing_is_refused() {
        for secs in [1e-10, 1e-300, f64::MIN_POSITIVE] {
            assert!(
                matches!(duration_from_secs(secs), Err(DurationError::Underflow(_))),
                "{secs:e} must be refused"
            );
        }
    }

    /// A refused magnitude is reported in scientific notation; `f64::MAX`
    /// written out in full is 309 digits.
    #[test]
    fn a_refused_magnitude_is_reported_compactly() {
        let text = duration_from_secs(f64::MAX).unwrap_err().to_string();
        assert!(text.contains("1.7976931348623157e308"), "got {text}");
        assert!(text.len() < 60, "{} chars: {text}", text.len());
    }

    #[test]
    fn a_usable_seconds_value_yields_its_duration() {
        assert_eq!(
            duration_from_secs(0.25).unwrap(),
            Duration::from_millis(250)
        );
    }
}
