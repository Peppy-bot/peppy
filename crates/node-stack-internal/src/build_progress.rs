//! Synthetic build-progress feedback derived from disk growth.
//!
//! During `apptainer build`, a docker base image download is mostly silent
//! off-TTY: one "Copying blob …" line per blob, then nothing while multi-GB
//! blobs stream into apptainer's cache, and nothing again while the SIF is
//! assembled. The daemon's per-phase idle watchdog and the CLI watchdog both
//! reset only when a line flows through the feedback channel, so that silence
//! reads as "idle" and a slow-but-progressing build gets killed.
//!
//! [`BuildProgressMonitor`] closes that gap: it samples the total on-disk
//! footprint of every surface the build writes to and emits a feedback line
//! **only when the total grew**. A wedged apptainer moves no bytes, produces
//! no lines, and still trips the idle timeout exactly as it does today; the
//! monitor can defer the timeout only while real bytes are landing on disk,
//! never neuter it.

use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::build_io::{FeedbackLine, FeedbackStream, format_bytes};

/// Cadence of usage samples. Each tick is one blocking probe (a filesystem
/// walk; a `limactl shell du` subprocess under Lima), so the interval also
/// bounds the probe overhead.
pub(crate) const BUILD_PROGRESS_SAMPLE_INTERVAL: Duration = Duration::from_secs(5);

/// Byte-count sampler the monitor polls each tick. A closure rather than a
/// concrete probe type so tests can drive the monitor with a synthetic
/// counter; production passes `containers::CacheUsageProbe::usage_bytes`.
/// Arc'd internally so [`sample`] can clone it into `spawn_blocking`.
type UsageSampler = Arc<dyn Fn() -> u64 + Send + Sync>;

/// Guard around the sampling task: dropping it aborts the task, so tying it to
/// the build future's stack scopes the monitor to `stream_child_output` — the
/// phase runner dropping the future on idle timeout, a cancelled `--force`
/// supersede, and normal completion all tear it down the same way.
pub(crate) struct BuildProgressMonitor {
    task: JoinHandle<()>,
}

impl BuildProgressMonitor {
    /// Starts the sampling task: a baseline sample immediately, then one
    /// sample per [`BUILD_PROGRESS_SAMPLE_INTERVAL`], emitting a stdout
    /// `FeedbackLine` only on growth since the last sample. Each sample runs
    /// on the blocking pool (the probe walks filesystems and may shell out).
    pub(crate) fn spawn(
        sampler: impl Fn() -> u64 + Send + Sync + 'static,
        feedback_tx: mpsc::UnboundedSender<FeedbackLine>,
    ) -> Self {
        let sampler: UsageSampler = Arc::new(sampler);
        let task = tokio::spawn(async move {
            // Baseline before the first tick, so the first line reports growth
            // since the build started rather than the preexisting cache size.
            let Some(mut last_total) = sample(&sampler).await else {
                return;
            };
            loop {
                tokio::time::sleep(BUILD_PROGRESS_SAMPLE_INTERVAL).await;
                let Some(total) = sample(&sampler).await else {
                    return;
                };
                if total > last_total {
                    let line = format!(
                        "Fetching/assembling container image: {} written (+{})",
                        format_bytes(total),
                        format_bytes(total - last_total),
                    );
                    if feedback_tx
                        .send(FeedbackLine {
                            stream: FeedbackStream::Stdout,
                            line,
                        })
                        .is_err()
                    {
                        // Channel closed: the build is over; stop sampling.
                        return;
                    }
                }
                // A shrink (cache cleanup) emits nothing but still rebases the
                // total, so growth after it is measured from the new floor.
                last_total = total;
            }
        });
        Self { task }
    }
}

impl Drop for BuildProgressMonitor {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// One blocking probe call on the blocking pool. `None` when the runtime is
/// shutting down (join error), which ends the monitor quietly.
async fn sample(sampler: &UsageSampler) -> Option<u64> {
    let sampler = Arc::clone(sampler);
    tokio::task::spawn_blocking(move || sampler()).await.ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// Sampler over a shared counter that also counts its own invocations, so
    /// tests can wait for the baseline sample before mutating the total
    /// (mutating earlier would fold the "growth" into the baseline).
    fn counter_sampler(
        bytes: &Arc<AtomicU64>,
        calls: &Arc<AtomicU64>,
    ) -> impl Fn() -> u64 + Send + Sync + 'static {
        let bytes = Arc::clone(bytes);
        let calls = Arc::clone(calls);
        move || {
            calls.fetch_add(1, Ordering::SeqCst);
            bytes.load(Ordering::SeqCst)
        }
    }

