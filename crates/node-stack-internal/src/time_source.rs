//! The time that the supervision of the stack's instances runs on: the
//! cadence and the kill of the health monitor, the wait and the reset window
//! of the restart backoff, the force-kill deadline of a stop, and the bound on
//! the output readers of a start that ends before it runs. A daemon
//! reads it from the host ([`TimeSource::System`]). A test reads it from a
//! [`ManualTime`] that it advances itself, so a timer of the test's daemon
//! fires only when the test moves the time past it.

use std::collections::BTreeMap;
use std::future::Future;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::watch;

/// Where the supervision of a stack reads its time.
#[derive(Clone, Debug, Default)]
pub enum TimeSource {
    /// The monotonic clock of the host, with tokio's timers.
    #[default]
    System,
    /// A time that moves only when a test advances it.
    Manual(ManualTime),
}

impl TimeSource {
    /// The current moment. The host's time is tokio's, which its timers keep.
    pub fn now(&self) -> Instant {
        match self {
            Self::System => tokio::time::Instant::now().into_std(),
            Self::Manual(manual) => manual.now(),
        }
    }

    /// Waits until `deadline`; a deadline that passed ends the wait at once.
    pub async fn sleep_until(&self, deadline: Instant) {
        match self {
            Self::System => tokio::time::sleep_until(deadline.into()).await,
            Self::Manual(manual) => manual.sleep_until(deadline).await,
        }
    }

    /// Waits for `duration` from now. A duration that no moment of the
    /// clock reaches waits forever.
    pub async fn sleep(&self, duration: Duration) {
        self.sleep_from(self.now(), duration).await;
    }

    /// Waits until `duration` after `start`. A duration that no moment of the
    /// clock reaches waits forever.
    pub async fn sleep_from(&self, start: Instant, duration: Duration) {
        match start.checked_add(duration) {
            Some(deadline) => self.sleep_until(deadline).await,
            None => std::future::pending().await,
        }
    }

    /// The output of `future`, or `None` when `duration` passes before it
    /// ends. A future and a deadline that end together give the output.
    pub async fn timeout<F: Future>(&self, duration: Duration, future: F) -> Option<F::Output> {
        tokio::select! {
            biased;
            output = future => Some(output),
            () = self.sleep(duration) => None,
        }
    }

    /// The outcome of a request to a node or a peer daemon that waits at
    /// most `bound` for its answer: `request` takes the deadline it keeps
    /// itself. On the host's time it keeps `bound`. On a manual time it keeps
    /// none, so a request that reaches no service fails at once, and one
    /// whose answer does not come gives `elapsed` once the time moves past
    /// `bound`.
    pub async fn bounded<T, F>(
        &self,
        bound: Duration,
        request: impl FnOnce(Option<Duration>) -> F,
        elapsed: impl FnOnce() -> T,
    ) -> T
    where
        F: Future<Output = T>,
    {
        match self {
            Self::System => request(Some(bound)).await,
            Self::Manual(_) => self
                .timeout(bound, request(None))
                .await
                .unwrap_or_else(elapsed),
        }
    }
}

/// A time that moves only when [`ManualTime::advance`] moves it. A sleep on
/// it ends once the time reaches its deadline, and it reports the deadlines
/// of the sleeps that wait on it, so a test advances to a moment that it
/// knows the daemon waits for. Clones share one time.
#[derive(Clone, Debug)]
pub struct ManualTime {
    inner: Arc<ManualInner>,
}

#[derive(Debug)]
struct ManualInner {
    /// The moment the time starts at: the current moment is `origin` plus
    /// `elapsed`.
    origin: Instant,
    elapsed: watch::Sender<Duration>,
    /// The deadlines of the sleeps that wait, as times since `origin`.
    pending: watch::Sender<PendingSleeps>,
}

#[derive(Debug, Default)]
struct PendingSleeps {
    next_id: u64,
    deadlines: BTreeMap<u64, Duration>,
}

impl PendingSleeps {
    /// The time left until each deadline once `elapsed` has passed, the
    /// shortest first.
    fn left_at(&self, elapsed: Duration) -> Vec<Duration> {
        let mut left: Vec<Duration> = self
            .deadlines
            .values()
            .map(|deadline| deadline.saturating_sub(elapsed))
            .collect();
        left.sort();
        left
    }
}

impl Default for ManualTime {
    fn default() -> Self {
        Self::new()
    }
}

