//! The handle log writers hand their lines to, and the bounded queue behind
//! it.

use crate::level::{printed_level, without_escapes};
use crate::record::{Iostream, LineOrigin, LogIdentity, LogKind, LogRecord};
use daemon_config::peppy_config::{OtlpMinSeverity, Severity};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::SystemTime;
use tokio::sync::mpsc;

/// How many bytes of records the queue holds before it discards arrivals.
const QUEUE_CAPACITY_BYTES: usize = 16 * 1024 * 1024;
/// How many bytes of stack log records the queue holds on top of
/// [`QUEUE_CAPACITY_BYTES`], so the output of a build or a node leaves room
/// for the record of an instance that failed.
const STACK_RESERVE_BYTES: usize = 256 * 1024;
/// The longest body a record carries. A longer line is cut here.
const MAX_BODY_BYTES: usize = 16 * 1024;
/// How much of a line the exporter reads: a body and the escape sequences
/// among it.
const MAX_LINE_BYTES: usize = 4 * MAX_BODY_BYTES;
/// What a queued record costs on top of its body: the record itself and its
/// share of the encoded request.
const RECORD_OVERHEAD_BYTES: usize = 128;

/// Where a log writer sends the lines it writes. Cloning it is cheap. A
/// disabled exporter does no work.
#[derive(Clone)]
pub struct LogExporter {
    queue: Option<Arc<Queue>>,
}

struct Queue {
    sender: mpsc::UnboundedSender<QueuedRecord>,
    min_severity: Option<OtlpMinSeverity>,
    lines: Arc<Budget>,
    stack: Arc<Budget>,
    discarded: Arc<AtomicU64>,
}

/// The receiving end of an exporter's queue.
pub struct LogRecordReceiver {
    receiver: mpsc::UnboundedReceiver<QueuedRecord>,
    discarded: Arc<AtomicU64>,
}

/// A record in the queue. It holds its share of the queue's capacity until it
/// is dropped.
pub struct QueuedRecord {
    record: LogRecord,
    reservation: Reservation,
}

impl LogExporter {
    /// An exporter that exports nothing.
    pub fn disabled() -> Self {
        Self { queue: None }
    }

    /// An exporter and the receiver of its queue. A record whose severity is
    /// below `min_severity` is not queued; a record with no severity is.
    pub fn channel(min_severity: Option<OtlpMinSeverity>) -> (Self, LogRecordReceiver) {
        Self::with_capacity(min_severity, QUEUE_CAPACITY_BYTES, STACK_RESERVE_BYTES)
    }

    /// [`Self::channel`] with the capacities of the tests of this crate.
    pub(crate) fn with_capacity(
        min_severity: Option<OtlpMinSeverity>,
        capacity_bytes: usize,
        stack_reserve_bytes: usize,
    ) -> (Self, LogRecordReceiver) {
        let (sender, receiver) = mpsc::unbounded_channel();
        let discarded = Arc::new(AtomicU64::new(0));
        let queue = Queue {
            sender,
            min_severity,
            lines: Arc::new(Budget::new(capacity_bytes)),
            stack: Arc::new(Budget::new(stack_reserve_bytes)),
            discarded: Arc::clone(&discarded),
        };
        let exporter = Self {
            queue: Some(Arc::new(queue)),
        };
        (
            exporter,
            LogRecordReceiver {
                receiver,
                discarded,
            },
        )
    }

    /// Exports a line the daemon wrote itself, at the daemon's `severity`.
    pub fn export_daemon_line(
        &self,
        identity: &Arc<LogIdentity>,
        time: SystemTime,
        severity: Severity,
        line: &str,
    ) {
        let Some(queue) = &self.queue else {
            return;
        };
        let (body, truncated) = body_of(line);
        queue.push(LogRecord {
            identity: Arc::clone(identity),
            time,
            origin: LineOrigin::Daemon,
            severity: Some(severity),
            severity_text: Some(severity.name().to_ascii_uppercase()),
            body,
            truncated,
        });
    }

    /// Exports a line the daemon captured from a node or a build, with the
    /// severity of the level the line prints. A line that holds only white
    /// space is not exported.
    pub fn export_captured_line(
        &self,
        identity: &Arc<LogIdentity>,
        time: SystemTime,
        stream: Iostream,
        line: &str,
    ) {
        let Some(queue) = &self.queue else {
            return;
        };
        let (body, truncated) = body_of(line);
        if body.trim().is_empty() {
            return;
        }
        let level = printed_level(&body);
        let severity = level.as_ref().map(|level| level.severity);
        let severity_text = level.map(|level| level.text.to_owned());
        queue.push(LogRecord {
            identity: Arc::clone(identity),
            time,
            origin: LineOrigin::Captured(stream),
            severity,
            severity_text,
            body,
            truncated,
        });
    }
}

