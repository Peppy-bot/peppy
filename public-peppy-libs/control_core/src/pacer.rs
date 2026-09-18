//! Fixed-rate pacing for the control loop, with overrun accounting.

use std::time::{Duration, Instant};

use crate::throttle::Throttle;

use tracing::warn;

use thiserror::Error;

/// A pacer needs a period to pace to.
#[derive(Debug, Error, PartialEq, Eq)]
#[error("pacer period must be non-zero")]
pub struct ZeroPacerPeriod;

/// How often [`Pacer`] reports accumulated overruns, so a chronically overloaded
/// loop is loud in the log without flooding it at the control rate.
const OVERRUN_REPORT_PERIOD: Duration = Duration::from_secs(1);

/// An overrun tally due for reporting: how many control-loop overruns
/// accumulated since the last report, and the worst lateness among them.
#[derive(Debug, PartialEq)]
struct OverrunReport {
    count: u32,
    worst: Duration,
}

/// Paces the control loop against an absolute timeline and reports overruns. The
/// deadline advances by exactly one period per tick, so the ~1 ms overshoot every
/// `tokio::time::sleep` incurs is corrected on the next cycle instead of
/// accumulating. A tick that finishes past its deadline is an overrun: the
/// timeline re-anchors to now (the next cycle starts immediately rather than
/// bursting to catch up) and the overrun is tallied for the periodic report.
pub struct Pacer {
    next_tick: tokio::time::Instant,
    period: Duration,
    overruns: u32,
    worst_late: Duration,
    /// Rate gate for the overrun report, so a loop overrunning every tick
    /// reports periodically rather than at tick rate.
    report_gate: Throttle,
}

impl Pacer {
    /// Create a pacer ticking at `period`, or [`ZeroPacerPeriod`] if the
    /// period is zero: a zero period makes every tick an overrun, so `pace()` would
    /// spin without awaiting.
    pub fn new(period: Duration) -> Result<Self, ZeroPacerPeriod> {
        if period.is_zero() {
            return Err(ZeroPacerPeriod);
        }
        Ok(Self {
            next_tick: tokio::time::Instant::now(),
            period,
            overruns: 0,
            worst_late: Duration::ZERO,
            report_gate: Throttle::started_at(OVERRUN_REPORT_PERIOD, Instant::now()),
        })
    }

    /// Sleep until the next tick deadline, tallying an overrun (and re-anchoring)
    /// when the loop body already blew past it.
    pub async fn pace(&mut self) {
        if let Some(deadline) = self.schedule(tokio::time::Instant::now()) {
            tokio::time::sleep_until(deadline).await;
        }
        if let Some(OverrunReport { count, worst }) = self.due_report(Instant::now()) {
            warn!(
                "{count} control-loop overrun(s) since last report (period {:?}, worst {worst:?} late)",
                self.period,
            );
        }
    }

    /// Advance the deadline by one period: the deadline to sleep until (the tick is
    /// on time when `now` is at or before it), or `None` on an overrun (`now` is
    /// already past it), in which case the overrun is tallied and the timeline
    /// re-anchors to `now`.
    fn schedule(&mut self, now: tokio::time::Instant) -> Option<tokio::time::Instant> {
        self.next_tick += self.period;
        if self.next_tick >= now {
            return Some(self.next_tick);
        }
        self.overruns += 1;
        self.worst_late = self.worst_late.max(now - self.next_tick);
        self.next_tick = now;
        None
    }

    /// The overrun tally to report, if any overruns accumulated and the report
    /// interval has elapsed; resets the tally. `None` otherwise.
    fn due_report(&mut self, now: Instant) -> Option<OverrunReport> {
        if self.overruns == 0 || !self.report_gate.admit_at(now) {
            return None;
        }
        let report = OverrunReport {
            count: self.overruns,
            worst: self.worst_late,
        };
        self.overruns = 0;
        self.worst_late = Duration::ZERO;
        Some(report)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PERIOD: Duration = Duration::from_millis(10);

    fn pacer_at(start: tokio::time::Instant, report_epoch: Instant) -> Pacer {
        Pacer {
            next_tick: start,
            period: PERIOD,
            overruns: 0,
            worst_late: Duration::ZERO,
            report_gate: Throttle::started_at(OVERRUN_REPORT_PERIOD, report_epoch),
        }
    }

    #[test]
    fn new_rejects_a_zero_period() {
        assert!(matches!(Pacer::new(Duration::ZERO), Err(ZeroPacerPeriod)));
    }

    #[test]
    fn schedule_keeps_absolute_timeline() {
        let start = tokio::time::Instant::now();
        let mut p = pacer_at(start, Instant::now());
        // On-time ticks: deadlines land exactly one period apart, regardless of
        // where within the cycle the tick finishes (sleep overshoot corrects).
        assert_eq!(p.schedule(start), Some(start + PERIOD));
        assert_eq!(p.schedule(start + PERIOD / 2), Some(start + 2 * PERIOD));
        assert_eq!(p.overruns, 0);
    }

    #[test]
    fn schedule_treats_an_exact_deadline_as_on_time() {
        let start = tokio::time::Instant::now();
        let mut p = pacer_at(start, Instant::now());
        // The tick finishes exactly on the deadline (now == next_tick after the
        // one-period advance): on time, not an overrun.
        assert_eq!(p.schedule(start + PERIOD), Some(start + PERIOD));
        assert_eq!(p.overruns, 0);
    }

    #[test]
    fn schedule_tallies_overrun_and_reanchors() {
        let start = tokio::time::Instant::now();
        let mut p = pacer_at(start, Instant::now());
        // The tick finishes half a period past its deadline: no sleep, tallied,
        // and the timeline restarts from the late finish.
        let late_finish = start + PERIOD + PERIOD / 2;
        assert_eq!(p.schedule(late_finish), None);
        assert_eq!(p.overruns, 1);
        assert_eq!(p.worst_late, PERIOD / 2);
        assert_eq!(p.schedule(late_finish), Some(late_finish + PERIOD));
    }

    #[test]
    fn schedule_keeps_worst_lateness() {
        let start = tokio::time::Instant::now();
        let mut p = pacer_at(start, Instant::now());
        assert_eq!(p.schedule(start + PERIOD + PERIOD / 2), None);
        assert_eq!(p.schedule(p.next_tick + PERIOD + PERIOD / 4), None);
        assert_eq!(p.overruns, 2);
        assert_eq!(p.worst_late, PERIOD / 2);
    }

    #[test]
    fn due_report_only_after_interval_with_overruns() {
        let start = tokio::time::Instant::now();
        let t0 = Instant::now();
        let mut p = pacer_at(start, t0);

        // No overruns: nothing to report even after the interval.
        assert_eq!(p.due_report(t0 + OVERRUN_REPORT_PERIOD), None);

        p.schedule(start + 2 * PERIOD); // one overrun
        // Interval not yet elapsed: deferred.
        assert_eq!(p.due_report(t0 + OVERRUN_REPORT_PERIOD / 2), None);
        // Interval elapsed: tally returned and reset.
        assert_eq!(
            p.due_report(t0 + OVERRUN_REPORT_PERIOD),
            Some(OverrunReport {
                count: 1,
                worst: PERIOD
            })
        );
        assert_eq!(p.overruns, 0);
        assert_eq!(p.due_report(t0 + 2 * OVERRUN_REPORT_PERIOD), None);
    }
}
