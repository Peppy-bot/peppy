//! Following one daemon goal from the command line, for `node add`, `node
//! build`, `node run`, `repo refresh` and `stack benchmark`: its feedback in a
//! scrolling output, the idle and max budgets of the command's watchdog, a
//! Ctrl-C, and how the goal ended.
//!
//! The watchdog sends no cancel: a budget that runs out ends the command, and
//! the goal goes on on the daemon. A Ctrl-C sends the daemon a cancel; the
//! command then follows the goal to the end the daemon gives it, says how it
//! ended, and exits with code 130. A second Ctrl-C stops the wait at once.

use peppylib::messaging::{ActionGoalHandle, GoalAnswer, GoalOutcome};
use peppylib::{ActionMessenger, MessengerHandle};
use tracing::{info, warn};

use super::node::TimeoutConfig;
use crate::commands::SCROLLING_OUTPUT_LINES;
use crate::error::{Error, Result};
use crate::terminal::ScrollingOutput;

use std::future::Future;
use std::path::Path;
use std::time::Duration;

/// How long the daemon has to answer a cancel request.
const CANCEL_TIMEOUT: Duration = Duration::from_secs(10);

/// Minimal shape of the goal response of an accepted node goal, accepted by
/// [`run_action_with_feedback`]. Implemented for `NodeAddGoalResponse`,
/// `NodeBuildGoalResponse`, and `NodeRunGoalResponse` in the caller files.
pub(crate) trait ActionGoalResponseLike: Sized {
    fn decode_payload(data: &[u8]) -> std::result::Result<Self, String>;
    fn log_path(&self) -> &Path;
}

/// Minimal shape of an action feedback used by [`run_action_with_feedback`].
pub(crate) trait ActionFeedbackLike: Sized {
    fn decode_payload(data: &[u8]) -> std::result::Result<Self, String>;
    fn line(&self) -> &str;
}

/// Minimal shape of the result of a completed node goal, used by
/// [`run_action_with_feedback`].
pub(crate) trait ActionResultLike: Sized {
    fn decode_payload(data: &[u8]) -> std::result::Result<Self, String>;
}

/// The Ctrl-C presses a command reads while it follows a goal.
pub(crate) trait Interrupts {
    /// Resolves at the next press.
    fn next(&mut self) -> impl Future<Output = ()>;
}

/// The Ctrl-C presses of the process. The first wait installs the handler of
/// SIGINT, so the default action of the signal, which ends the process,
/// holds until the command follows a goal.
pub(crate) struct CtrlC;

impl Interrupts for CtrlC {
    async fn next(&mut self) {
        if let Err(error) = tokio::signal::ctrl_c().await {
            warn!("Ctrl-C cannot be read: {error}");
            std::future::pending::<()>().await;
        }
    }
}

/// What the command calls the goal it follows, and the goal's log file on
/// the daemon's machine, which every failure names.
pub(crate) struct FollowedGoal<'a> {
    /// The goal at the start of a sentence: "Node add", "Repository refresh".
    pub doing: &'a str,
    pub log_path: Option<&'a Path>,
}

impl FollowedGoal<'_> {
    /// " Log file: {path}", or nothing for a goal with no log file.
    fn log_tail(&self) -> String {
        self.log_path
            .map(|path| format!(" Log file: {}", path.display()))
            .unwrap_or_default()
    }
}

/// Drives a node goal that has already been sent to its end: reads the
/// daemon's answer, refused with a failure kind or accepted with its log
/// file, prints the log file, polls feedback into a scrolling output, and
/// gives the result of the completed goal. Shared by `node add`, `node
/// build` and `node run`.
pub(crate) async fn run_action_with_feedback<Resp, Fb, Res>(
    messenger: &MessengerHandle,
    action_handle: &mut ActionGoalHandle,
    timeouts: &TimeoutConfig,
    doing: &str,
) -> Result<Res>
where
    Resp: ActionGoalResponseLike,
    Fb: ActionFeedbackLike,
    Res: ActionResultLike,
{
    let goal_response = match &action_handle.goal_reply().answer {
        GoalAnswer::Accepted(response) => Resp::decode_payload(response.as_ref()).map_err(|e| {
            Error::ExecutionFailed(format!("Failed to decode goal response: {}", e))
        })?,
        GoalAnswer::Refused(refusal) => {
            return Err(Error::ExecutionFailed(crate::commands::failure_text(
                &format!("{doing} refused"),
                refusal,
            )));
        }
    };

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
                .map_err(|err| format!("Failed to decode the result: {err}"))
        },
        &FollowedGoal {
            doing,
            log_path: Some(goal_response.log_path()),
        },
        &mut CtrlC,
    )
    .await?;

    scrolling_output.clear();
    Ok(result)
}

