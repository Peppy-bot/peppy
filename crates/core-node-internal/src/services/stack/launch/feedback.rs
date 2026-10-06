use crate::services::node::{FeedbackLine, FeedbackStream};
use crate::services::stack::action::StackChangeContext;
use core_node_api::encoding::{LaunchFeedback, LaunchFeedbackStep};
use daemon_config::peppy_config::Severity;
use node_stack::{ActionLog, FeedbackOwner};
use peppylib::messaging::ActionFeedbackPublisher;
use std::sync::Arc;
use tokio::sync::{Notify, mpsc};
use tokio::task::JoinHandle;

/// Writes `feedback` to the launch log and publishes it. A line with a
/// `severity` is the launch log's own and is exported; a line with none is in
/// the file only.
async fn publish_feedback(
    ctx: &StackChangeContext,
    feedback: LaunchFeedback,
    severity: Option<Severity>,
) {
    let stream = if feedback.is_stdout() {
        FeedbackStream::Stdout
    } else {
        FeedbackStream::Stderr
    };
    match severity {
        Some(severity) => ctx.log.narrate(stream, severity, &feedback.line),
        None => ctx.log.relay(stream, &feedback.line),
    }

    if let Ok(payload) = feedback.encode() {
        let _ = ctx.feedback_publisher.publish(payload).await;
    }
}

/// Narrates a step of the stack change.
pub(in crate::services::stack) async fn publish_stdout(
    ctx: &StackChangeContext,
    line: impl Into<String>,
    step: LaunchFeedbackStep,
) {
    publish_feedback(
        ctx,
        LaunchFeedback::stdout(line, step),
        Some(Severity::Info),
    )
    .await;
}

/// Narrates a problem the stack change goes on after.
pub(in crate::services::stack) async fn publish_warning(
    ctx: &StackChangeContext,
    line: impl Into<String>,
    step: LaunchFeedbackStep,
) {
    publish_feedback(
        ctx,
        LaunchFeedback::stderr(line, step),
        Some(Severity::Warn),
    )
    .await;
}

/// Narrates the problem that ends the stack change.
pub(in crate::services::stack) async fn publish_error(
    ctx: &StackChangeContext,
    line: impl Into<String>,
    step: LaunchFeedbackStep,
) {
    publish_feedback(
        ctx,
        LaunchFeedback::stderr(line, step),
        Some(Severity::Error),
    )
    .await;
}

/// Relays a line another daemon reported for its part of the stack change.
/// That daemon exports the line from its own log; here it reaches the launch
/// log file and the terminal.
pub(in crate::services::stack) async fn publish_relayed(
    ctx: &StackChangeContext,
    line: impl Into<String>,
    step: LaunchFeedbackStep,
) {
    publish_feedback(ctx, LaunchFeedback::stdout(line, step), None).await;
}

/// Writes `line`, which a node action of the launch reported, to the launch
/// log when the log holds such a line.
fn record_in_launch_log(log: &ActionLog, line: &FeedbackLine) {
    match line.owner() {
        FeedbackOwner::ActionLog => log.relay(line.stream, &line.line),
        FeedbackOwner::NoLog => log.note(&line.line),
        FeedbackOwner::Progress => {}
    }
}

/// Spawns a feedback forwarding task that reads `FeedbackLine` values from the
/// channel and publishes them as `LaunchFeedback` to the launch feedback topic.
///
/// Each line received also pings `activity_notify` (if provided), which the
/// per-phase idle watcher uses to reset its idle clock. The notify is the
/// single seam where real subprocess / git2 / http-downloader output (which
/// all flow through this mpsc) gets observed. Launcher narration
/// (`publish_stdout`, `publish_warning`, `publish_error`) bypasses this
/// channel and does not reset the idle clock: the clock measures subprocess
/// liveness.
///
/// A line the log of its node action holds lands in the launch log file and
/// is exported from the node log. A step that no node log holds is the launch
/// log's own line, exported from it. A progress sample reaches the terminal
/// alone.
///
/// Returns the sender end (to pass into the process context) and a join handle
/// for the consumer task. Drop the sender to signal completion, then await the
/// handle to drain remaining messages.
pub(in crate::services::stack) fn spawn_feedback_forwarder(
    feedback_publisher: &ActionFeedbackPublisher,
    step: LaunchFeedbackStep,
    log: &ActionLog,
    activity_notify: Option<Arc<Notify>>,
) -> (mpsc::UnboundedSender<FeedbackLine>, JoinHandle<()>) {
    let (feedback_tx, mut feedback_rx) = mpsc::unbounded_channel::<FeedbackLine>();
    let publisher = feedback_publisher.clone();
    let log = log.clone();
    let handle = tokio::spawn(async move {
        while let Some(line) = feedback_rx.recv().await {
            if let Some(notify) = &activity_notify {
                notify.notify_one();
            }

            record_in_launch_log(&log, &line);

            let launch_feedback = match line.stream {
                FeedbackStream::Stdout => LaunchFeedback::stdout(&line.line, step),
                FeedbackStream::Stderr => LaunchFeedback::stderr(&line.line, step),
                // Warnings bypass the per-node scrolling step and surface as
                // persistent LauncherStep stderr lines so the operator sees
                // them even after the step buffer scrolls past.
                FeedbackStream::Warning => {
                    LaunchFeedback::stderr(&line.line, LaunchFeedbackStep::LauncherStep)
                }
            };
            if let Ok(payload) = launch_feedback.encode() {
                let _ = publisher.publish(payload).await;
            }
        }
    });
    (feedback_tx, handle)
}

#[cfg(test)]
mod tests {
    use super::*;
    use log_export::test_support::drain_records;
    use log_export::{LogExporter, LogKind, StackAction};
    use node_stack::Announcer;
    use node_stack::action_log::test_support::lines_without_time;

    #[test]
    fn the_launch_log_holds_node_lines_without_exporting_them_and_no_progress_sample() {
        let dir = tempfile::tempdir().unwrap();
        let (exporter, mut exported) = LogExporter::channel(None);
        let launch = LogKind::Launch {
            action: StackAction::Launch,
        };
        let launch_log = ActionLog::create(dir.path(), "launch.log", launch, exporter).unwrap();
        let build_log = ActionLog::create(
            dir.path(),
            "build.log",
            LogKind::Build,
            LogExporter::disabled(),
        )
        .unwrap();
        let (feedback_tx, mut feedback_rx) = mpsc::unbounded_channel();
        let announcer = Announcer::new(build_log, feedback_tx.clone());
        announcer.line("Using sccache for Rust compilation");
        announcer.progress("Build progress: 12 MB written");
        feedback_tx
            .send(FeedbackLine::unlogged("Cloning the launcher repository"))
            .unwrap();

        while let Ok(line) = feedback_rx.try_recv() {
            record_in_launch_log(&launch_log, &line);
        }

        let file = std::fs::read_to_string(launch_log.path()).unwrap();
        assert_eq!(
            lines_without_time(&file),
            [
                "[stdout] Using sccache for Rust compilation",
                "[stdout] Cloning the launcher repository",
            ]
        );
        let bodies: Vec<String> = drain_records(&mut exported)
            .into_iter()
            .map(|record| record.body)
            .collect();
        assert_eq!(bodies, ["Cloning the launcher repository"]);
    }
}
