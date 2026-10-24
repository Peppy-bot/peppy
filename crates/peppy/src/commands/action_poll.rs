//! Following one daemon goal from the command line, for `node add`, `node
//! build`, `node run`, `repo refresh` and `stack benchmark`: its feedback in a
//! scrolling output, the idle and max budgets of the command's watchdog, a
//! Ctrl-C, and how the goal ended.
//!
//! The watchdog sends no cancel: a budget that runs out ends the command, and
//! its process exits. The daemon then finds that the sender of the goal is
//! gone and cancels the goal, with the reason "its sender is gone". A Ctrl-C
//! sends the daemon a cancel; the command then follows the goal to the end
//! the daemon gives it, under the max budget alone, says how it ended, and
//! exits with code 130. A second Ctrl-C stops the wait at once, also while
//! the cancel is on its way to the daemon.

use peppylib::messaging::{ActionGoalHandle, ActionResultReply, GoalAnswer, GoalOutcome};
use peppylib::{ActionMessenger, Message, MessengerHandle, PeppyError};
use tokio::sync::mpsc;
use tracing::{info, warn};

use super::node::TimeoutConfig;
use crate::commands::SCROLLING_OUTPUT_LINES;
use crate::error::{Error, INTERRUPTED_EXIT_CODE, Result};
use crate::terminal::ScrollingOutput;

use std::future::Future;
use std::path::Path;
use std::sync::{Mutex, OnceLock};
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
    /// Resolves at the next press. A press that comes while no call waits
    /// is kept for the next call.
    fn next(&mut self) -> impl Future<Output = ()>;
}

/// The Ctrl-C presses of the process for one follow of a goal, from
/// [`CtrlC::listen`] until the value is dropped.
///
/// The first follow of the process starts the one listener of SIGINT that
/// the process keeps from then on, on a thread of its own. A press goes to
/// the follow that holds a [`CtrlC`], which keeps the press until the
/// follow reads it: no press is lost while the follow sends a cancel or
/// reads the end of its goal. A press while no follow holds a [`CtrlC`],
/// between two goals of one command or after the last one, ends the process
/// with code 130.
pub(crate) struct CtrlC {
    presses: mpsc::UnboundedReceiver<()>,
    /// The sender that [`FOLLOW`] holds while this follow reads the presses.
    own: mpsc::UnboundedSender<()>,
}

/// The sender of the presses to the follow that reads them at this moment,
/// if a follow does.
static FOLLOW: Mutex<Option<mpsc::UnboundedSender<()>>> = Mutex::new(None);

/// Set once the first follow started the listener of SIGINT.
static SIGINT_LISTENER: OnceLock<()> = OnceLock::new();

impl CtrlC {
    /// Starts to read the presses of the process for one follow.
    pub(crate) fn listen() -> Self {
        let (own, presses) = mpsc::unbounded_channel();
        *follow_slot() = Some(own.clone());
        SIGINT_LISTENER.get_or_init(start_sigint_listener);
        Self { presses, own }
    }
}

impl Interrupts for CtrlC {
    async fn next(&mut self) {
        self.presses
            .recv()
            .await
            .expect("a CtrlC holds a sender of its own presses");
    }
}

impl Drop for CtrlC {
    fn drop(&mut self) {
        let mut follow = follow_slot();
        if follow
            .as_ref()
            .is_some_and(|press| press.same_channel(&self.own))
        {
            *follow = None;
        }
    }
}

/// The follow slot. A panic while it is held leaves nothing half written,
/// so a poisoned lock is read as it is.
fn follow_slot() -> std::sync::MutexGuard<'static, Option<mpsc::UnboundedSender<()>>> {
    FOLLOW
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Starts the thread that reads each SIGINT of the process and hands it on
/// ([`route_press`]). The default action of SIGINT, which ends the process,
/// holds until this runs. The listener is registered before this returns,
/// and it runs on a runtime of its own: it outlives the runtime of each
/// command step, which [`crate::commands::block_on`] can make and drop.
fn start_sigint_listener() {
    if let Err(error) = spawn_sigint_thread() {
        warn!("Ctrl-C cannot be read: {error}");
    }
}