impl ManualTime {
    /// A time that starts at the current moment of the host.
    pub fn new() -> Self {
        Self {
            inner: Arc::new(ManualInner {
                origin: Instant::now(),
                elapsed: watch::Sender::new(Duration::ZERO),
                pending: watch::Sender::new(PendingSleeps::default()),
            }),
        }
    }

    /// The current moment.
    pub fn now(&self) -> Instant {
        self.inner.origin + *self.inner.elapsed.borrow()
    }

    /// Moves the time `by` forward, which ends each sleep whose deadline it
    /// reaches.
    pub fn advance(&self, by: Duration) {
        self.inner
            .elapsed
            .send_modify(|elapsed| *elapsed = elapsed.saturating_add(by));
    }

    /// The time left until the deadline of each sleep that waits, the
    /// shortest first.
    pub fn pending(&self) -> Vec<Duration> {
        self.inner
            .pending
            .borrow()
            .left_at(*self.inner.elapsed.borrow())
    }

    /// Waits until a sleep with `left` to go waits on this time.
    pub async fn sleeping_for(&self, left: Duration) {
        self.sleeping(1, left).await;
    }

    /// Waits until `count` sleeps or more with `left` to go wait on this
    /// time.
    pub async fn sleeping(&self, count: usize, left: Duration) {
        let mut pending = self.inner.pending.subscribe();
        let _ = pending
            .wait_for(|pending| {
                pending
                    .left_at(*self.inner.elapsed.borrow())
                    .iter()
                    .filter(|sleep| **sleep == left)
                    .count()
                    >= count
            })
            .await;
    }

    async fn sleep_until(&self, deadline: Instant) {
        let due = deadline.saturating_duration_since(self.inner.origin);
        let mut elapsed = self.inner.elapsed.subscribe();
        if *elapsed.borrow_and_update() >= due {
            return;
        }
        let _waiting = PendingSleep::register(&self.inner, due);
        let _ = elapsed.wait_for(|elapsed| *elapsed >= due).await;
    }
}

/// The deadline of one sleep in the pending sleeps of its time, from its
/// first poll until it ends or is dropped.
struct PendingSleep {
    inner: Arc<ManualInner>,
    id: u64,
}

impl PendingSleep {
    fn register(inner: &Arc<ManualInner>, due: Duration) -> Self {
        let mut id = 0;
        inner.pending.send_modify(|pending| {
            id = pending.next_id;
            pending.next_id += 1;
            pending.deadlines.insert(id, due);
        });
        Self {
            inner: Arc::clone(inner),
            id,
        }
    }
}

impl Drop for PendingSleep {
    fn drop(&mut self) {
        self.inner.pending.send_modify(|pending| {
            pending.deadlines.remove(&self.id);
        });
    }
}

#[cfg(test)]
mod tests {
    use super::{ManualTime, TimeSource};
    use std::time::Duration;

    const SECOND: Duration = Duration::from_secs(1);

    #[tokio::test]
    async fn a_manual_sleep_ends_once_the_time_reaches_its_deadline() {
        let manual = ManualTime::new();
        let time = TimeSource::Manual(manual.clone());
        let sleep = tokio::spawn(async move { time.sleep(5 * SECOND).await });
        manual.sleeping_for(5 * SECOND).await;

        manual.advance(4 * SECOND);
        assert_eq!(
            manual.pending(),
            [SECOND],
            "the sleep waits for the second it has left"
        );
        manual.advance(SECOND);
        sleep.await.expect("the sleep ends");
        assert!(manual.pending().is_empty());
    }

    #[tokio::test]
    async fn the_pending_sleeps_are_the_time_left_the_shortest_first() {
        let manual = ManualTime::new();
        let time = TimeSource::Manual(manual.clone());
        let sleeps: Vec<_> = [3, 1, 2]
            .into_iter()
            .map(|secs| {
                let time = time.clone();
                tokio::spawn(async move { time.sleep(secs * SECOND).await })
            })
            .collect();
        for secs in 1..=3 {
            manual.sleeping_for(secs * SECOND).await;
        }
        assert_eq!(manual.pending(), [SECOND, 2 * SECOND, 3 * SECOND]);

        manual.advance(SECOND);
        let [longest, shortest, middle] = <[_; 3]>::try_from(sleeps).expect("three sleeps");
        shortest.await.expect("the shortest sleep ends");
        assert_eq!(manual.pending(), [SECOND, 2 * SECOND]);
        manual.advance(2 * SECOND);
        middle.await.expect("the middle sleep ends");
        longest.await.expect("the longest sleep ends");
    }

