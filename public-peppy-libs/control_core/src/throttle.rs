//! Rate gate for a repeating event: admits at most one per window.

use std::time::{Duration, Instant};

/// Admits at most one event per window, so a condition that persists at loop
/// or producer rate stays visible without burying everything else.
///
/// Unlike [`Pacer`](crate::pacer::Pacer) this never sleeps: it answers whether
/// now is a moment to act and leaves the caller running at its own rate.
///
/// The first call always admits. A condition's onset is the report worth most
/// and the one a fresh window would otherwise swallow.
pub struct Throttle {
    period: Duration,
    last: Option<Instant>,
}

impl Throttle {
    pub fn new(period: Duration) -> Self {
        Self { period, last: None }
    }

    /// A throttle whose first window has already started at `start`, for a
    /// caller that must wait one full window before its first report rather
    /// than emitting one immediately.
    pub fn started_at(period: Duration, start: Instant) -> Self {
        Self {
            period,
            last: Some(start),
        }
    }

    /// Whether to act at `now`; acting starts the next window.
    ///
    /// Takes the clock rather than reading it, so a caller that already has a
    /// timestamp uses the same one and a test drives the window without
    /// sleeping.
    pub fn admit_at(&mut self, now: Instant) -> bool {
        let due = self
            .last
            .is_none_or(|last| now.duration_since(last) >= self.period);
        if due {
            self.last = Some(now);
        }
        due
    }

    /// [`admit_at`](Self::admit_at) against the current instant.
    pub fn admit(&mut self) -> bool {
        self.admit_at(Instant::now())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_first_event_always_admits() {
        assert!(Throttle::new(Duration::from_secs(3600)).admit());
    }

    #[test]
    fn admits_once_per_window() {
        let mut throttle = Throttle::new(Duration::from_secs(3600));
        assert!(throttle.admit());
        assert!(!throttle.admit(), "the window has not elapsed");
        assert!(!throttle.admit());
    }

    #[test]
    fn a_new_window_admits_again() {
        let mut throttle = Throttle::new(Duration::from_millis(10));
        let t0 = Instant::now();
        assert!(throttle.admit_at(t0));
        assert!(!throttle.admit_at(t0 + Duration::from_millis(9)));
        assert!(throttle.admit_at(t0 + Duration::from_millis(10)));
    }

    #[test]
    fn a_started_throttle_waits_out_its_first_window() {
        let t0 = Instant::now();
        let mut throttle = Throttle::started_at(Duration::from_millis(10), t0);
        assert!(!throttle.admit_at(t0), "the window began at construction");
        assert!(throttle.admit_at(t0 + Duration::from_millis(10)));
    }

    #[test]
    fn a_zero_window_always_admits() {
        let mut throttle = Throttle::new(Duration::ZERO);
        assert!(throttle.admit());
        assert!(throttle.admit());
    }
}
