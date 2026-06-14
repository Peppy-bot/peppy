use peppylib::messaging::{ActionGoalHandle, ResultStatus};
use peppylib::{ActionMessenger, MessengerHandle};
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
                output.add_line(feedback.line());
            }
        },
        |payload| {
            Res::decode_payload(payload)
                .map_err(|err| format!("Failed to decode {action_name} result: {err}"))
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

/// Drives an accepted action goal to completion: drains feedback into the
/// scrolling output until the server closes the feedback stream (which it does
/// when the goal `complete`s, or if its worker drops the goal), honoring the
/// idle/max timeouts, then fetches the final result once. The result service
/// rendezvous server-side, so the single `request_result` resolves as soon as
/// the goal has completed instead of needing a poll loop.
///
/// - `on_feedback` is called with each raw feedback payload.
/// - `decode_result` decodes the final result, returning `Err` on failure.
///
/// On timeout or error, the scrolling output is cleared before returning.
pub(crate) async fn poll_action_to_completion<R>(
    messenger: &MessengerHandle,
    action_handle: &mut ActionGoalHandle,
    timeouts: &TimeoutConfig,
    scrolling_output: &mut ScrollingOutput,
    mut on_feedback: impl FnMut(&[u8], &mut ScrollingOutput),
    decode_result: impl Fn(&[u8]) -> std::result::Result<R, String>,
) -> Result<R> {
    let idle_timeout = Duration::from_secs(timeouts.idle_secs);
    let absolute_deadline = tokio::time::Instant::now() + Duration::from_secs(timeouts.max_secs);
    let mut last_activity = tokio::time::Instant::now();

    // Drain feedback until the server closes the stream on completion. The
    // idle / max-timeout budgets bound a goal that goes silent or runs away.
    loop {
        check_timeouts(
            tokio::time::Instant::now(),
            last_activity,
            idle_timeout,
            absolute_deadline,
            timeouts,
        )
        .inspect_err(|_| scrolling_output.clear())?;

        match tokio::time::timeout(FEEDBACK_DRAIN_TIMEOUT, action_handle.on_next_feedback()).await {
            Ok(Ok(msg)) => {
                last_activity = tokio::time::Instant::now();
                on_feedback(&msg.payload(), scrolling_output);
            }
            Ok(Err(_)) => break, // end-of-stream: the goal has completed
            Err(_) => {}         // drain slice elapsed; re-check timeouts and keep draining
        }
    }

    // The goal has completed; fetch its (server-buffered) result once. Give the
    // request the remaining max budget so it resolves promptly.
    let result_timeout = absolute_deadline
        .saturating_duration_since(tokio::time::Instant::now())
        .max(Duration::from_secs(1));
    match ActionMessenger::request_result(messenger, action_handle, result_timeout).await {
        Ok(reply) => match reply.status {
            ResultStatus::Completed | ResultStatus::Cancelled => {
                match decode_result(reply.body.as_ref()) {
                    Ok(result) => Ok(result),
                    Err(err) => {
                        scrolling_output.clear();
                        Err(Error::ExecutionFailed(err))
                    }
                }
            }
            ResultStatus::Abandoned => {
                scrolling_output.clear();
                Err(Error::ExecutionFailed(
                    "the action goal was abandoned by its worker before producing a result"
                        .to_string(),
                ))
            }
            ResultStatus::Expired => {
                scrolling_output.clear();
                Err(Error::ExecutionFailed(
                    "the action result expired before it could be fetched".to_string(),
                ))
            }
        },
        Err(err) => {
            scrolling_output.clear();
            Err(Error::ExecutionFailed(format!(
                "Failed to get action result: {err}"
            )))
        }
    }
}

mod impls {
    use super::{ActionFeedbackLike, ActionGoalResponseLike, ActionResultLike};
    use core_node_api::encoding::{
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

/// Decides whether an action poll loop has exceeded its idle or absolute-max
/// budget. Pure in `now` (the caller passes `tokio::time::Instant::now()`) so
/// the boundary semantics and the exact user-facing messages can be unit-tested
/// with synthetic instants instead of a real clock. The max budget is checked
/// before the idle budget, so a goal that blows both is reported as a max
/// timeout.
fn check_timeouts(
    now: tokio::time::Instant,
    last_activity: tokio::time::Instant,
    idle_timeout: Duration,
    absolute_deadline: tokio::time::Instant,
    timeouts: &TimeoutConfig,
) -> Result<()> {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn timeouts(idle_secs: u64, max_secs: u64) -> TimeoutConfig {
        TimeoutConfig {
            idle_secs,
            max_secs,
        }
    }

    #[test]
    fn within_both_budgets_is_ok() {
        let base = tokio::time::Instant::now();
        let deadline = base + Duration::from_secs(100);
        let now = base + Duration::from_secs(1);
        assert!(
            check_timeouts(
                now,
                base,
                Duration::from_secs(5),
                deadline,
                &timeouts(5, 100)
            )
            .is_ok()
        );
    }

    #[test]
    fn just_under_idle_budget_is_ok() {
        let base = tokio::time::Instant::now();
        let deadline = base + Duration::from_secs(100);
        let now = base + Duration::from_millis(4_999);
        assert!(
            check_timeouts(
                now,
                base,
                Duration::from_secs(5),
                deadline,
                &timeouts(5, 100)
            )
            .is_ok()
        );
    }

    #[test]
    fn reaching_idle_budget_reports_no_output() {
        let base = tokio::time::Instant::now();
        let deadline = base + Duration::from_secs(100);
        // No activity since `base`; `now` is exactly one idle budget later.
        let now = base + Duration::from_secs(5);
        let err = check_timeouts(
            now,
            base,
            Duration::from_secs(5),
            deadline,
            &timeouts(5, 100),
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("no output received for 5s"),
            "got: {err}"
        );
    }

    #[test]
    fn reaching_max_deadline_reports_max_exceeded() {
        let base = tokio::time::Instant::now();
        let deadline = base + Duration::from_secs(100);
        // Activity is current, only the absolute deadline is hit.
        let now = base + Duration::from_secs(100);
        let err = check_timeouts(
            now,
            now,
            Duration::from_secs(5),
            deadline,
            &timeouts(5, 100),
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("max timeout of 100s exceeded"),
            "got: {err}"
        );
    }

    #[test]
    fn max_budget_is_checked_before_idle() {
        let base = tokio::time::Instant::now();
        let deadline = base + Duration::from_secs(100);
        // Both budgets are blown (idle since `base`, past the deadline): the max
        // timeout must be the one reported.
        let now = base + Duration::from_secs(100);
        let err = check_timeouts(
            now,
            base,
            Duration::from_secs(5),
            deadline,
            &timeouts(5, 100),
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("max timeout"),
            "max must be reported before idle; got: {err}"
        );
    }
}