/// How long one drain slice waits on feedback before the timeouts are
/// re-checked.
pub(crate) const FEEDBACK_DRAIN_TIMEOUT: Duration = Duration::from_millis(50);

/// How far ahead a deadline lies when its budget is too large for the clock to
/// hold.
const UNREACHABLE_DEADLINE_DISTANCE: Duration = Duration::from_secs(86_400 * 365 * 30);

/// The instant `budget` after `start`. A budget the clock cannot represent (a
/// `--max-timeout` near `u64::MAX` seconds) gives an instant no command
/// reaches, instead of an overflow panic after the goal is already sent.
pub(crate) fn deadline_after(
    start: tokio::time::Instant,
    budget: Duration,
) -> tokio::time::Instant {
    start
        .checked_add(budget)
        .unwrap_or(start + UNREACHABLE_DEADLINE_DISTANCE)
}

/// Drives an accepted action goal to its end: drains feedback into the
/// scrolling output until the daemon closes the feedback stream (which it
/// does when it ends the goal), honoring the idle/max timeouts, then fetches
/// the goal's end once. The result service rendezvous server-side, so the
/// single request resolves as soon as the goal has ended instead of needing
/// a poll loop.
///
/// - `on_feedback` is called with each raw feedback payload.
/// - `decode_result` decodes the result of a completed goal, returning `Err`
///   on failure.
/// - A failed goal ends the command with its failure kind, its message and
///   the goal's log file; a cancelled goal with its reason.
/// - The first press `interrupts` gives sends the daemon a cancel, and the
///   command follows the goal to its end, says how it ended, and ends with
///   [`Error::Interrupted`]. A second press stops the wait at once.
///
/// On timeout or error, the scrolling output is cleared before returning.
#[allow(clippy::too_many_arguments)] // Distinct inputs; bundling them would only move the list.
pub(crate) async fn poll_action_to_completion<R>(
    messenger: &MessengerHandle,
    action_handle: &mut ActionGoalHandle,
    timeouts: &TimeoutConfig,
    scrolling_output: &mut ScrollingOutput,
    mut on_feedback: impl FnMut(&[u8], &mut ScrollingOutput),
    decode_result: impl Fn(&[u8]) -> std::result::Result<R, String>,
    goal: &FollowedGoal<'_>,
    interrupts: &mut impl Interrupts,
) -> Result<R> {
    let idle_timeout = Duration::from_secs(timeouts.idle_secs);
    let absolute_deadline = deadline_after(
        tokio::time::Instant::now(),
        Duration::from_secs(timeouts.max_secs),
    );
    let mut last_activity = tokio::time::Instant::now();
    let mut cancel_sent = false;

    // Drain feedback until the daemon closes the stream as it ends the goal.
    // The idle / max-timeout budgets bound a goal that goes silent or runs
    // away; they send no cancel.
    loop {
        check_timeouts(
            tokio::time::Instant::now(),
            last_activity,
            idle_timeout,
            absolute_deadline,
            timeouts,
        )
        .inspect_err(|_| scrolling_output.clear())?;

        let next = tokio::select! {
            biased;
            () = interrupts.next() => {
                if cancel_sent {
                    scrolling_output.clear();
                    return Err(Error::Interrupted(format!(
                        "Stopped waiting: the daemon goes on with the cancel of the {}.{}",
                        goal.doing.to_lowercase(),
                        goal.log_tail()
                    )));
                }
                cancel_sent = true;
                send_cancel(messenger, action_handle, goal).await;
                continue;
            }
            next = tokio::time::timeout(FEEDBACK_DRAIN_TIMEOUT, action_handle.on_next_feedback()) => next,
        };
        match next {
            Ok(Ok(msg)) => {
                last_activity = tokio::time::Instant::now();
                on_feedback(msg.payload_bytes().as_ref(), scrolling_output);
            }
            Ok(Err(_)) => break, // end-of-stream: the daemon ended the goal
            Err(_) => {}         // drain slice elapsed; re-check timeouts and keep draining
        }
    }

    // The goal has ended; fetch its (server-buffered) end once. Give the
    // request the remaining max budget so it resolves promptly.
    let result_timeout = absolute_deadline
        .saturating_duration_since(tokio::time::Instant::now())
        .max(Duration::from_secs(1));
    let reply = ActionMessenger::request_result(messenger, action_handle, result_timeout)
        .await
        .inspect_err(|_| scrolling_output.clear())
        .map_err(|e| {
            Error::ExecutionFailed(format!(
                "Failed to get how the {} ended: {e}.{}",
                goal.doing.to_lowercase(),
                goal.log_tail()
            ))
        })?;
    scrolling_output.clear();
    let ended = goal_end(goal, reply.outcome, decode_result);
    if !cancel_sent {
        return ended;
    }
    // The command ends with code 130 whichever way the goal ended: the
    // operator pressed Ctrl-C.
    let how = match ended {
        Ok(_) => format!(
            "{} completed before the cancel took effect.{}",
            goal.doing,
            goal.log_tail()
        ),
        Err(error) => error.to_string(),
    };
    Err(Error::Interrupted(how))
}

