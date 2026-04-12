use peppylib::messaging::ActionGoalHandle;
use peppylib::{ActionMessenger, MessengerHandle, PeppyError};
use tracing::info;

use super::node::TimeoutConfig;
use crate::commands::SCROLLING_OUTPUT_LINES;
use crate::error::{Error, Result};
use crate::terminal::ScrollingOutput;

use std::path::Path;
use std::time::Duration;

/// Minimal shape of an action goal response accepted by
/// [`run_action_with_feedback`]. Implemented for `NodeAddGoalResponse`,
/// `NodeBuildGoalResponse`, and `NodeRunGoalResponse` in the caller files.
pub(crate) trait ActionGoalResponseLike: Sized {
    fn decode_payload(data: &[u8]) -> std::result::Result<Self, String>;
    fn accepted(&self) -> bool;
    fn rejection_reason(self) -> Option<String>;
    fn log_path(&self) -> &Path;
}

/// Minimal shape of an action feedback used by [`run_action_with_feedback`].
pub(crate) trait ActionFeedbackLike: Sized {
    fn decode_payload(data: &[u8]) -> std::result::Result<Self, String>;
    fn line(&self) -> &str;
    fn is_stderr(&self) -> bool;
}

/// Minimal shape of an action result used by [`run_action_with_feedback`].
pub(crate) trait ActionResultLike: Sized {
    fn decode_payload(data: &[u8]) -> std::result::Result<Self, String>;
    fn success(&self) -> bool;
    fn take_error_message(self) -> Option<String>;
}

/// Drives a goal that has already been sent to completion: decodes the goal
/// response, prints the log-file path, polls feedback into a scrolling output,
/// decodes the final result, and maps `success == false` into an execution
/// error. Replaces ~40 lines of duplicated boilerplate in the `node add`,
/// `node build`, and `node run`/`run` commands.
pub(crate) async fn run_action_with_feedback<Resp, Fb, Res>(
    messenger: &MessengerHandle,
    action_handle: &mut ActionGoalHandle,
    timeouts: &TimeoutConfig,
    action_name: &str,
) -> Result<Res>
where
    Resp: ActionGoalResponseLike,
    Fb: ActionFeedbackLike,
    Res: ActionResultLike,
{
    let goal_response_payload = action_handle.goal_response().payload();
    let goal_response = Resp::decode_payload(&goal_response_payload)
        .map_err(|e| Error::ExecutionFailed(format!("Failed to decode goal response: {}", e)))?;

    if !goal_response.accepted() {
        return Err(Error::ExecutionFailed(format!(
            "Goal rejected: {}",
            goal_response
                .rejection_reason()
                .unwrap_or_else(|| "unknown reason".to_string())
        )));
    }

    info!("Log file: {}", goal_response.log_path().display());

    let mut scrolling_output = ScrollingOutput::new(SCROLLING_OUTPUT_LINES);

    let result = poll_action_to_completion::<Res>(
        messenger,
        action_handle,
        timeouts,
        &mut scrolling_output,
        |payload, output| {
            if let Ok(feedback) = Fb::decode_payload(payload) {
                output.add_line(feedback.line(), feedback.is_stderr());
            }
        },
        |payload| match Res::decode_payload(payload) {
            Ok(result) => Ok(Some(result)),
            Err(err) => {
                if peppylib::encoding::is_result_pending(payload) {
                    Ok(None)
                } else {
                    Err(format!("Failed to decode {action_name} result: {err}"))
                }
            }
        },
    )
    .await?;

    scrolling_output.clear();

    if !result.success() {
        return Err(Error::ExecutionFailed(
            result
                .take_error_message()
                .unwrap_or_else(|| format!("{action_name} failed with no error message")),
        ));
    }

    Ok(result)
}

const FEEDBACK_DRAIN_TIMEOUT: Duration = Duration::from_millis(50);
const RESULT_POLL_TIMEOUT: Duration = Duration::from_millis(200);
const SLEEP_BETWEEN_POLLS: Duration = Duration::from_millis(50);