impl Queue {
    fn push(&self, record: LogRecord) {
        let below_minimum = matches!(
            (record.severity, self.min_severity),
            (Some(severity), Some(minimum)) if severity < minimum.severity()
        );
        if below_minimum {
            return;
        }
        let cost = cost(&record);
        // A stack record takes its room of the reserve once the queue is full.
        let reserve = matches!(record.identity.kind, LogKind::Stack).then_some(&self.stack);
        let reservation = Budget::reserve(&self.lines, cost)
            .or_else(|| reserve.and_then(|reserve| Budget::reserve(reserve, cost)));
        let Some(reservation) = reservation else {
            self.discarded.fetch_add(1, Ordering::Relaxed);
            return;
        };
        // A closed queue drops the record, which gives its bytes back.
        let _ = self.sender.send(QueuedRecord {
            record,
            reservation,
        });
    }
}

impl LogRecordReceiver {
    /// The next record, or `None` once every exporter of the queue is gone
    /// and the queue is empty.
    pub async fn recv(&mut self) -> Option<QueuedRecord> {
        self.receiver.recv().await
    }

    /// The next record when one is queued.
    pub fn try_recv(&mut self) -> Option<QueuedRecord> {
        self.receiver.try_recv().ok()
    }

    /// How many records the full queue discarded since the last call.
    pub(crate) fn take_discarded(&self) -> u64 {
        self.discarded.swap(0, Ordering::Relaxed)
    }
}

impl QueuedRecord {
    pub fn record(&self) -> &LogRecord {
        &self.record
    }

    /// The record, and its share of the queue's capacity.
    pub(crate) fn into_parts(self) -> (LogRecord, Reservation) {
        (self.record, self.reservation)
    }
}

/// The bytes `record` takes of the queue's capacity: what its strings hold,
/// and the record itself.
fn cost(record: &LogRecord) -> usize {
    let text = record.severity_text.as_ref().map_or(0, String::capacity);
    RECORD_OVERHEAD_BYTES + record.body.capacity() + text
}

/// The body of the record of `line`, and whether it is the start of a longer
/// line: the line without its terminal escape sequences, cut at
/// [`MAX_BODY_BYTES`]. The exporter reads [`MAX_LINE_BYTES`] of a line and
/// keeps an allocation the size of the body, so a record takes a bounded
/// number of bytes whatever the length of its line.
fn body_of(line: &str) -> (String, bool) {
    let read = prefix(line, MAX_LINE_BYTES);
    let mut body = without_escapes(read);
    let kept = prefix(&body, MAX_BODY_BYTES).len();
    let truncated = read.len() < line.len() || kept < body.len();
    body.truncate(kept);
    body.shrink_to_fit();
    (body, truncated)
}

/// The longest start of `text` that holds at most `max_bytes` and ends on a
/// character boundary.
fn prefix(text: &str, max_bytes: usize) -> &str {
    let end = (0..=max_bytes.min(text.len()))
        .rev()
        .find(|index| text.is_char_boundary(*index))
        .expect("the start of a string is a character boundary");
    &text[..end]
}

/// A number of bytes that reservations share.
struct Budget {
    capacity: usize,
    used: AtomicUsize,
}

/// A share of a [`Budget`], given back when dropped.
pub(crate) struct Reservation {
    budget: Arc<Budget>,
    cost: usize,
}

impl Budget {
    fn new(capacity: usize) -> Self {
        Self {
            capacity,
            used: AtomicUsize::new(0),
        }
    }

    /// Takes `cost` bytes of `budget`, or `None` when they do not fit.
    fn reserve(budget: &Arc<Self>, cost: usize) -> Option<Reservation> {
        budget
            .used
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |used| {
                used.checked_add(cost)
                    .filter(|total| *total <= budget.capacity)
            })
            .ok()?;
        Some(Reservation {
            budget: Arc::clone(budget),
            cost,
        })
    }
}

impl Drop for Reservation {
    fn drop(&mut self) {
        self.budget.used.fetch_sub(self.cost, Ordering::AcqRel);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::record::StackAction;
    use crate::test_support::drain_records as drain;
    use std::time::Duration;

    fn identity(kind: LogKind) -> Arc<LogIdentity> {
        Arc::new(LogIdentity::new(kind, "/logs/any.log"))
    }

    fn at(seconds: u64) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(seconds)
    }

