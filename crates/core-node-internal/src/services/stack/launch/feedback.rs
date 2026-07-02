use super::ProcessLaunchContext;
use crate::services::node::{FeedbackLine, FeedbackStream};
use chrono::Local;
use core_node_api::encoding::{LaunchFeedback, LaunchFeedbackStep};
use parking_lot::Mutex as StdMutex;
use peppylib::messaging::ActionFeedbackPublisher;
use std::fs::File;
use std::io::Write;
use std::sync::Arc;
use tokio::sync::{Notify, mpsc};
use tokio::task::JoinHandle;

async fn publish_feedback(ctx: &ProcessLaunchContext, feedback: LaunchFeedback) {
    {
        let mut file = ctx.log_file.lock();
        let timestamp = Local::now().format("%Y-%m-%dT%H:%M:%S%.3f");
        let _ = writeln!(
            file,
            "[{}] [{}] {}",
            timestamp, feedback.stream, feedback.line
        );
    }

    if let Ok(payload) = feedback.encode() {
        let _ = ctx.feedback_publisher.publish(payload).await;
    }
}

pub(super) async fn publish_stdout(
    ctx: &ProcessLaunchContext,
    line: impl Into<String>,
    step: LaunchFeedbackStep,
) {
    publish_feedback(ctx, LaunchFeedback::stdout(line, step)).await;
}

pub(super) async fn publish_stderr(
    ctx: &ProcessLaunchContext,
    line: impl Into<String>,
    step: LaunchFeedbackStep,
) {
    publish_feedback(ctx, LaunchFeedback::stderr(line, step)).await;
}

/// Spawns a feedback forwarding task that reads `FeedbackLine` values from the
/// channel and publishes them as `LaunchFeedback` to the launch feedback topic.
///
/// Each line received also pings `activity_notify` (if provided), which the per-phase idle
/// watcher uses to reset its idle clock. The notify is the single seam where real subprocess /
/// git2 / http-downloader output (which all flow through this mpsc) gets observed; launcher
/// orchestration messages (`publish_stdout` / `publish_stderr`) bypass this channel and so do
/// NOT reset the idle clock, which is the right behavior, since they're operator narration,
/// not subprocess liveness.
///
/// Returns the sender end (to pass into the process context) and a join handle
/// for the consumer task. Drop the sender to signal completion, then await the
/// handle to drain remaining messages.
pub(super) fn spawn_feedback_forwarder(
    feedback_publisher: &ActionFeedbackPublisher,
    step: LaunchFeedbackStep,
    log_file: &Arc<StdMutex<File>>,
    activity_notify: Option<Arc<Notify>>,
) -> (mpsc::UnboundedSender<FeedbackLine>, JoinHandle<()>) {
    let (feedback_tx, mut feedback_rx) = mpsc::unbounded_channel::<FeedbackLine>();
    let publisher = feedback_publisher.clone();
    let log_file = Arc::clone(log_file);
    let handle = tokio::spawn(async move {
        while let Some(line) = feedback_rx.recv().await {
            if let Some(notify) = &activity_notify {
                notify.notify_one();
            }

            node_stack::build_io::write_feedback_log_line(&log_file, line.stream, &line.line);

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
