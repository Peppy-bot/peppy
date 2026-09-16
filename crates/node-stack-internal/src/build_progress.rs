//! Synthetic build-progress feedback derived from the work a build does.
//!
//! A build is mostly silent off-TTY. `apptainer build` prints one "Copying
//! blob …" line per blob of a docker base image, then nothing while multi-GB
//! blobs stream into its cache, and nothing again while the SIF is assembled.
//! A compiler run by a `%post` or a `build_cmd` prints one line when it starts
//! a crate and then holds the CPU without a word until the crate is done, for
//! minutes on a size-optimized link. The daemon's per-phase idle watchdog and
//! the CLI watchdog both reset only when a line flows through the feedback
//! channel, so that silence reads as "idle" and a slow-but-working build gets
//! killed.
//!
//! [`BuildProgressMonitor`] closes that gap: it samples the build's activity
//! (the on-disk footprint of every surface it writes to and the CPU time its
//! processes have consumed, see `containers::BuildActivityProbe`) and emits a
//! feedback line **only when the build did work**: bytes landed on disk, or
//! its processes consumed [`CPU_ACTIVITY_THRESHOLD`] of CPU since the last
//! line that reported CPU. A wedged build moves no bytes, burns no CPU,
//! produces no lines, and trips the idle timeout exactly as it would without
//! the monitor; the monitor can defer the timeout only while real work
//! happens, never neuter it.
//!
//! Work alone does not say which step does it, and after a few quiet
//! intervals the line the step printed has scrolled away behind progress
//! lines. Once the build has printed nothing for [`NAME_LAST_OUTPUT_AFTER`],
//! each line the monitor emits also names the last line the build printed
//! and how long ago, as [`LastOutputLine`] recorded it. That context rides on
//! a line the work already earned and never earns one by itself.

use containers::BuildActivity;
use parking_lot::Mutex;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::Instant;

use crate::build_io::{FeedbackLine, FeedbackStream, OutputReaderHooks, format_bytes};

/// Cadence of activity samples. Each tick is one blocking probe (filesystem
/// walks and a `/proc` scan or a `ps` listing; a `limactl shell` subprocess
/// under Lima), so the interval also bounds the probe overhead.
pub(crate) const BUILD_PROGRESS_SAMPLE_INTERVAL: Duration = Duration::from_secs(5);

/// The CPU time the build's processes must accrue, since the last line that
/// reported CPU, for a tick to report it: one fifth of a core over a tick. A
/// compiler holds a core outright and clears it every tick; a process blocked
/// on a lock, a socket or a pipe never does. Sub-threshold slices accumulate
/// across ticks rather than being dropped, so a build merely starved of CPU
/// keeps reporting too, just less often.
const CPU_ACTIVITY_THRESHOLD: Duration = Duration::from_secs(1);

/// How long a build must have printed nothing before its progress lines name
/// the last line it printed: one sample interval. A progress line emitted
/// sooner sits right below that line; one emitted later may sit below the
/// previous progress line instead.
const NAME_LAST_OUTPUT_AFTER: Duration = BUILD_PROGRESS_SAMPLE_INTERVAL;

/// The most characters of the last output line a progress line repeats. A
/// longer line (a command echoed with all its flags, a compiler invocation)
/// is cut there and marked with `...`, so a line repeated every tick stays
/// readable.
const LAST_OUTPUT_MAX_CHARS: usize = 120;

/// Activity sampler the monitor polls each tick. A closure rather than a
/// concrete probe type so tests can drive the monitor with synthetic
/// counters; production passes `containers::BuildActivityProbe::sample`.
/// Arc'd internally so [`sample`] can clone it into `spawn_blocking`.
type ActivitySampler = Arc<dyn Fn() -> BuildActivity + Send + Sync>;

/// The last line a build printed and when it arrived. The build's output
/// readers record every line they forward through their
/// [`OutputReaderHooks`], and its [`BuildProgressMonitor`] names that line
/// once the build has gone quiet. Clones share one record.
#[derive(Clone, Default)]
pub(crate) struct LastOutputLine {
    recorded: Arc<Mutex<Option<RecordedLine>>>,
}

