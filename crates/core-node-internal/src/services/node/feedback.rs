//! Feedback-stream plumbing shared by the add/build/run goal handlers: the
//! [`Report`] a blocking checkout or materialization makes of its work, the
//! sinks that route a report to an action's log and feedback channel, and
//! the forwarder that republishes the feedback lines of an action onto a
//! peppylib topic.
//!
//! `FeedbackLine`/`FeedbackStream` are re-exported from
//! `node-stack-internal::build_io`, where `stream_child_output` streams
//! build output. `FeedbackStream` itself is owned by `core-node-api` and
//! re-exported through node-stack, so producers, transports, and the wire
//! types all share one enum.

use node_stack::Announcer;
pub(crate) use node_stack::{FeedbackLine, FeedbackStream};
use tokio::sync::mpsc;

/// One line a blocking checkout or materialization reports while it works.
#[derive(Clone, Copy)]
pub enum Report<'a> {
    /// A step of the work.
    Step(&'a str),
    /// A sample of a transfer in progress, which repeats until the transfer
    /// ends.
    Progress(&'a str),
}

impl<'a> Report<'a> {
    /// The line of the report.
    pub fn text(self) -> &'a str {
        match self {
            Report::Step(line) | Report::Progress(line) => line,
        }
    }
}

/// Reports to `announcer`: a step lands in its log and on its feedback
/// channel, and a progress sample on the feedback channel alone.
pub(crate) fn step_sink(announcer: Announcer) -> impl Fn(Report<'_>) + Send + Sync + 'static {
    move |report| match report {
        Report::Step(line) => announcer.line(line),
        Report::Progress(line) => announcer.progress(line),
    }
}

/// [`step_sink`] for the steps of a node sync, which the daemon's own output
/// shows too under the `peppy::interface` target.
pub(crate) fn interface_step_sink(
    announcer: Announcer,
) -> impl Fn(Report<'_>) + Send + Sync + 'static {
    let sink = step_sink(announcer);
    move |report| {
        trace_interface_step(report);
        sink(report);
    }
}

/// Shows a step of a node sync in the daemon's own output.
pub(crate) fn trace_interface_step(report: Report<'_>) {
    if let Report::Step(line) = report {
        tracing::info!(target: "peppy::interface", "{line}");
    }
}

/// Reports to `tx` for a caller with no log of its own: a step is a stdout
/// line that no log holds. Send errors are ignored: a closed channel means
/// the consumer is gone.
pub(crate) fn unlogged_step_sink(
    tx: mpsc::UnboundedSender<FeedbackLine>,
) -> impl Fn(Report<'_>) + Send + Sync + 'static {
    move |report| {
        let line = match report {
            Report::Step(line) => FeedbackLine::unlogged(line),
            Report::Progress(line) => FeedbackLine::progress(line),
        };
        let _ = tx.send(line);
    }
}

/// Spawns a task that consumes `FeedbackLine` values from `feedback_rx`,
/// converts each one via `encode` and publishes the resulting payload. Shared
/// by the add/build/start goal handlers, which all run the same
/// consumer-side forwarder over differently-typed feedback encoders.
///
/// `encode` returns a [`peppylib::messaging::NonEmptyPayload`] so the
/// publish path is type-guaranteed not to send the empty end-of-stream
/// sentinel by mistake; the codegen-driven `*Feedback::encode()` methods
/// in `core-node-api` already return that type via
/// `encode_message_non_empty`.
pub(crate) fn spawn_feedback_forwarder<F>(
    mut feedback_rx: tokio::sync::mpsc::UnboundedReceiver<FeedbackLine>,
    publisher: peppylib::messaging::ActionFeedbackPublisher,
    encode: F,
) -> tokio::task::JoinHandle<()>
where
    F: Fn(FeedbackLine) -> core_node_api::Result<peppylib::messaging::NonEmptyPayload>
        + Send
        + 'static,
{
    tokio::spawn(async move {
        while let Some(line) = feedback_rx.recv().await {
            if let Ok(payload) = encode(line) {
                let _ = publisher.publish(payload).await;
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use log_export::{LogExporter, LogKind};
    use node_stack::{ActionLog, FeedbackOwner};

    const STEP: &str = "Cloning repository https://host/repo.git...";
    const REPORT: &str = "Cloning https://host/repo.git: received 12/480 objects (3.0 MB)";

    fn sent(
        feedback_rx: &mut mpsc::UnboundedReceiver<FeedbackLine>,
    ) -> Vec<(String, FeedbackOwner)> {
        std::iter::from_fn(|| feedback_rx.try_recv().ok())
            .map(|line| (line.line.clone(), line.owner()))
            .collect()
    }

    #[test]
    fn a_step_sink_logs_each_step_and_keeps_a_progress_report_out_of_the_log() {
        let dir = tempfile::tempdir().unwrap();
        let (exporter, mut records) = LogExporter::channel(None);
        let log = ActionLog::create(dir.path(), "add.log", LogKind::Add, exporter).unwrap();
        let (feedback_tx, mut feedback_rx) = mpsc::unbounded_channel();
        let sink = step_sink(Announcer::new(log.clone(), feedback_tx));

        sink(Report::Step(STEP));
        sink(Report::Progress(REPORT));

        assert_eq!(
            sent(&mut feedback_rx),
            [
                (STEP.to_owned(), FeedbackOwner::ActionLog),
                (REPORT.to_owned(), FeedbackOwner::Progress),
            ]
        );
        let file = std::fs::read_to_string(log.path()).unwrap();
        assert!(file.contains(STEP), "{file}");
        assert!(!file.contains(REPORT), "{file}");
        let exported: Vec<String> = log_export::test_support::drain_records(&mut records)
            .into_iter()
            .map(|record| record.body)
            .collect();
        assert_eq!(exported, [STEP]);
    }

    #[test]
    fn an_unlogged_step_sink_marks_each_step_as_held_by_no_log() {
        let (feedback_tx, mut feedback_rx) = mpsc::unbounded_channel();
        let sink = unlogged_step_sink(feedback_tx);

        sink(Report::Step(STEP));
        sink(Report::Progress(REPORT));

        assert_eq!(
            sent(&mut feedback_rx),
            [
                (STEP.to_owned(), FeedbackOwner::NoLog),
                (REPORT.to_owned(), FeedbackOwner::Progress),
            ]
        );
    }
}
