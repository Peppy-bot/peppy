//! The progress lines of the work on the `.sif` of a node that prints
//! nothing of its own: its hash, its push and its pull. Each line counts as
//! activity for the idle timeout of the goal.

use node_stack::Announcer;
use node_stack::build_io::format_bytes;
use std::time::{Duration, Instant};

/// How many lines the size of an image earns at most: one for each percent.
const PERCENT_STEPS: u64 = 100;

/// How long work that moves bytes goes without a line at most. A percent of
/// a large image on a slow link takes minutes, more than the idle timeout of
/// a goal, so a line also comes when bytes moved and the last line is this
/// old.
const LINE_INTERVAL: Duration = Duration::from_secs(10);

/// What the work on an image of `total` bytes reports.
struct TransferProgress {
    /// The verb of each line: `Hashed`, `Pushed`, `Pulled`.
    verb: &'static str,
    total: u64,
    reported_percent: u64,
    reported_bytes: u64,
    last_line_at: Instant,
}

impl TransferProgress {
    fn new(verb: &'static str, total: u64, now: Instant) -> Self {
        Self {
            verb,
            total,
            reported_percent: 0,
            reported_bytes: 0,
            last_line_at: now,
        }
    }

    /// The line to report, as of `now`, once `moved` bytes of the image
    /// moved: when a percent more of the image moved, or when bytes moved
    /// since the last line and that line is [`LINE_INTERVAL`] old.
    fn observe(&mut self, moved: u64, now: Instant) -> Option<String> {
        let percent = moved
            .saturating_mul(PERCENT_STEPS)
            .checked_div(self.total)
            .unwrap_or(PERCENT_STEPS);
        let moved_since_last_line = moved > self.reported_bytes;
        let overdue = now.saturating_duration_since(self.last_line_at) >= LINE_INTERVAL;
        if percent <= self.reported_percent && !(moved_since_last_line && overdue) {
            return None;
        }
        self.reported_percent = self.reported_percent.max(percent);
        self.reported_bytes = moved;
        self.last_line_at = now;
        Some(format!(
            "{} {percent}% ({} of {})",
            self.verb,
            format_bytes(moved),
            format_bytes(self.total)
        ))
    }
}

/// The callback that gets the bytes the work on an image of `total` bytes
/// moved so far, and reports its progress lines through `announcer`.
pub(crate) fn announce_progress(
    verb: &'static str,
    total: u64,
    announcer: &Announcer,
) -> impl FnMut(u64) + Send + 'static {
    let announcer = announcer.clone();
    let mut progress = TransferProgress::new(verb, total, Instant::now());
    move |moved| {
        if let Some(line) = progress.observe(moved, Instant::now()) {
            announcer.line(line);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The unit of [`format_bytes`].
    const MB: u64 = 1 << 20;

    #[test]
    fn a_line_comes_with_each_new_percent() {
        let start = Instant::now();
        let mut progress = TransferProgress::new("Pushed", 100 * MB, start);
        assert_eq!(
            progress.observe(MB / 2, start).as_deref(),
            None,
            "half a percent"
        );
        assert_eq!(
            progress.observe(MB, start).as_deref(),
            Some("Pushed 1% (1.0 MB of 100.0 MB)")
        );
        assert_eq!(progress.observe(MB + 1, start), None, "the same percent");
        assert_eq!(
            progress.observe(100 * MB, start).as_deref(),
            Some("Pushed 100% (100.0 MB of 100.0 MB)")
        );
    }

    /// A percent of a large image on a slow link takes longer than the idle
    /// timeout of a goal: a line comes once bytes moved and the last line is
    /// old, and never while no byte moves.
    #[test]
    fn bytes_that_move_within_a_percent_earn_a_line_once_the_last_one_is_old() {
        let start = Instant::now();
        let mut progress = TransferProgress::new("Pulled", 20 * 1024 * MB, start);
        let later = start + LINE_INTERVAL;
        assert_eq!(
            progress.observe(0, later),
            None,
            "no byte moved since the start"
        );
        assert_eq!(
            progress.observe(10 * MB, start + LINE_INTERVAL / 2),
            None,
            "the last line is recent"
        );
        assert_eq!(
            progress.observe(10 * MB, later).as_deref(),
            Some("Pulled 0% (10.0 MB of 20.0 GB)")
        );
        let even_later = later + LINE_INTERVAL;
        assert_eq!(
            progress.observe(10 * MB, even_later),
            None,
            "no byte moved since the last line"
        );
        assert_eq!(
            progress.observe(20 * MB, even_later).as_deref(),
            Some("Pulled 0% (20.0 MB of 20.0 GB)")
        );
    }

    #[test]
    fn an_empty_image_is_whole_at_once() {
        let start = Instant::now();
        let mut progress = TransferProgress::new("Hashed", 0, start);
        assert_eq!(
            progress.observe(0, start).as_deref(),
            Some("Hashed 100% (0 B of 0 B)")
        );
    }
}