struct RecordedLine {
    text: String,
    printed_at: Instant,
}

/// The last line a quiet build printed, and how long it has been quiet since.
#[derive(Debug, PartialEq, Eq)]
struct QuietOutput {
    text: String,
    quiet_for: Duration,
}

impl LastOutputLine {
    /// Records `line` as the last one the build printed, now. The line is
    /// trimmed (a compiler right-aligns its status words) and cut to
    /// [`LAST_OUTPUT_MAX_CHARS`]; a blank line names nothing, so it leaves the
    /// previous record in place.
    pub(crate) fn record(&self, line: &str) {
        let line = line.trim();
        if line.is_empty() {
            return;
        }
        let text = match line.char_indices().nth(LAST_OUTPUT_MAX_CHARS) {
            Some((cut, _)) => format!("{}...", &line[..cut]),
            None => line.to_owned(),
        };
        *self.recorded.lock() = Some(RecordedLine {
            text,
            printed_at: Instant::now(),
        });
    }

    /// The recorded line and how long ago it was printed, as of `now`, once
    /// the build has printed nothing for [`NAME_LAST_OUTPUT_AFTER`]. `None`
    /// before the build printed a line, and while it prints.
    fn quiet_output(&self, now: Instant) -> Option<QuietOutput> {
        let recorded = self.recorded.lock();
        let recorded = recorded.as_ref()?;
        let quiet_for = now.saturating_duration_since(recorded.printed_at);
        (quiet_for >= NAME_LAST_OUTPUT_AFTER).then(|| QuietOutput {
            text: recorded.text.clone(),
            quiet_for,
        })
    }
}

impl OutputReaderHooks for LastOutputLine {
    fn on_line(&self, line: &str) {
        self.record(line);
    }
}

/// Guard around the sampling task: dropping it aborts the task, so tying it to
/// the build future's stack scopes the monitor to `stream_child_output`: the
/// phase runner dropping the future on idle timeout, a cancelled `--force`
/// supersede, and normal completion all tear it down the same way.
pub(crate) struct BuildProgressMonitor {
    task: JoinHandle<()>,
}