/// Polls an action goal to completion, draining feedback and checking timeouts.
///
/// - `on_feedback` is called with each raw feedback payload to process feedback
///   (typically decode + feed to scrolling output).
/// - `decode_result` attempts to decode the final result from the raw payload.
///   It should return `Ok(Some(result))` on success, `Ok(None)` if the payload
///   is a "result pending" sentinel, or `Err` on decode failure.
///
/// On timeout or error, the scrolling output is cleared before returning.
pub(crate) async fn poll_action_to_completion<R>(
    messenger: &MessengerHandle,
    action_handle: &mut ActionGoalHandle,
    timeouts: &TimeoutConfig,
    scrolling_output: &mut ScrollingOutput,
    mut on_feedback: impl FnMut(&[u8], &mut ScrollingOutput),
    decode_result: impl Fn(&[u8]) -> std::result::Result<Option<R>, String>,
) -> Result<R> {
    let idle_timeout = Duration::from_secs(timeouts.idle_secs);
    let absolute_deadline = tokio::time::Instant::now() + Duration::from_secs(timeouts.max_secs);
    let mut last_activity = tokio::time::Instant::now();

    loop {
        // Drain feedback so the publisher doesn't block on a full channel.
        loop {
            check_timeouts(last_activity, idle_timeout, absolute_deadline, timeouts).inspect_err(
                |_| {
                    scrolling_output.clear();
                },
            )?;

            match tokio::time::timeout(FEEDBACK_DRAIN_TIMEOUT, action_handle.on_next_feedback())
                .await
            {
                Ok(Ok(msg)) => {
                    last_activity = tokio::time::Instant::now();
                    on_feedback(&msg.payload(), scrolling_output);
                }
                Ok(Err(_)) => {
                    tracing::debug!("Feedback channel closed");
                    break;
                }
                Err(_) => break, // timeout — drain complete
            }
        }

        check_timeouts(last_activity, idle_timeout, absolute_deadline, timeouts).inspect_err(
            |_| {
                scrolling_output.clear();
            },
        )?;

        match ActionMessenger::request_result(messenger, action_handle, RESULT_POLL_TIMEOUT).await {
            Ok(msg) => {
                let payload = msg.payload();
                match decode_result(&payload) {
                    Ok(Some(result)) => return Ok(result),
                    Ok(None) => {} // "result pending" — keep polling (don't reset idle timer)
                    Err(err) => {
                        scrolling_output.clear();
                        return Err(Error::ExecutionFailed(err));
                    }
                }
            }
            Err(PeppyError::ActionResultTimeout { .. }) => {}
            Err(err) => {
                scrolling_output.clear();
                return Err(Error::ExecutionFailed(format!(
                    "Failed to get action result: {err}"
                )));
            }
        }

        tokio::time::sleep(SLEEP_BETWEEN_POLLS).await;
    }
}

mod impls {
    use super::{ActionFeedbackLike, ActionGoalResponseLike, ActionResultLike};
    use core_node::encoding::{
        NodeAddFeedback, NodeAddGoalResponse, NodeAddResult, NodeBuildFeedback,
        NodeBuildGoalResponse, NodeBuildResult, NodeRunFeedback, NodeRunGoalResponse,
        NodeRunResult,
    };
    use std::path::Path;

    macro_rules! impl_goal_response {
        ($ty:ty) => {
            impl ActionGoalResponseLike for $ty {
                fn decode_payload(data: &[u8]) -> std::result::Result<Self, String> {
                    Self::decode(data).map_err(|e| e.to_string())
                }
                fn accepted(&self) -> bool {
                    self.accepted
                }
                fn rejection_reason(self) -> Option<String> {
                    self.rejection_reason
                }
                fn log_path(&self) -> &Path {
                    &self.log_path
                }
            }
        };
    }
    impl_goal_response!(NodeAddGoalResponse);
    impl_goal_response!(NodeBuildGoalResponse);
    impl_goal_response!(NodeRunGoalResponse);

    macro_rules! impl_feedback {
        ($ty:ty) => {
            impl ActionFeedbackLike for $ty {
                fn decode_payload(data: &[u8]) -> std::result::Result<Self, String> {
                    Self::decode(data).map_err(|e| e.to_string())
                }
                fn line(&self) -> &str {
                    &self.line
                }
                fn is_stderr(&self) -> bool {
                    Self::is_stderr(self)
                }
            }
        };
    }
    impl_feedback!(NodeAddFeedback);
    impl_feedback!(NodeBuildFeedback);
    impl_feedback!(NodeRunFeedback);

    macro_rules! impl_result {
        ($ty:ty) => {
            impl ActionResultLike for $ty {
                fn decode_payload(data: &[u8]) -> std::result::Result<Self, String> {
                    Self::decode(data).map_err(|e| e.to_string())
                }
                fn success(&self) -> bool {
                    self.success
                }
                fn take_error_message(self) -> Option<String> {
                    self.error_message
                }
            }
        };
    }
    impl_result!(NodeAddResult);
    impl_result!(NodeBuildResult);
    impl_result!(NodeRunResult);
}

fn check_timeouts(
    last_activity: tokio::time::Instant,
    idle_timeout: Duration,
    absolute_deadline: tokio::time::Instant,
    timeouts: &TimeoutConfig,
) -> Result<()> {
    let now = tokio::time::Instant::now();
    if now >= absolute_deadline {
        return Err(Error::ExecutionFailed(format!(
            "Timeout: max timeout of {}s exceeded. \
             Use --max-timeout <seconds> to increase.",
            timeouts.max_secs
        )));
    }
    if now.duration_since(last_activity) >= idle_timeout {
        return Err(Error::ExecutionFailed(format!(
            "Timeout: no output received for {}s. \
             Use --idle-timeout <seconds> to increase.",
            timeouts.idle_secs
        )));
    }
    Ok(())
}