    #[test]
    fn a_daemon_line_carries_the_daemons_severity() {
        let (exporter, mut receiver) = LogExporter::channel(None);
        let run = identity(LogKind::Run);
        exporter.export_daemon_line(&run, at(7), Severity::Warn, "bind source created");

        assert_eq!(
            drain(&mut receiver),
            [LogRecord {
                identity: run,
                time: at(7),
                origin: LineOrigin::Daemon,
                severity: Some(Severity::Warn),
                severity_text: Some("WARN".to_owned()),
                body: "bind source created".to_owned(),
                truncated: false,
            }]
        );
    }

    #[test]
    fn a_captured_line_carries_the_level_it_prints_and_its_stream() {
        let (exporter, mut receiver) = LogExporter::channel(None);
        let run = identity(LogKind::Run);
        exporter.export_captured_line(
            &run,
            at(1),
            Iostream::Stderr,
            "\u{1b}[33mWARNING\u{1b}[0m: battery low",
        );
        exporter.export_captured_line(&run, at(2), Iostream::Stdout, "control loop: 60 Hz");

        let records = drain(&mut receiver);
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].origin, LineOrigin::Captured(Iostream::Stderr));
        assert_eq!(records[0].severity, Some(Severity::Warn));
        assert_eq!(records[0].severity_text.as_deref(), Some("WARNING"));
        assert_eq!(records[0].body, "WARNING: battery low");
        assert_eq!(records[1].origin, LineOrigin::Captured(Iostream::Stdout));
        assert_eq!(records[1].severity, None);
        assert_eq!(records[1].severity_text, None);
    }

    #[test]
    fn a_line_of_white_space_or_escapes_is_not_exported() {
        let (exporter, mut receiver) = LogExporter::channel(None);
        let run = identity(LogKind::Run);
        for line in ["", "   ", "\u{1b}[0m", "\u{1b}[2K \u{1b}[0m"] {
            exporter.export_captured_line(&run, at(1), Iostream::Stdout, line);
        }
        assert_eq!(drain(&mut receiver), []);
        assert_eq!(receiver.take_discarded(), 0);
    }

    #[test]
    fn a_record_below_the_minimum_severity_is_not_queued_and_one_with_no_severity_is() {
        let (exporter, mut receiver) = LogExporter::channel(Some(OtlpMinSeverity::Info));
        let run = identity(LogKind::Run);
        for line in [
            "DEBUG tick 12",
            "TRACE tick 12",
            "INFO ready",
            "ERROR stopped",
            "a plain print",
        ] {
            exporter.export_captured_line(&run, at(1), Iostream::Stdout, line);
        }
        exporter.export_daemon_line(&run, at(2), Severity::Debug, "a debug note");

        let bodies: Vec<String> = drain(&mut receiver)
            .into_iter()
            .map(|record| record.body)
            .collect();
        assert_eq!(bodies, ["INFO ready", "ERROR stopped", "a plain print"]);
        assert_eq!(receiver.take_discarded(), 0);
    }

    #[test]
    fn a_long_line_is_cut_on_a_character_boundary() {
        let (exporter, mut receiver) = LogExporter::channel(None);
        // A two-byte character straddles the cut.
        let line = format!("{}é{}", "a".repeat(MAX_BODY_BYTES - 1), "b".repeat(10));
        exporter.export_captured_line(&identity(LogKind::Build), at(1), Iostream::Stdout, &line);

        let records = drain(&mut receiver);
        assert_eq!(records[0].body, "a".repeat(MAX_BODY_BYTES - 1));
        assert!(records[0].truncated);
    }

    #[test]
    fn a_full_queue_discards_the_newest_records_and_counts_them() {
        let line = "x".repeat(72);
        let record_cost = RECORD_OVERHEAD_BYTES + line.len();
        let (exporter, mut receiver) = LogExporter::with_capacity(None, 2 * record_cost, 0);
        let run = identity(LogKind::Run);
        for second in 1..=5 {
            exporter.export_captured_line(&run, at(second), Iostream::Stdout, &line);
        }

        let queued: Vec<QueuedRecord> = std::iter::from_fn(|| receiver.try_recv()).collect();
        let times: Vec<SystemTime> = queued.iter().map(|queued| queued.record().time).collect();
        assert_eq!(times, [at(1), at(2)]);
        assert_eq!(receiver.take_discarded(), 3);
        assert_eq!(receiver.take_discarded(), 0);

        // Dropping the queued records gives their bytes back.
        drop(queued);
        exporter.export_captured_line(&run, at(6), Iostream::Stdout, &line);
        assert_eq!(drain(&mut receiver).len(), 1);
    }

    #[test]
    fn a_record_holds_a_bounded_allocation_whatever_the_length_of_its_line() {
        let (exporter, mut receiver) = LogExporter::channel(None);
        let line = format!("\u{1b}[32mINFO\u{1b}[0m {}", "x".repeat(1024 * 1024));
        exporter.export_captured_line(&identity(LogKind::Run), at(1), Iostream::Stdout, &line);
        exporter.export_daemon_line(&identity(LogKind::Run), at(2), Severity::Error, &line);

        let queued: Vec<QueuedRecord> = std::iter::from_fn(|| receiver.try_recv()).collect();
        assert_eq!(queued.len(), 2);
        for queued in &queued {
            let record = queued.record();
            assert_eq!(record.body.len(), MAX_BODY_BYTES);
            assert_eq!(record.body.capacity(), MAX_BODY_BYTES);
            assert!(record.body.starts_with("INFO xxx"));
            assert!(record.truncated);
        }
        // The queue charges what the records hold.
        let queue = exporter.queue.as_ref().expect("the exporter is enabled");
        let charged = queue.lines.used.load(Ordering::Acquire);
        assert!(
            charged < 2 * (MAX_BODY_BYTES + 2 * RECORD_OVERHEAD_BYTES),
            "{charged}"
        );
    }

    #[test]
    fn a_stack_record_takes_room_of_the_queue_before_its_reserve() {
        let line = "Instance 'arm_inst' failed";
        let record_cost = RECORD_OVERHEAD_BYTES + line.len() + "ERROR".len();
        let (exporter, mut receiver) = LogExporter::with_capacity(None, record_cost, record_cost);
        let stack = identity(LogKind::Stack);
        for second in 1..=3 {
            exporter.export_daemon_line(&stack, at(second), Severity::Error, line);
        }

        assert_eq!(drain(&mut receiver).len(), 2);
        assert_eq!(receiver.take_discarded(), 1);
    }

    #[test]
    fn stack_records_have_their_own_room_in_a_full_queue() {
        let line = "x".repeat(72);
        let record_cost = RECORD_OVERHEAD_BYTES + line.len();
        let (exporter, mut receiver) = LogExporter::with_capacity(None, record_cost, 512);
        let run = identity(LogKind::Run);
        exporter.export_captured_line(&run, at(1), Iostream::Stdout, &line);
        exporter.export_captured_line(&run, at(2), Iostream::Stdout, &line);
        exporter.export_daemon_line(
            &identity(LogKind::Stack),
            at(3),
            Severity::Error,
            "Instance 'arm_inst' failed",
        );

        let kinds: Vec<LogKind> = std::iter::from_fn(|| receiver.try_recv())
            .map(|queued| queued.record().identity.kind)
            .collect();
        assert_eq!(kinds, [LogKind::Run, LogKind::Stack]);
        assert_eq!(receiver.take_discarded(), 1);
    }

    #[tokio::test]
    async fn a_record_sent_after_the_receiver_is_gone_gives_its_bytes_back() {
        let line = "x".repeat(72);
        let record_cost = RECORD_OVERHEAD_BYTES + line.len();
        let (exporter, receiver) = LogExporter::with_capacity(None, record_cost, 0);
        drop(receiver);
        let run = identity(LogKind::Run);
        exporter.export_captured_line(&run, at(1), Iostream::Stdout, &line);
        exporter.export_captured_line(&run, at(2), Iostream::Stdout, &line);

        let queue = exporter.queue.as_ref().expect("the exporter is enabled");
        assert_eq!(queue.lines.used.load(Ordering::Acquire), 0);
        assert_eq!(queue.discarded.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn recv_ends_once_every_exporter_is_gone() {
        let (exporter, mut receiver) = LogExporter::channel(None);
        exporter.export_daemon_line(
            &identity(LogKind::Launch {
                action: StackAction::Launch,
            }),
            at(1),
            Severity::Info,
            "Running nodes...",
        );
        drop(exporter);

        let first = receiver.recv().await.expect("the queued record");
        assert_eq!(first.record().body, "Running nodes...");
        assert!(receiver.recv().await.is_none());
    }
}