impl BuildProgressMonitor {
    /// Starts the sampling task: a baseline sample immediately, then one
    /// sample per [`BUILD_PROGRESS_SAMPLE_INTERVAL`], emitting a stdout
    /// `FeedbackLine` only for the ticks [`ProgressTracker`] deems work, each
    /// naming `last_output` once the build has gone quiet. Each sample runs on
    /// the blocking pool (the probe walks filesystems and may shell out).
    pub(crate) fn spawn(
        sampler: impl Fn() -> BuildActivity + Send + Sync + 'static,
        last_output: LastOutputLine,
        feedback_tx: mpsc::UnboundedSender<FeedbackLine>,
    ) -> Self {
        let sampler: ActivitySampler = Arc::new(sampler);
        let task = tokio::spawn(async move {
            // Baseline before the first tick, so the first line reports work
            // since the build started rather than the preexisting cache size
            // and whatever the build's leader consumed before.
            let Some(baseline) = sample(&sampler).await else {
                return;
            };
            let mut tracker = ProgressTracker::new(baseline);
            loop {
                tokio::time::sleep(BUILD_PROGRESS_SAMPLE_INTERVAL).await;
                let Some(activity) = sample(&sampler).await else {
                    return;
                };
                let quiet_output = last_output.quiet_output(Instant::now());
                let Some(line) = tracker.observe(activity, quiet_output.as_ref()) else {
                    continue;
                };
                let line = FeedbackLine {
                    stream: FeedbackStream::Stdout,
                    line,
                };
                if feedback_tx.send(line).is_err() {
                    // Channel closed: the build is over; stop sampling.
                    return;
                }
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
async fn sample(sampler: &ActivitySampler) -> Option<BuildActivity> {
    let sampler = Arc::clone(sampler);
    tokio::task::spawn_blocking(move || sampler()).await.ok()
}

/// The bookkeeping between samples: what of a sample counts as work since the
/// last one, and the line reporting it.
struct ProgressTracker {
    /// The previous sample's footprint. Growth is measured from it; a shrink
    /// (cache cleanup) reports nothing but rebases it, so growth after the
    /// shrink is measured from the new floor.
    bytes_on_disk: u64,
    /// The CPU time at the last line that reported CPU, so slices under
    /// [`CPU_ACTIVITY_THRESHOLD`] accumulate across ticks. A drop below it
    /// (time reaped outside the build's processes) rebases it the way a
    /// shrink does.
    cpu_reported: Duration,
}

impl ProgressTracker {
    fn new(baseline: BuildActivity) -> Self {
        Self {
            bytes_on_disk: baseline.bytes_on_disk,
            cpu_reported: baseline.cpu_time,
        }
    }

    /// Folds one sample in and returns the feedback line it earns, if any,
    /// naming `quiet_output` when the build has gone quiet.
    fn observe(
        &mut self,
        activity: BuildActivity,
        quiet_output: Option<&QuietOutput>,
    ) -> Option<String> {
        let bytes_written = activity.bytes_on_disk.saturating_sub(self.bytes_on_disk);
        self.bytes_on_disk = activity.bytes_on_disk;
        self.cpu_reported = self.cpu_reported.min(activity.cpu_time);
        let cpu_accrued = activity.cpu_time - self.cpu_reported;
        let cpu_burned = (cpu_accrued >= CPU_ACTIVITY_THRESHOLD).then_some(cpu_accrued);
        if cpu_burned.is_some() {
            self.cpu_reported = activity.cpu_time;
        }
        progress_line(activity, bytes_written, cpu_burned, quiet_output)
    }
}

/// The line reporting a tick's work: each total with the tick's share in
/// parentheses, then the last output of a quiet build, e.g. `Build progress:
/// 1.9 GB written (+320.0 MB), CPU time 5m14s (+5.0s), last output 3m07s ago:
/// Compiling viewer v0.1.0`. `None` when neither bytes nor CPU moved, whatever
/// the output.
fn progress_line(
    activity: BuildActivity,
    bytes_written: u64,
    cpu_burned: Option<Duration>,
    quiet_output: Option<&QuietOutput>,
) -> Option<String> {
    let mut parts = Vec::with_capacity(3);
    if bytes_written > 0 {
        parts.push(format!(
            "{} written (+{})",
            format_bytes(activity.bytes_on_disk),
            format_bytes(bytes_written),
        ));
    }
    if let Some(burned) = cpu_burned {
        parts.push(format!(
            "CPU time {} (+{})",
            format_duration(activity.cpu_time),
            format_duration(burned),
        ));
    }
    if parts.is_empty() {
        return None;
    }
    if let Some(quiet_output) = quiet_output {
        parts.push(format!(
            "last output {} ago: {}",
            format_duration(quiet_output.quiet_for),
            quiet_output.text,
        ));
    }
    Some(format!("Build progress: {}", parts.join(", ")))
}

/// `1h02m03s` from an hour up, `5m14s` from a minute up, `4.9s` below.
fn format_duration(time: Duration) -> String {
    let secs = time.as_secs();
    match secs {
        3600.. => format!("{}h{:02}m{:02}s", secs / 3600, secs % 3600 / 60, secs % 60),
        60.. => format!("{}m{:02}s", secs / 60, secs % 60),
        _ => format!("{:.1}s", time.as_secs_f64()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    const MB: u64 = 1024 * 1024;

    fn activity(bytes_on_disk: u64, cpu_time: Duration) -> BuildActivity {
        BuildActivity {
            bytes_on_disk,
            cpu_time,
        }
    }

    #[test]
    fn tracker_reports_bytes_written_since_the_previous_sample() {
        let mut tracker = ProgressTracker::new(activity(1_000, Duration::ZERO));
        let line = tracker
            .observe(activity(300 * MB + 1_000, Duration::ZERO), None)
            .expect("growth earns a line");
        assert_eq!(line, "Build progress: 300.0 MB written (+300.0 MB)");
        assert_eq!(
            tracker.observe(activity(300 * MB + 1_000, Duration::ZERO), None),
            None,
            "a flat sample earns nothing"
        );
    }

    #[test]
    fn tracker_reports_a_shrink_as_nothing_and_measures_growth_from_the_new_floor() {
        let mut tracker = ProgressTracker::new(activity(10 * MB, Duration::ZERO));
        assert_eq!(tracker.observe(activity(MB, Duration::ZERO), None), None);
        let line = tracker
            .observe(activity(3 * MB, Duration::ZERO), None)
            .expect("growth after a shrink earns a line");
        assert_eq!(line, "Build progress: 3.0 MB written (+2.0 MB)");
    }

    #[test]
    fn tracker_reports_cpu_time_once_a_threshold_worth_accrued() {
        let mut tracker = ProgressTracker::new(activity(MB, Duration::ZERO));
        let line = tracker
            .observe(activity(MB, Duration::from_secs(5)), None)
            .expect("a core held for a tick earns a line");
        assert_eq!(line, "Build progress: CPU time 5.0s (+5.0s)");
    }

    #[test]
    fn tracker_accumulates_sub_threshold_cpu_across_ticks() {
        let mut tracker = ProgressTracker::new(activity(0, Duration::ZERO));
        assert_eq!(
            tracker.observe(activity(0, Duration::from_millis(400)), None),
            None
        );
        assert_eq!(
            tracker.observe(activity(0, Duration::from_millis(800)), None),
            None
        );
        let line = tracker
            .observe(activity(0, Duration::from_millis(1_200)), None)
            .expect("the accrued slices cross the threshold together");
        assert_eq!(line, "Build progress: CPU time 1.2s (+1.2s)");
        assert_eq!(
            tracker.observe(activity(0, Duration::from_millis(1_500)), None),
            None,
            "the report rebased the accrual"
        );
    }

    #[test]
    fn tracker_rebases_after_cpu_time_drops() {
        let mut tracker = ProgressTracker::new(activity(0, Duration::from_secs(10)));
        assert_eq!(
            tracker.observe(activity(0, Duration::from_secs(4)), None),
            None
        );
        let line = tracker
            .observe(activity(0, Duration::from_secs(5)), None)
            .expect("growth from the new floor earns a line");
        assert_eq!(line, "Build progress: CPU time 5.0s (+1.0s)");
    }

    #[test]
    fn tracker_reports_both_signals_in_one_line() {
        let mut tracker = ProgressTracker::new(activity(0, Duration::ZERO));
        let line = tracker
            .observe(activity(MB, Duration::from_secs(2)), None)
            .expect("work earns a line");
        assert_eq!(
            line,
            "Build progress: 1.0 MB written (+1.0 MB), CPU time 2.0s (+2.0s)"
        );
    }

    #[test]
    fn tracker_names_the_last_output_of_a_quiet_build() {
        let mut tracker = ProgressTracker::new(activity(0, Duration::ZERO));
        let quiet_output = QuietOutput {
            text: "Compiling viewer v0.1.0 (/opt/waldo/crates/viewer)".to_string(),
            quiet_for: Duration::from_secs(187),
        };
        let line = tracker
            .observe(activity(0, Duration::from_secs(5)), Some(&quiet_output))
            .expect("work earns a line");
        assert_eq!(
            line,
            "Build progress: CPU time 5.0s (+5.0s), last output 3m07s ago: \
             Compiling viewer v0.1.0 (/opt/waldo/crates/viewer)"
        );
    }

    #[test]
    fn tracker_earns_no_line_for_the_last_output_alone() {
        let mut tracker = ProgressTracker::new(activity(MB, Duration::from_secs(1)));
        let quiet_output = QuietOutput {
            text: "Compiling viewer v0.1.0".to_string(),
            quiet_for: Duration::from_secs(60),
        };
        assert_eq!(
            tracker.observe(activity(MB, Duration::from_secs(1)), Some(&quiet_output)),
            None,
            "a quiet build that did no work stays silent, so it still times out"
        );
    }

    #[test]
    fn durations_format_by_magnitude() {
        assert_eq!(format_duration(Duration::from_millis(4_940)), "4.9s");
        assert_eq!(format_duration(Duration::from_secs(314)), "5m14s");
        assert_eq!(format_duration(Duration::from_secs(3_723)), "1h02m03s");
    }

    #[tokio::test(start_paused = true)]
    async fn last_output_is_named_once_the_build_was_quiet_for_an_interval() {
        let last_output = LastOutputLine::default();
        assert_eq!(
            last_output.quiet_output(Instant::now() + NAME_LAST_OUTPUT_AFTER),
            None,
            "a build that printed nothing has no last output"
        );

        // The paused clock does not move between the record and `printed_at`.
        let printed_at = Instant::now();
        last_output.record("   Compiling viewer v0.1.0 (/opt/waldo/crates/viewer)");
        assert_eq!(
            last_output
                .quiet_output(printed_at + NAME_LAST_OUTPUT_AFTER - Duration::from_millis(1)),
            None,
            "a progress line this soon sits right below the output line"
        );
        assert_eq!(
            last_output.quiet_output(printed_at + NAME_LAST_OUTPUT_AFTER),
            Some(QuietOutput {
                text: "Compiling viewer v0.1.0 (/opt/waldo/crates/viewer)".to_string(),
                quiet_for: NAME_LAST_OUTPUT_AFTER,
            })
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_blank_line_leaves_the_last_output_in_place() {
        let last_output = LastOutputLine::default();
        let printed_at = Instant::now();
        last_output.record("INFO:    Creating SIF file...");
        tokio::time::advance(Duration::from_secs(30)).await;
        last_output.record("");
        last_output.record(" \t ");
        assert_eq!(
            last_output.quiet_output(printed_at + Duration::from_secs(60)),
            Some(QuietOutput {
                text: "INFO:    Creating SIF file...".to_string(),
                quiet_for: Duration::from_secs(60),
            })
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_long_output_line_is_cut_at_the_maximum() {
        let last_output = LastOutputLine::default();
        let later = Instant::now() + NAME_LAST_OUTPUT_AFTER;
        let text_at = |line: &str| {
            last_output.record(line);
            last_output
                .quiet_output(later)
                .expect("the line was recorded")
                .text
        };

        let at_the_maximum = "x".repeat(LAST_OUTPUT_MAX_CHARS);
        assert_eq!(text_at(&at_the_maximum), at_the_maximum);
        assert_eq!(
            text_at(&"x".repeat(LAST_OUTPUT_MAX_CHARS + 1)),
            format!("{at_the_maximum}...")
        );
        // Characters are counted, not bytes, so a multi-byte one is never split.
        assert_eq!(
            text_at(&"é".repeat(LAST_OUTPUT_MAX_CHARS + 5)),
            format!("{}...", "é".repeat(LAST_OUTPUT_MAX_CHARS))
        );
    }

    /// Sampler over shared counters that also counts its own invocations, so
    /// tests can wait for the baseline sample before mutating the counters
    /// (mutating earlier would fold the "growth" into the baseline).
    fn counter_sampler(
        bytes: &Arc<AtomicU64>,
        cpu_millis: &Arc<AtomicU64>,
        calls: &Arc<AtomicU64>,
    ) -> impl Fn() -> BuildActivity + Send + Sync + 'static {
        let bytes = Arc::clone(bytes);
        let cpu_millis = Arc::clone(cpu_millis);
        let calls = Arc::clone(calls);
        move || {
            calls.fetch_add(1, Ordering::SeqCst);
            activity(
                bytes.load(Ordering::SeqCst),
                Duration::from_millis(cpu_millis.load(Ordering::SeqCst)),
            )
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
        let cpu_millis = Arc::new(AtomicU64::new(0));
        let calls = Arc::new(AtomicU64::new(0));
        let (tx, mut rx) = mpsc::unbounded_channel();
        let _monitor = BuildProgressMonitor::spawn(
            counter_sampler(&bytes, &cpu_millis, &calls),
            LastOutputLine::default(),
            tx,
        );
        wait_for_calls(&calls, 1).await;

        // Growth after the baseline produces a line reporting the delta.
        bytes.store(300 * MB + 1_000, Ordering::SeqCst);
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

        // CPU time accruing on a flat disk is work too.
        cpu_millis.store(5_000, Ordering::SeqCst);
        let line = tokio::time::timeout(INTERVAL * 3, rx.recv())
            .await
            .expect("a CPU tick must emit within an interval")
            .expect("channel open");
        assert!(
            line.line.contains("CPU time 5.0s (+5.0s)"),
            "got: {}",
            line.line
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_shrink_emits_nothing_and_rebases_the_total() {
        let bytes = Arc::new(AtomicU64::new(10 * MB));
        let cpu_millis = Arc::new(AtomicU64::new(0));
        let calls = Arc::new(AtomicU64::new(0));
        let (tx, mut rx) = mpsc::unbounded_channel();
        let _monitor = BuildProgressMonitor::spawn(
            counter_sampler(&bytes, &cpu_millis, &calls),
            LastOutputLine::default(),
            tx,
        );
        wait_for_calls(&calls, 1).await;

        // Shrink (e.g. cache cleanup): no line, but only assert once at least
        // one post-shrink sample has actually run.
        bytes.store(MB, Ordering::SeqCst);
        let seen = calls.load(Ordering::SeqCst);
        wait_for_calls(&calls, seen + 2).await;
        assert!(rx.try_recv().is_err(), "a shrink must not emit");

        // Growth from the new floor emits, measured from the rebased total.
        bytes.store(3 * MB, Ordering::SeqCst);
        let line = tokio::time::timeout(INTERVAL * 3, rx.recv())
            .await
            .expect("growth after a shrink must emit")
            .expect("channel open");
        assert!(line.line.contains("(+2.0 MB)"), "got: {}", line.line);
    }

    #[tokio::test(start_paused = true)]
    async fn names_the_last_output_once_the_build_goes_quiet() {
        let bytes = Arc::new(AtomicU64::new(0));
        let cpu_millis = Arc::new(AtomicU64::new(0));
        let calls = Arc::new(AtomicU64::new(0));
        let last_output = LastOutputLine::default();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let _monitor = BuildProgressMonitor::spawn(
            counter_sampler(&bytes, &cpu_millis, &calls),
            last_output.clone(),
            tx,
        );
        wait_for_calls(&calls, 1).await;

        // The build prints its last line, then two flat ticks pass in silence:
        // the line alone earns nothing.
        last_output.record("   Compiling viewer v0.1.0 (/opt/waldo/crates/viewer)");
        let seen = calls.load(Ordering::SeqCst);
        wait_for_calls(&calls, seen + 2).await;
        assert!(rx.try_recv().is_err(), "a flat build must not emit");

        // The quiet build burns CPU: its line names the last output.
        cpu_millis.store(5_000, Ordering::SeqCst);
        let line = tokio::time::timeout(INTERVAL * 3, rx.recv())
            .await
            .expect("a CPU tick must emit within an interval")
            .expect("channel open");
        assert!(
            line.line
                .starts_with("Build progress: CPU time 5.0s (+5.0s), last output ")
                && line
                    .line
                    .ends_with(" ago: Compiling viewer v0.1.0 (/opt/waldo/crates/viewer)"),
            "got: {}",
            line.line
        );
    }

    #[tokio::test(start_paused = true)]
    async fn dropping_the_guard_aborts_the_sampling_task() {
        let (tx, mut rx) = mpsc::unbounded_channel::<FeedbackLine>();
        let monitor =
            BuildProgressMonitor::spawn(BuildActivity::default, LastOutputLine::default(), tx);
        drop(monitor);
        // The aborted task drops its sender, so the channel closes; a live
        // task would hold it open forever.
        let closed = tokio::time::timeout(INTERVAL * 12, rx.recv())
            .await
            .expect("the channel must close once the guard is dropped");
        assert!(closed.is_none());
    }
}