/// Sends the daemon a cancel of the goal `action_handle` follows, after a
/// line that says so; a cancel that does not reach the daemon is a warning.
async fn send_cancel(
    messenger: &MessengerHandle,
    action_handle: &ActionGoalHandle,
    goal: &FollowedGoal<'_>,
) {
    warn!(
        "Cancelling the {}; press Ctrl-C again to stop waiting for its end",
        goal.doing.to_lowercase()
    );
    if let Err(error) = ActionMessenger::cancel_goal(messenger, action_handle, CANCEL_TIMEOUT).await
    {
        warn!(
            "The cancel of the {} did not reach the daemon: {error}",
            goal.doing.to_lowercase()
        );
    }
}

/// How the command ends for a goal that ended with `outcome`: the result of
/// a completed goal, or the error that says how the goal ended otherwise.
fn goal_end<R>(
    goal: &FollowedGoal<'_>,
    outcome: GoalOutcome,
    decode_result: impl Fn(&[u8]) -> std::result::Result<R, String>,
) -> Result<R> {
    let log_tail = goal.log_tail();
    match outcome {
        GoalOutcome::Completed(payload) => {
            decode_result(payload.as_ref()).map_err(Error::ExecutionFailed)
        }
        GoalOutcome::Failed(failure) => Err(Error::ExecutionFailed(format!(
            "{}.{log_tail}",
            crate::commands::failure_text(&format!("{} failed", goal.doing), &failure)
        ))),
        GoalOutcome::Cancelled { reason, .. } => Err(Error::ExecutionFailed(format!(
            "{} was {reason}.{log_tail}",
            goal.doing
        ))),
        GoalOutcome::Abandoned => Err(Error::ExecutionFailed(format!(
            "{} was abandoned by the daemon before it ended.{log_tail}",
            goal.doing
        ))),
        GoalOutcome::Expired => Err(Error::ExecutionFailed(format!(
            "{} ended, and its end expired before the command fetched it.{log_tail}",
            goal.doing
        ))),
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
    let raise = |flag: &str| {
        if timeouts.flags {
            format!(" Use {flag} <seconds> to increase.")
        } else {
            String::new()
        }
    };
    if now >= absolute_deadline {
        return Err(Error::ExecutionFailed(format!(
            "Timeout: max timeout of {}s exceeded.{}",
            timeouts.max_secs,
            raise("--max-timeout")
        )));
    }
    if now.duration_since(last_activity) >= idle_timeout {
        return Err(Error::ExecutionFailed(format!(
            "Timeout: no output received for {}s.{}",
            timeouts.idle_secs,
            raise("--idle-timeout")
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
            flags: true,
        }
    }

    #[test]
    fn deadline_after_saturates_a_budget_the_clock_cannot_hold() {
        let start = tokio::time::Instant::now();

        assert!(
            deadline_after(start, Duration::from_secs(u64::MAX))
                > start + Duration::from_secs(86_400 * 365)
        );
        assert_eq!(
            deadline_after(start, Duration::from_secs(5)),
            start + Duration::from_secs(5)
        );
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