/// Registers the listener of SIGINT on a runtime of its own, and drives it
/// on a thread of its own for the rest of the process.
fn spawn_sigint_thread() -> std::io::Result<()> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .build()?;
    let mut sigint = {
        let _runtime_context = runtime.enter();
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?
    };
    std::thread::Builder::new()
        .name("peppy-sigint".to_owned())
        .spawn(move || {
            runtime.block_on(async move {
                while sigint.recv().await.is_some() {
                    if route_press() == PressRoute::EndsTheProcess {
                        std::process::exit(INTERRUPTED_EXIT_CODE);
                    }
                }
            });
        })?;
    Ok(())
}

/// Where a press of Ctrl-C goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PressRoute {
    /// To the follow that reads the presses at this moment.
    ToTheFollow,
    /// No follow reads them: the press ends the process.
    EndsTheProcess,
}

/// Hands a press to the follow that reads the presses at this moment, if a
/// follow does.
fn route_press() -> PressRoute {
    match follow_slot().as_ref() {
        Some(press) if press.send(()).is_ok() => PressRoute::ToTheFollow,
        _ => PressRoute::EndsTheProcess,
    }
}

/// What a command calls the goal it follows, and the goal's log file on
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

    /// The line a command writes when a Ctrl-C sends the daemon a cancel.
    pub(crate) fn announce_cancel(&self) {
        warn!(
            "Cancelling the {}; press Ctrl-C again to stop waiting for its end",
            self.doing.to_lowercase()
        );
    }

    /// The end of a command whose operator pressed Ctrl-C a second time.
    pub(crate) fn stopped_waiting(&self) -> Error {
        Error::Interrupted(format!(
            "Stopped waiting: the daemon goes on with the cancel of the {}.{}",
            self.doing.to_lowercase(),
            self.log_tail()
        ))
    }

    /// The end of a command that fell behind the output of its goal by more
    /// than `bound` unread messages: the handle of the goal cancelled it.
    pub(crate) fn fell_behind(&self, bound: usize) -> Error {
        Error::ExecutionFailed(format!(
            "The command fell behind the output of the {} by more than {bound} unread messages \
             and cancelled it.{}",
            self.doing.to_lowercase(),
            self.log_tail()
        ))
    }
}

/// `error` as the end of a follow that sent the daemon a cancel at a
/// Ctrl-C when `cancel_sent`: the command then ends with code 130, whichever
/// way the follow ended.
pub(crate) fn end_of_follow(cancel_sent: bool, error: Error) -> Error {
    if !cancel_sent {
        return error;
    }
    match error {
        Error::Interrupted(_) => error,
        other => Error::Interrupted(other.to_string()),
    }
}

/// What came first after a Ctrl-C started a cancel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CancelRace {
    /// The cancel request ended: the daemon answered it, or it failed.
    Ended,
    /// A second Ctrl-C: the command stops waiting.
    StoppedWaiting,
}

