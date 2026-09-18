//! The injected time source behind freshness and update-rate policies.

use std::fmt;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

/// A monotonic-enough nanosecond source with an arbitrary origin. The
/// runtime only ever compares differences between readings, so any source
/// that never runs backwards works: the wall clock, a node's sim-time-aware
/// clock, or a test counter.
#[derive(Clone)]
pub struct Clock {
    now_nanos: Arc<dyn Fn() -> u64 + Send + Sync>,
}

impl Clock {
    /// Wraps a nanosecond reading, such as the host's sim-time-aware
    /// clock.
    pub fn from_nanos_fn(now_nanos: impl Fn() -> u64 + Send + Sync + 'static) -> Self {
        Self {
            now_nanos: Arc::new(now_nanos),
        }
    }

    /// The host wall clock, as nanoseconds since the Unix epoch.
    pub fn wall() -> Self {
        Self::from_nanos_fn(|| {
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos() as u64
        })
    }

    pub fn now_nanos(&self) -> u64 {
        (self.now_nanos)()
    }
}

impl fmt::Debug for Clock {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Clock").finish_non_exhaustive()
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::Clock;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// A clock the test advances by hand.
    pub(crate) fn manual_clock() -> (Clock, Arc<AtomicU64>) {
        let nanos = Arc::new(AtomicU64::new(0));
        let source = Arc::clone(&nanos);
        let clock = Clock::from_nanos_fn(move || source.load(Ordering::SeqCst));
        (clock, nanos)
    }
}