    /// Parks the test until the sampler has run at least `at_least` times.
    /// The 1 ms paused-clock sleeps yield to the scheduler while the blocking
    /// pool finishes the sample in real time.
    async fn wait_for_calls(calls: &Arc<AtomicU64>, at_least: u64) {
        while calls.load(Ordering::SeqCst) < at_least {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    }

    const INTERVAL: Duration = BUILD_PROGRESS_SAMPLE_INTERVAL;

    #[tokio::test(start_paused = true)]
    async fn emits_on_growth_and_stays_silent_when_flat() {
        let bytes = Arc::new(AtomicU64::new(1_000));
        let calls = Arc::new(AtomicU64::new(0));
        let (tx, mut rx) = mpsc::unbounded_channel();
        let _monitor = BuildProgressMonitor::spawn(counter_sampler(&bytes, &calls), tx);
        wait_for_calls(&calls, 1).await;

        // Growth after the baseline produces a line reporting the delta.
        bytes.store(300 * 1024 * 1024 + 1_000, Ordering::SeqCst);
        let line = tokio::time::timeout(INTERVAL * 3, rx.recv())
            .await
            .expect("a growth tick must emit within an interval")
            .expect("channel open");
        assert!(matches!(line.stream, FeedbackStream::Stdout));
        assert!(
            line.line.contains("(+300.0 MB)"),
            "the line must report the growth delta, got: {}",
            line.line
        );

        // Flat samples emit nothing: several intervals pass in silence.
        tokio::time::sleep(INTERVAL * 4).await;
        assert!(
            rx.try_recv().is_err(),
            "a flat total must not emit progress lines"
        );

        // Growth resumes → another line.
        bytes.fetch_add(1024 * 1024, Ordering::SeqCst);
        let line = tokio::time::timeout(INTERVAL * 3, rx.recv())
            .await
            .expect("growth after a flat stretch must emit again")
            .expect("channel open");
        assert!(line.line.contains("(+1.0 MB)"), "got: {}", line.line);
    }

    #[tokio::test(start_paused = true)]
    async fn a_shrink_emits_nothing_and_rebases_the_total() {
        let bytes = Arc::new(AtomicU64::new(10 * 1024 * 1024));
        let calls = Arc::new(AtomicU64::new(0));
        let (tx, mut rx) = mpsc::unbounded_channel();
        let _monitor = BuildProgressMonitor::spawn(counter_sampler(&bytes, &calls), tx);
        wait_for_calls(&calls, 1).await;

        // Shrink (e.g. cache cleanup): no line, but only assert once at least
        // one post-shrink sample has actually run.
        bytes.store(1024 * 1024, Ordering::SeqCst);
        let seen = calls.load(Ordering::SeqCst);
        wait_for_calls(&calls, seen + 2).await;
        assert!(rx.try_recv().is_err(), "a shrink must not emit");

        // Growth from the new floor emits, measured from the rebased total.
        bytes.store(3 * 1024 * 1024, Ordering::SeqCst);
        let line = tokio::time::timeout(INTERVAL * 3, rx.recv())
            .await
            .expect("growth after a shrink must emit")
            .expect("channel open");
        assert!(line.line.contains("(+2.0 MB)"), "got: {}", line.line);
    }

    #[tokio::test(start_paused = true)]
    async fn dropping_the_guard_aborts_the_sampling_task() {
        let (tx, mut rx) = mpsc::unbounded_channel::<FeedbackLine>();
        let monitor = BuildProgressMonitor::spawn(|| 0, tx);
        drop(monitor);
        // The aborted task drops its sender, so the channel closes; a live
        // task would hold it open forever.
        let closed = tokio::time::timeout(INTERVAL * 12, rx.recv())
            .await
            .expect("the channel must close once the guard is dropped");
        assert!(closed.is_none());
    }
}