    #[tokio::test]
    async fn a_test_waits_for_several_sleeps_with_the_same_time_left() {
        let manual = ManualTime::new();
        let time = TimeSource::Manual(manual.clone());
        let sleeps: Vec<_> = (0..2)
            .map(|_| {
                let time = time.clone();
                tokio::spawn(async move { time.sleep(SECOND).await })
            })
            .collect();
        manual.sleeping(2, SECOND).await;
        assert_eq!(manual.pending(), [SECOND, SECOND]);

        manual.advance(SECOND);
        for sleep in sleeps {
            sleep.await.expect("the sleep ends");
        }
    }

    #[tokio::test]
    async fn a_dropped_sleep_leaves_the_pending_sleeps() {
        let manual = ManualTime::new();
        let time = TimeSource::Manual(manual.clone());
        let sleep = tokio::spawn(async move { time.sleep(SECOND).await });
        manual.sleeping_for(SECOND).await;
        sleep.abort();
        let _ = sleep.await;
        assert!(manual.pending().is_empty());
    }

    #[tokio::test]
    async fn a_deadline_the_time_passed_ends_the_sleep_at_once() {
        let manual = ManualTime::new();
        let time = TimeSource::Manual(manual.clone());
        let start = time.now();
        manual.advance(2 * SECOND);
        assert_eq!(time.now() - start, 2 * SECOND);
        time.sleep_until(start + SECOND).await;
        time.sleep(Duration::ZERO).await;
        assert!(manual.pending().is_empty(), "no ended sleep stays pending");
    }

    #[tokio::test]
    async fn a_manual_timeout_gives_the_output_or_ends_at_its_deadline() {
        let manual = ManualTime::new();
        let time = TimeSource::Manual(manual.clone());
        assert_eq!(time.timeout(SECOND, async { 7 }).await, Some(7));

        let unanswered = tokio::spawn({
            let time = time.clone();
            async move { time.timeout(SECOND, std::future::pending::<()>()).await }
        });
        manual.sleeping_for(SECOND).await;
        manual.advance(SECOND);
        assert_eq!(unanswered.await.expect("the timeout ends"), None);
    }

    #[tokio::test]
    async fn a_bounded_request_keeps_the_bound_on_the_host_time() {
        let time = TimeSource::System;
        let kept = time
            .bounded(SECOND, |bound| async move { bound }, || None)
            .await;
        assert_eq!(kept, Some(SECOND));
    }

    #[tokio::test]
    async fn a_bounded_request_on_manual_time_ends_by_its_answer_or_the_time() {
        let manual = ManualTime::new();
        let time = TimeSource::Manual(manual.clone());
        let answered = time
            .bounded(SECOND, |bound| async move { Ok(bound) }, || Err("elapsed"))
            .await;
        assert_eq!(answered, Ok(None), "the request keeps no deadline itself");

        let unanswered = tokio::spawn({
            let time = time.clone();
            async move {
                time.bounded(
                    SECOND,
                    |_| std::future::pending::<Result<(), &str>>(),
                    || Err("elapsed"),
                )
                .await
            }
        });
        manual.sleeping_for(SECOND).await;
        manual.advance(SECOND);
        assert_eq!(unanswered.await.expect("the request ends"), Err("elapsed"));
    }

    #[tokio::test(start_paused = true)]
    async fn the_system_time_runs_on_tokio_timers() {
        let time = TimeSource::System;
        let start = time.now();
        time.sleep(SECOND).await;
        assert!(time.now() - start >= SECOND);
        assert_eq!(
            time.timeout(SECOND, std::future::pending::<()>()).await,
            None
        );
        assert_eq!(time.timeout(SECOND, async { 7 }).await, Some(7));
    }

    /// The paused tokio clock jumps to each timer it has, so a sleep with a
    /// deadline would end within the day; the endless sleep registers none.
    #[tokio::test(start_paused = true)]
    async fn a_duration_that_no_moment_reaches_waits_forever() {
        let endless = TimeSource::System.sleep(Duration::MAX);
        let day = 24 * 3600 * SECOND;
        assert!(tokio::time::timeout(day, endless).await.is_err());
    }
}