/// Runs `cancel`, the request that sends the daemon a cancel of the goal,
/// on a task of its own, and waits for it against the next press of
/// `interrupts`: a second Ctrl-C stops the wait at once, also while the
/// request is on its way, and the request goes on.
pub(crate) async fn cancel_unless_stopped(
    cancel: impl Future<Output = ()> + Send + 'static,
    interrupts: &mut impl Interrupts,
) -> CancelRace {
    let request = tokio::spawn(cancel);
    tokio::select! {
        biased;
        _ = request => CancelRace::Ended,
        () = interrupts.next() => CancelRace::StoppedWaiting,
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
        &mut CtrlC::listen(),
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
/// the goal's end. The result service rendezvous server-side, so one request
/// resolves as soon as the goal has ended instead of needing a poll loop.
///
/// - `on_feedback` is called with each raw feedback payload.
/// - `decode_result` decodes the result of a completed goal, returning `Err`
///   on failure.
/// - A failed goal ends the command with its failure kind, its message and
///   the goal's log file; a cancelled goal with its reason.
/// - A command that falls behind the output of the goal loses the stream:
///   the handle of the goal cancels it, and the command ends with an error
///   that says so.
/// - The first press `interrupts` gives sends the daemon a cancel, and the
///   command follows the goal to its end under the max budget alone, says
///   how it ended, and ends with [`Error::Interrupted`]. A second press
///   stops the wait at once.
///
/// The scrolling output is cleared before returning.
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
    let followed = follow_goal(
        messenger,
        action_handle,
        timeouts,
        scrolling_output,
        &mut on_feedback,
        goal,
        interrupts,
    )
    .await;
    scrolling_output.clear();
    let Followed {
        outcome,
        cancel_sent,
    } = followed?;
    let ended = goal_end(goal, outcome, decode_result);
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

/// A goal followed to its end, and whether the operator's Ctrl-C sent the
/// daemon a cancel on the way.
struct Followed {
    outcome: GoalOutcome,
    cancel_sent: bool,
}

/// Follows the goal of `action_handle` to its end, which it returns (see
/// [`poll_action_to_completion`]). Every error after a Ctrl-C is an
/// [`Error::Interrupted`].
async fn follow_goal(
    messenger: &MessengerHandle,
    action_handle: &mut ActionGoalHandle,
    timeouts: &TimeoutConfig,
    scrolling_output: &mut ScrollingOutput,
    on_feedback: &mut impl FnMut(&[u8], &mut ScrollingOutput),
    goal: &FollowedGoal<'_>,
    interrupts: &mut impl Interrupts,
) -> Result<Followed> {
    let idle_timeout = Duration::from_secs(timeouts.idle_secs);
    let absolute_deadline = deadline_after(
        tokio::time::Instant::now(),
        Duration::from_secs(timeouts.max_secs),
    );
    let mut last_activity = tokio::time::Instant::now();
    let mut feedback_open = true;
    let mut cancel_sent = false;

    loop {
        // After a Ctrl-C the daemon rolls the work back, which can be silent
        // for long: the goal is followed to its end under the max budget
        // alone.
        let now = tokio::time::Instant::now();
        let within_budgets = if cancel_sent {
            check_max_timeout(now, absolute_deadline, timeouts)
        } else {
            check_timeouts(
                now,
                last_activity,
                idle_timeout,
                absolute_deadline,
                timeouts,
            )
        };
        within_budgets.map_err(|error| end_of_follow(cancel_sent, error))?;

        let event = tokio::select! {
            biased;
            () = interrupts.next() => {
                if cancel_sent {
                    return Err(goal.stopped_waiting());
                }
                cancel_sent = true;
                scrolling_output.clear();
                goal.announce_cancel();
                let cancel = cancel_request(messenger, action_handle, goal);
                if cancel_unless_stopped(cancel, interrupts).await == CancelRace::StoppedWaiting {
                    return Err(goal.stopped_waiting());
                }
                continue;
            }
            event = next_goal_event(messenger, action_handle, &mut feedback_open, absolute_deadline) => event,
        };
        match event {
            GoalEvent::Feedback(message) => {
                last_activity = tokio::time::Instant::now();
                on_feedback(message.payload_bytes().as_ref(), scrolling_output);
            }
            GoalEvent::Quiet => {}
            GoalEvent::FeedbackEnded => last_activity = tokio::time::Instant::now(),
            GoalEvent::FellBehind { bound } => {
                return Err(end_of_follow(cancel_sent, goal.fell_behind(bound)));
            }
            GoalEvent::Ended(Ok(reply)) => {
                return Ok(Followed {
                    outcome: reply.outcome,
                    cancel_sent,
                });
            }
            GoalEvent::Ended(Err(error)) => {
                let error = Error::ExecutionFailed(format!(
                    "Failed to get how the {} ended: {error}.{}",
                    goal.doing.to_lowercase(),
                    goal.log_tail()
                ));
                return Err(end_of_follow(cancel_sent, error));
            }
        }
    }
}

/// One event of a followed goal.
enum GoalEvent {
    /// A feedback message.
    Feedback(Message),
    /// A drain slice passed with no message.
    Quiet,
    /// The daemon closed the feedback, which it does when it ends the goal.
    FeedbackEnded,
    /// The command fell behind the feedback by more than `bound` unread
    /// messages, and the handle of the goal cancelled it.
    FellBehind { bound: usize },
    /// The reply to the request for the end of the goal.
    Ended(std::result::Result<ActionResultReply, PeppyError>),
}

/// The next event of the goal of `action_handle`: a feedback message, a
/// drain slice with none, the end of the feedback, or, once the feedback
/// ended, the reply to the request for the end of the goal, which has the
/// rest of the max budget. Cancel safe: a dropped call leaves `feedback_open`
/// as it was, and the next call asks again.
async fn next_goal_event(
    messenger: &MessengerHandle,
    action_handle: &mut ActionGoalHandle,
    feedback_open: &mut bool,
    absolute_deadline: tokio::time::Instant,
) -> GoalEvent {
    if *feedback_open {
        return match tokio::time::timeout(FEEDBACK_DRAIN_TIMEOUT, action_handle.on_next_feedback())
            .await
        {
            Err(_) => GoalEvent::Quiet,
            Ok(Ok(message)) => GoalEvent::Feedback(message),
            Ok(Err(PeppyError::ActionFeedbackOverflow { bound, .. })) => {
                GoalEvent::FellBehind { bound }
            }
            // The end of the stream: the daemon ended the goal.
            Ok(Err(_)) => {
                *feedback_open = false;
                GoalEvent::FeedbackEnded
            }
        };
    }
    let result_timeout = absolute_deadline
        .saturating_duration_since(tokio::time::Instant::now())
        .max(Duration::from_secs(1));
    GoalEvent::Ended(
        ActionMessenger::request_result(messenger, action_handle, result_timeout).await,
    )
}

/// The request that sends the daemon a cancel of the goal `action_handle`
/// follows, which borrows nothing; a cancel that does not reach the daemon
/// is a warning.
fn cancel_request(
    messenger: &MessengerHandle,
    action_handle: &ActionGoalHandle,
    goal: &FollowedGoal<'_>,
) -> impl Future<Output = ()> + Send + 'static {
    let messenger = messenger.clone();
    let sender = action_handle.sender().clone();
    let goal_id = action_handle.goal_id().to_owned();
    let doing = goal.doing.to_lowercase();
    async move {
        let sent =
            ActionMessenger::cancel_with_sender(&messenger, &sender, &goal_id, CANCEL_TIMEOUT)
                .await;
        if let Err(error) = sent {
            warn!("The cancel of the {doing} did not reach the daemon: {error}");
        }
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
    check_max_timeout(now, absolute_deadline, timeouts)?;
    if now.duration_since(last_activity) >= idle_timeout {
        return Err(Error::ExecutionFailed(format!(
            "Timeout: no output received for {}s.{}",
            timeouts.idle_secs,
            raise_hint(timeouts, "--idle-timeout")
        )));
    }
    Ok(())
}

/// Decides whether an action poll loop has exceeded its absolute-max budget,
/// as [`check_timeouts`] does.
fn check_max_timeout(
    now: tokio::time::Instant,
    absolute_deadline: tokio::time::Instant,
    timeouts: &TimeoutConfig,
) -> Result<()> {
    if now >= absolute_deadline {
        return Err(Error::ExecutionFailed(format!(
            "Timeout: max timeout of {}s exceeded.{}",
            timeouts.max_secs,
            raise_hint(timeouts, "--max-timeout")
        )));
    }
    Ok(())
}

/// " Use {flag} <seconds> to increase.", for a command whose budgets have
/// flags.
fn raise_hint(timeouts: &TimeoutConfig, flag: &str) -> String {
    if timeouts.flags {
        format!(" Use {flag} <seconds> to increase.")
    } else {
        String::new()
    }
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

/// What the tests of a follow share: the presses of Ctrl-C that a test
/// gives, and a daemon's goal engine over the in-process mock messaging.
#[cfg(test)]
pub(crate) mod test_support {
    use super::Interrupts;
    use core_node_api::names::CORE_NODE_TAG;
    use peppylib::MessengerHandle;
    use peppylib::messaging::{ConcurrentAction, PendingGoal, SenderTarget};
    use pmi::{Messenger, MessengerAdapter, MessengerBackend, MockAdapter};
    use std::sync::Arc;
    use tokio::sync::{Mutex, mpsc};

    /// Ctrl-C presses that the test gives, one at a time.
    pub(crate) struct Presses(mpsc::UnboundedReceiver<()>);

    impl Interrupts for Presses {
        async fn next(&mut self) {
            if self.0.recv().await.is_none() {
                std::future::pending::<()>().await;
            }
        }
    }

    /// `count` presses, given before the command follows its goal.
    pub(crate) fn presses(count: usize) -> Presses {
        let (press, presses) = mpsc::unbounded_channel();
        for _ in 0..count {
            press.send(()).expect("the command reads the press");
        }
        Presses(presses)
    }

    /// A daemon of `core_node` that exposes `action` on a mock messenger,
    /// and the messenger a command uses to reach it.
    pub(crate) async fn daemon_exposing(
        core_node: &str,
        action: &str,
    ) -> (MessengerHandle, ConcurrentAction) {
        let mut messenger = Messenger::new(MessengerAdapter::Mock(MockAdapter::default()));
        messenger
            .start_session()
            .await
            .expect("the mock session starts");
        let messenger = MessengerHandle::from_shared(Arc::new(Mutex::new(messenger)));
        let engine = ConcurrentAction::expose(
            &messenger,
            core_node,
            "daemon",
            SenderTarget::node(core_node, CORE_NODE_TAG).expect("a core node identity"),
            action,
            true,
        )
        .await
        .expect("the daemon exposes its action");
        (messenger, engine)
    }

    /// The daemon's next pending goal.
    pub(crate) async fn next_goal(engine: &mut ConcurrentAction) -> PendingGoal {
        engine
            .recv_next_goal()
            .await
            .expect("the goal service runs")
            .expect("the goal arrives")
    }
}

/// The end of a goal as the command line says it, and the Ctrl-C path, on
/// a daemon's goal engine over the in-process mock messaging. The presses
/// come from the test, at fixed points.
#[cfg(test)]
mod ctrl_c {
    use super::test_support::{Presses, daemon_exposing, next_goal, presses};
    use super::*;
    use config::failure_kind::PeppyFailureKind;
    use core_node_api::encoding::{
        NodeAddFeedback, NodeAddGoal, NodeAddGoalResponse, NodeAddResult,
    };
    use core_node_api::names::CORE_NODE_TAG;
    use peppylib::messaging::{
        CancelCause, ConcurrentAction, Failure, FeedbackBound, GoalContext, SenderTarget,
    };
    use peppylib::types::Payload;

    const CORE_NODE: &str = "cn-ctrl-c";
    const LOG_PATH: &str = "/logs/add/source_20260101_000000_000.log";

    /// A daemon that exposes `node_add` on a mock messenger, and the
    /// messenger a command uses to reach it.
    async fn daemon() -> (MessengerHandle, ConcurrentAction) {
        daemon_exposing(CORE_NODE, core_node_api::ActionId::NodeAdd.name()).await
    }

    /// The daemon accepts its next goal with the add log [`LOG_PATH`].
    fn accept_next(
        mut engine: ConcurrentAction,
    ) -> tokio::task::JoinHandle<(GoalContext, ConcurrentAction)> {
        tokio::spawn(async move {
            let context = next_goal(&mut engine)
                .await
                .accept(
                    NodeAddGoalResponse::new(LOG_PATH)
                        .encode()
                        .expect("the goal response encodes"),
                )
                .await
                .expect("the daemon accepts the goal");
            (context, engine)
        })
    }

    /// Sends a `node_add` goal from the command, whose handle keeps one
    /// unread feedback message.
    async fn send_node_add_keeping_one_message(messenger: &MessengerHandle) -> ActionGoalHandle {
        ActionMessenger::send_goal(
            messenger,
            CORE_NODE,
            "peppy-cli-test",
            SenderTarget::node(CORE_NODE, CORE_NODE_TAG).expect("a core node identity"),
            core_node_api::ActionId::NodeAdd.name(),
            None,
            NodeAddGoal::new("/tmp/source", "hash", 60)
                .encode()
                .expect("the goal encodes"),
            FeedbackBound::new(std::num::NonZeroUsize::MIN),
            Duration::from_secs(5),
        )
        .await
        .expect("the command sends the goal")
    }

    /// Sends a `node_add` goal from the command.
    async fn send_node_add(messenger: &MessengerHandle) -> ActionGoalHandle {
        peppylib::core_node::transport::send_goal(
            &NodeAddGoal::new("/tmp/source", "hash", 60),
            messenger,
            CORE_NODE,
            "peppy-cli-test",
            Some(CORE_NODE),
            Duration::from_secs(5),
        )
        .await
        .expect("the command sends the goal")
    }

    /// The budgets of a command that waits an hour for anything.
    const HOUR_LONG: TimeoutConfig = TimeoutConfig {
        idle_secs: 3600,
        max_secs: 3600,
        flags: true,
    };

    /// Follows `handle` as `node add` does, with `presses`.
    async fn follow(
        messenger: &MessengerHandle,
        handle: &mut ActionGoalHandle,
        presses: &mut Presses,
    ) -> Result<NodeAddResult> {
        follow_under(messenger, handle, &HOUR_LONG, presses).await
    }

    /// Follows `handle` as `node add` does, under `timeouts`, with
    /// `presses`.
    async fn follow_under(
        messenger: &MessengerHandle,
        handle: &mut ActionGoalHandle,
        timeouts: &TimeoutConfig,
        presses: &mut Presses,
    ) -> Result<NodeAddResult> {
        let goal = FollowedGoal {
            doing: "Node add",
            log_path: Some(Path::new(LOG_PATH)),
        };
        poll_action_to_completion(
            messenger,
            handle,
            timeouts,
            &mut ScrollingOutput::new(SCROLLING_OUTPUT_LINES),
            |_, _| {},
            |payload| NodeAddResult::decode(payload).map_err(|error| error.to_string()),
            &goal,
            presses,
        )
        .await
    }

    /// A Ctrl-C sends the daemon a cancel. The command follows the goal to
    /// the end the daemon gives it, says how it ended, and exits with code
    /// 130.
    #[tokio::test]
    async fn a_ctrl_c_cancels_the_goal_and_the_command_says_how_it_ended() {
        let (messenger, engine) = daemon().await;
        let accepted = accept_next(engine);
        let mut handle = send_node_add(&messenger).await;
        let (context, _engine) = accepted.await.expect("the daemon does not panic");

        let mut presses = presses(1);
        let (ended, ()) = tokio::join!(follow(&messenger, &mut handle, &mut presses), async {
            context.cancel_signal().await;
            assert_eq!(context.cancel_cause(), Some(CancelCause::Caller));
            context
                .complete_cancelled(CancelCause::Caller.reason(), Payload::new())
                .await
                .expect("the daemon ends the goal cancelled");
        });

        let error = ended.expect_err("the command ends with the cancel");
        assert_eq!(error.exit_code(), 130);
        assert_eq!(
            error.to_string(),
            format!("Node add was cancelled by its caller. Log file: {LOG_PATH}")
        );
    }

    /// A goal that completes before the cancel takes effect still ends the
    /// command with code 130, and the command says that it completed.
    #[tokio::test]
    async fn a_goal_that_completes_before_the_cancel_still_ends_the_command_with_130() {
        let (messenger, engine) = daemon().await;
        let accepted = accept_next(engine);
        let mut handle = send_node_add(&messenger).await;
        let (context, _engine) = accepted.await.expect("the daemon does not panic");

        let mut presses = presses(1);
        let (ended, ()) = tokio::join!(follow(&messenger, &mut handle, &mut presses), async {
            context.cancel_signal().await;
            context
                .complete(
                    NodeAddResult::new("source", "v1")
                        .encode()
                        .expect("the result encodes"),
                )
                .await
                .expect("the daemon completes the goal");
        });

        let error = ended.expect_err("the command ends with the Ctrl-C");
        assert_eq!(error.exit_code(), 130);
        assert_eq!(
            error.to_string(),
            format!("Node add completed before the cancel took effect. Log file: {LOG_PATH}")
        );
    }

    /// A second Ctrl-C stops the wait at once, also while the cancel that
    /// the first one sent is on its way to the daemon: here the second press
    /// is there before the daemon answers the cancel. The daemon goes on
    /// with the cancel.
    #[tokio::test]
    async fn a_second_ctrl_c_stops_the_wait_while_the_cancel_is_on_its_way() {
        let (messenger, engine) = daemon().await;
        let accepted = accept_next(engine);
        let mut handle = send_node_add(&messenger).await;
        let (context, _engine) = accepted.await.expect("the daemon does not panic");

        let error = follow(&messenger, &mut handle, &mut presses(2))
            .await
            .expect_err("the second press stops the wait");

        assert_eq!(error.exit_code(), 130);
        assert_eq!(
            error.to_string(),
            format!(
                "Stopped waiting: the daemon goes on with the cancel of the node add. \
                 Log file: {LOG_PATH}"
            )
        );
        tokio::time::timeout(Duration::from_secs(30), context.cancel_signal())
            .await
            .expect("the cancel that the first press sent reaches the daemon");
        assert_eq!(context.cancel_cause(), Some(CancelCause::Caller));
    }

    /// After a Ctrl-C the daemon rolls the work back, which can be silent
    /// for longer than the idle budget: the command follows the goal to its
    /// end under the max budget alone, says how it ended, and exits with
    /// code 130. The daemon is silent for three idle budgets on the paused
    /// clock.
    #[tokio::test(start_paused = true)]
    async fn after_a_ctrl_c_a_silent_rollback_is_followed_to_its_end() {
        let (messenger, engine) = daemon().await;
        let accepted = accept_next(engine);
        let mut handle = send_node_add(&messenger).await;
        let (context, _engine) = accepted.await.expect("the daemon does not panic");
        let timeouts = TimeoutConfig {
            idle_secs: 5,
            max_secs: 3600,
            flags: true,
        };

        let mut presses = presses(1);
        let (ended, ()) = tokio::join!(
            follow_under(&messenger, &mut handle, &timeouts, &mut presses),
            async {
                context.cancel_signal().await;
                tokio::time::sleep(Duration::from_secs(3 * timeouts.idle_secs)).await;
                context
                    .complete_cancelled(CancelCause::Caller.reason(), Payload::new())
                    .await
                    .expect("the daemon ends the goal cancelled");
            }
        );

        let error = ended.expect_err("the command ends with the cancel");
        assert_eq!(error.exit_code(), 130);
        assert_eq!(
            error.to_string(),
            format!("Node add was cancelled by its caller. Log file: {LOG_PATH}")
        );
    }

    /// After a Ctrl-C the max budget still bounds the wait, and the command
    /// that it ends exits with code 130, as every command that a Ctrl-C
    /// cancelled does.
    #[tokio::test(start_paused = true)]
    async fn after_a_ctrl_c_the_max_budget_ends_the_wait_with_130() {
        let (messenger, engine) = daemon().await;
        let accepted = accept_next(engine);
        let mut handle = send_node_add(&messenger).await;
        let (context, _engine) = accepted.await.expect("the daemon does not panic");
        let timeouts = TimeoutConfig {
            idle_secs: 5,
            max_secs: 60,
            flags: true,
        };

        let error = follow_under(&messenger, &mut handle, &timeouts, &mut presses(1))
            .await
            .expect_err("the max budget ends the wait");

        assert!(context.is_cancelled(), "the press sent the cancel");
        assert_eq!(error.exit_code(), 130);
        assert_eq!(
            error.to_string(),
            "Timeout: max timeout of 60s exceeded. Use --max-timeout <seconds> to increase."
        );
    }

    /// A command that falls behind the output of its goal by more than the
    /// bound of its feedback loses the stream: its handle cancels the goal,
    /// and the command says that it fell behind and cancelled the goal, with
    /// exit code 1. The goal here keeps one unread message; the daemon sends
    /// two before the command reads one, and the test waits for the cancel
    /// that the overflow sends.
    #[tokio::test]
    async fn a_command_that_falls_behind_the_output_says_it_cancelled_the_goal() {
        let (messenger, engine) = daemon().await;
        let accepted = accept_next(engine);
        let mut handle = send_node_add_keeping_one_message(&messenger).await;
        let (context, _engine) = accepted.await.expect("the daemon does not panic");
        let line = || {
            NodeAddFeedback::stdout("Cloning https://example.com/hub.git")
                .encode()
                .expect("the feedback encodes")
        };
        context
            .publish_feedback(line())
            .await
            .expect("the daemon publishes");
        context
            .publish_feedback(line())
            .await
            .expect("the daemon publishes");
        tokio::time::timeout(Duration::from_secs(30), context.cancel_signal())
            .await
            .expect("the overflow cancels the goal");

        let error = follow(&messenger, &mut handle, &mut presses(0))
            .await
            .expect_err("the command fell behind");

        assert_eq!(error.exit_code(), 1);
        assert_eq!(
            error.to_string(),
            format!(
                "The command fell behind the output of the node add by more than 1 unread \
                 messages and cancelled it. Log file: {LOG_PATH}"
            )
        );
    }

    /// A goal that fails ends the command with the kind and the message of
    /// its failure and the goal's log file, and exit code 1.
    #[tokio::test]
    async fn a_failed_goal_gives_its_kind_to_the_command() {
        let (messenger, engine) = daemon().await;
        let accepted = accept_next(engine);
        let mut handle = send_node_add(&messenger).await;
        let (context, _engine) = accepted.await.expect("the daemon does not panic");
        context
            .fail(Failure::new(
                PeppyFailureKind::PreconditionFailed,
                "Git hash mismatch: run `peppy node sync`",
            ))
            .await
            .expect("the daemon fails the goal");

        let error = follow(&messenger, &mut handle, &mut presses(0))
            .await
            .expect_err("the goal failed");

        assert_eq!(error.exit_code(), 1);
        assert_eq!(
            error.to_string(),
            format!(
                "Node add failed (precondition_failed): Git hash mismatch: run `peppy node \
                 sync`. Log file: {LOG_PATH}"
            )
        );
    }

    /// A goal that the daemon refuses ends the command with the kind and the
    /// message of the refusal.
    #[tokio::test]
    async fn a_refused_goal_gives_its_kind_to_the_command() {
        let (messenger, mut engine) = daemon().await;
        let refused = tokio::spawn(async move {
            next_goal(&mut engine)
                .await
                .refuse(Failure::new(
                    PeppyFailureKind::PreconditionFailed,
                    "action already in progress, use `--force` to force adding the node",
                ))
                .await
                .expect("the daemon refuses the goal");
            engine
        });
        let mut handle = send_node_add(&messenger).await;
        let _engine = refused.await.expect("the daemon does not panic");

        let error =
            run_action_with_feedback::<NodeAddGoalResponse, NodeAddFeedback, NodeAddResult>(
                &messenger,
                &mut handle,
                &TimeoutConfig {
                    idle_secs: 3600,
                    max_secs: 3600,
                    flags: true,
                },
                "Node add",
            )
            .await
            .expect_err("the daemon refused the goal");

        assert_eq!(
            error.to_string(),
            "Node add refused (precondition_failed): action already in progress, use `--force` \
             to force adding the node"
        );
    }
}

/// The listener of SIGINT of the process, driven by real signals that the
/// test sends to its own process. The test sends a press only while a
/// follow holds a [`CtrlC`]: with no follow, a press ends the process.
#[cfg(test)]
mod sigint {
    use super::*;
    use rustix::process::{Signal, getpid, kill_process};

    fn press_ctrl_c() {
        kill_process(getpid(), Signal::INT).expect("the test sends itself SIGINT");
    }

    /// Waits for the next press `ctrl_c` reads; the bound only fails a
    /// press that never arrives.
    async fn next_press(ctrl_c: &mut CtrlC) {
        tokio::time::timeout(Duration::from_secs(30), ctrl_c.next())
            .await
            .expect("the press reaches the follow");
    }

    /// A runtime of one command step, as [`crate::commands::block_on`]
    /// makes it.
    fn command_step_runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Runtime::new().expect("the runtime of a command step starts")
    }

    /// Each press goes to the follow that holds a [`CtrlC`], which keeps a
    /// press until it reads it: the second press here comes while the
    /// follow waits for nothing, and it is not lost. Between two follows a
    /// press ends the process. The follow of the next goal reads the presses
    /// again, also on the runtime of another command step: the listener
    /// outlives the runtime of the step that started it.
    #[test]
    fn presses_go_to_the_follow_of_the_moment_and_end_the_process_between_follows() {
        command_step_runtime().block_on(async {
            let mut first = CtrlC::listen();
            press_ctrl_c();
            next_press(&mut first).await;
            press_ctrl_c();
            next_press(&mut first).await;
        });

        assert_eq!(
            route_press(),
            PressRoute::EndsTheProcess,
            "between two follows a press ends the process"
        );

        command_step_runtime().block_on(async {
            let mut second = CtrlC::listen();
            press_ctrl_c();
            next_press(&mut second).await;
        });
        assert_eq!(route_press(), PressRoute::EndsTheProcess);
    }
}
