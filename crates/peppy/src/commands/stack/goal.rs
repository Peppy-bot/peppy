//! Driving one stack goal to its end: sending it and following it through
//! the shared goal path, rendering its feedback, holding it to the
//! operator's budgets, cancelling it at a Ctrl-C, and reporting how it ended
//! and the log files it wrote.
//!
//! The watchdog sends no cancel: a budget that runs out ends the command,
//! and its process exits. The daemon then finds that the sender of the goal
//! is gone and cancels the goal, with the reason "its sender is gone".

use std::path::Path;
use std::time::Duration;

use core_node::{idle_timeout_flag, slow_connection_hint};
use core_node_api::encoding::{
    LaunchFeedbackStep, LaunchOutput, NodeAddLogEntry, NodeBuildLogEntry, NodeLogs,
    NodeRunLogEntry, StackBudgets,
};
use stack_goal::{
    DAEMON_RESPONSE_GRACE, DaemonRoute, FollowError, RunningStackGoal, SendError, StackGoal,
    StackGoalEnd, StackGoalEvent, StackGoalOutcome,
};
use tokio::time::Instant;
use tracing::{info, warn};

use crate::commands::action_poll::{
    CancelRace, CtrlC, FollowedGoal, Interrupts, cancel_unless_stopped, end_of_follow,
};
use crate::commands::{SCROLLING_OUTPUT_LINES, caller_instance_id, failure_text};
use crate::context::DaemonConnection;
use crate::error::{Error, Result};
use crate::terminal::ScrollingOutput;

// Minimum CLI fallback ceiling when the user opts into `--max-timeout-secs`. Ensures the CLI's
// safety net never fires before the daemon's own per-phase timeout, so users see the daemon's
// precise error first. When the user omits the flag, no CLI ceiling is installed; the
// contract is idle-only (daemon-side `max_timeout_secs = None`).
const CLI_MAX_TIMEOUT_FLOOR: Duration = Duration::from_secs(7200);

// CLI wall-clock fallback ceiling. `None` means idle-only (daemon-side contract honored).
// When the user opts into `--max-timeout-secs`, add DAEMON_RESPONSE_GRACE and enforce
// CLI_MAX_TIMEOUT_FLOOR so the daemon's per-phase error fires first.
fn compute_cli_max_timeout(max_timeout_secs: Option<u64>) -> Option<Duration> {
    max_timeout_secs.map(|n| {
        Duration::from_secs(n)
            .saturating_add(DAEMON_RESPONSE_GRACE)
            .max(CLI_MAX_TIMEOUT_FLOOR)
    })
}

/// One line of the "Node log files" listing:
/// `node_name:tag@core-node: /path/to/log`, with a ` [FAILED]` marker before
/// the colon when the phase failed. The core node names the machine whose
/// filesystem holds the log file.
fn node_log_line(node_label: &str, core_node: &str, failed: bool, log_path: &Path) -> String {
    let marker = if failed { " [FAILED]" } else { "" };
    format!("{node_label}@{core_node}{marker}: {}", log_path.display())
}

fn display_node_log_files(logs: &NodeLogs) {
    if logs.is_empty() {
        return;
    }
    let NodeLogs {
        node_add_logs: add_logs,
        node_build_logs: build_logs,
        node_run_logs: run_logs,
    } = logs;
    let line = |node_label: &str, core_node: &str, failed: bool, log_path: &Path| {
        (
            node_log_line(node_label, core_node, failed, log_path),
            failed,
        )
    };
    let sections = [
        (
            "Add",
            add_logs
                .iter()
                .map(|e: &NodeAddLogEntry| line(&e.node_label, &e.core_node, e.failed, &e.log_path))
                .collect::<Vec<_>>(),
        ),
        (
            "Build",
            build_logs
                .iter()
                .map(|e: &NodeBuildLogEntry| {
                    line(&e.node_label, &e.core_node, e.failed, &e.log_path)
                })
                .collect(),
        ),
        (
            "Run",
            run_logs
                .iter()
                .map(|e: &NodeRunLogEntry| line(&e.node_label, &e.core_node, e.failed, &e.log_path))
                .collect(),
        ),
    ];
    info!("Node log files:");
    for (title, lines) in sections {
        if lines.is_empty() {
            continue;
        }
        info!("  {title}:");
        for (text, failed) in lines {
            if failed {
                tracing::error!("    {text}");
            } else {
                info!("    {text}");
            }
        }
    }
}

/// What the terminal shows of a goal's feedback: the launcher's lines as
/// they come, and the lines of a node phase in a scrolling window that a
/// new step clears.
#[derive(Default)]
struct GoalOutput {
    scrolling: Option<ScrollingOutput>,
    /// The step of the last feedback, which names the phase that went quiet
    /// when the watchdog fires.
    step: Option<LaunchFeedbackStep>,
}

impl GoalOutput {
    fn show(&mut self, feedback: &LaunchOutput) {
        if self.step != Some(feedback.step) {
            self.clear();
            self.scrolling = None;
            self.step = Some(feedback.step);
        }

        match &feedback.step {
            LaunchFeedbackStep::LauncherStep => {
                if feedback.is_stdout() {
                    info!("{}", feedback.line);
                } else {
                    tracing::warn!("{}", feedback.line);
                }
            }
            LaunchFeedbackStep::AddingNode
            | LaunchFeedbackStep::BuildingNode
            | LaunchFeedbackStep::RunningNode => {
                let output = self
                    .scrolling
                    .get_or_insert_with(|| ScrollingOutput::new(SCROLLING_OUTPUT_LINES));
                output.add_line(&feedback.line);
            }
        }
    }

    /// Takes the scrolling window off the terminal.
    fn clear(&mut self) {
        if let Some(output) = self.scrolling.as_mut() {
            output.clear();
        }
    }
}

/// The CLI's route to the daemon a command addresses.
fn daemon_route<'a>(conn: &'a DaemonConnection<'_>) -> DaemonRoute<'a> {
    DaemonRoute {
        messenger: conn.messenger,
        caller_core_node: &conn.core_node_name,
        caller_instance_id: caller_instance_id(),
        daemon_core_node: &conn.target_core_node,
    }
}

/// Sends `goal` to the daemon and follows it to its end under `budgets`,
/// then says how it ended. A Ctrl-C sends the daemon a cancel and the
/// command ends with code 130 once the goal ended.
pub(super) async fn drive_stack_goal<G: StackGoal>(
    conn: &DaemonConnection<'_>,
    goal: &G,
    budgets: &StackBudgets,
) -> Result<()> {
    let operation = G::OPERATION;
    let mut running =
        stack_goal::send(daemon_route(conn), goal)
            .await
            .map_err(|error| match error {
                SendError::Refused { failure, .. } => {
                    Error::ExecutionFailed(failure_text(&format!("{operation} refused"), &failure))
                }
                other => Error::ExecutionFailed(other.to_string()),
            })?;
    info!(
        "{operation} goal accepted, log file: {}",
        running.log_path().display()
    );

    let mut output = GoalOutput::default();
    let followed =
        follow_under_watchdog(&mut running, budgets, &mut output, &mut CtrlC::listen()).await;
    output.clear();
    let Followed { end, cancel_sent } = followed?;
    display_node_log_files(&end.node_logs);
    report_end(operation, running.log_path(), end.outcome, cancel_sent)
}

/// How the command ends for a stack goal that ended with `outcome`, whose
/// log file is `log_path`: a completed goal is a success, unless the
/// operator pressed Ctrl-C, which ends the command with code 130 whichever
/// way the goal ended.
fn report_end(
    operation: &str,
    log_path: &Path,
    outcome: StackGoalOutcome,
    cancel_sent: bool,
) -> Result<()> {
    let log_file = format!("Log file: {}", log_path.display());
    let how = match outcome {
        StackGoalOutcome::Completed(result) => {
            crate::commands::log_endpoints(&result.instance_endpoints);
            if !cancel_sent {
                info!("{operation} completed successfully");
                return Ok(());
            }
            format!("{operation} completed before the cancel took effect. {log_file}")
        }
        StackGoalOutcome::Failed(failure) => format!(
            "{}. {log_file}",
            failure_text(&format!("{operation} failed"), &failure)
        ),
        StackGoalOutcome::Cancelled { reason } => format!("{operation} was {reason}. {log_file}"),
    };
    if cancel_sent {
        return Err(Error::Interrupted(how));
    }
    Err(Error::ExecutionFailed(how))
}

/// A stack goal followed to its end, and whether the operator's Ctrl-C sent
/// the daemon a cancel on the way.
struct Followed {
    end: StackGoalEnd,
    cancel_sent: bool,
}

/// Shows the goal's output until its end. The CLI's watchdog gives up when
/// no feedback arrives for the goal's silence window, which covers the
/// longest per-phase idle budget (only one phase runs at a time) plus a grace
/// window so the daemon's phase-specific timeout always fires first and
/// surfaces a precise error, or when `--max-timeout-secs` sets a ceiling and
/// the goal outlasts it. The watchdog sends no cancel.
///
/// The first press `interrupts` gives sends the daemon a cancel, and the goal
/// is followed to the end the daemon gives it, under the ceiling alone; a
/// second press stops the wait at once, also while the cancel is on its way.
/// Every error after a Ctrl-C is an [`Error::Interrupted`].
async fn follow_under_watchdog(
    running: &mut RunningStackGoal,
    budgets: &StackBudgets,
    output: &mut GoalOutput,
    interrupts: &mut impl Interrupts,
) -> Result<Followed> {
    let silence_window = stack_goal::silence_window(budgets);
    let ceiling: Option<Instant> = compute_cli_max_timeout(budgets.max_timeout_secs)
        .and_then(|max_timeout| Instant::now().checked_add(max_timeout));
    let mut cancel_sent = false;
    loop {
        // After a Ctrl-C the daemon rolls the change back, which can be
        // silent for long: the goal is followed to its end under the ceiling
        // alone.
        let silent_at = if cancel_sent {
            None
        } else {
            Instant::now().checked_add(silence_window)
        };
        let wake_at = [silent_at, ceiling].into_iter().flatten().min();
        let watched = async {
            match wake_at {
                Some(wake_at) => tokio::time::timeout_at(wake_at, running.next()).await,
                None => Ok(running.next().await),
            }
        };
        let event = tokio::select! {
            biased;
            () = interrupts.next() => {
                output.clear();
                let followed = followed_goal(running);
                if cancel_sent {
                    return Err(followed.stopped_waiting());
                }
                cancel_sent = true;
                followed.announce_cancel();
                let cancel = running.cancel();
                let cancel = async move {
                    if let Err(error) = cancel.await {
                        warn!("{error}");
                    }
                };
                if cancel_unless_stopped(cancel, interrupts).await == CancelRace::StoppedWaiting {
                    return Err(followed.stopped_waiting());
                }
                continue;
            }
            event = watched => event,
        };
        let event = match event {
            Ok(Ok(event)) => event,
            Ok(Err(error)) => {
                let error = follow_error(&followed_goal(running), error);
                return Err(end_of_follow(cancel_sent, error));
            }
            Err(_silence_or_ceiling) => {
                let error = watchdog_error(
                    running,
                    silence_window,
                    output.step,
                    ceiling.is_some_and(|ceiling| Instant::now() >= ceiling),
                );
                return Err(end_of_follow(cancel_sent, error));
            }
        };
        match event {
            StackGoalEvent::Output(line) => output.show(&line),
            StackGoalEvent::Ended(end) => return Ok(Followed { end, cancel_sent }),
        }
    }
}

/// The goal `running` follows, as the shared follow path names it.
fn followed_goal(running: &RunningStackGoal) -> FollowedGoal<'_> {
    FollowedGoal {
        doing: running.operation(),
        log_path: Some(running.log_path()),
    }
}

/// The error of the stack goal `goal` that gave no end: a command that fell
/// behind the output of the goal says that it cancelled the goal.
fn follow_error(goal: &FollowedGoal<'_>, error: FollowError) -> Error {
    match error {
        FollowError::FellBehind { bound, .. } => goal.fell_behind(bound),
        other => Error::ExecutionFailed(other.to_string()),
    }
}

/// The watchdog's error: the ceiling that ran out, or the silence, with the
/// phase that went quiet and the flag that raises its budget, so a
/// slow-connection user is pointed at the fix instead of a bare timeout.
fn watchdog_error(
    running: &RunningStackGoal,
    silence_window: Duration,
    step: Option<LaunchFeedbackStep>,
    ceiling_reached: bool,
) -> Error {
    let operation = running.operation();
    let log_path = running.log_path().display();
    if ceiling_reached {
        return Error::ExecutionFailed(format!(
            "{operation} timed out: max timeout exceeded. Log file: {log_path}"
        ));
    }
    let (phase, hint) = match step {
        Some(step) => (
            format!(" during the {} phase", step.phase_label()),
            idle_timeout_flag(step)
                .map(|flag| format!("; {}", slow_connection_hint(flag)))
                .unwrap_or_default(),
        ),
        None => (String::new(), String::new()),
    };
    Error::ExecutionFailed(format!(
        "{operation} timed out: no output received for {}s{phase}{hint}. Log file: {log_path}",
        silence_window.as_secs(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn none_preserves_idle_only_contract() {
        assert_eq!(compute_cli_max_timeout(None), None);
    }

    #[test]
    fn small_value_hits_the_floor() {
        let got = compute_cli_max_timeout(Some(60)).expect("some");
        assert_eq!(got, CLI_MAX_TIMEOUT_FLOOR);
    }

    #[test]
    fn large_value_dominates_the_floor() {
        let n = CLI_MAX_TIMEOUT_FLOOR.as_secs() * 2;
        let got = compute_cli_max_timeout(Some(n)).expect("some");
        assert_eq!(got, Duration::from_secs(n) + DAEMON_RESPONSE_GRACE);
    }

    #[test]
    fn saturating_add_does_not_panic_at_u64_max() {
        let got = compute_cli_max_timeout(Some(u64::MAX)).expect("some");
        assert_eq!(got, Duration::MAX);
    }

    #[test]
    fn node_log_line_names_the_core_node_holding_the_file() {
        let line = node_log_line(
            "deliberative_planner:v1",
            "cn-vibrant-chaplygin",
            false,
            Path::new("/tmp/.peppy/logs/add/deliberative_planner_v1.log"),
        );
        assert_eq!(
            line,
            "deliberative_planner:v1@cn-vibrant-chaplygin: \
             /tmp/.peppy/logs/add/deliberative_planner_v1.log"
        );
    }

    #[test]
    fn node_log_line_marks_a_failed_phase_before_the_colon() {
        let line = node_log_line(
            "reactive_policy:v1",
            "cn-robot-7",
            true,
            Path::new("/tmp/.peppy/logs/build/reactive_policy_v1.log"),
        );
        assert_eq!(
            line,
            "reactive_policy:v1@cn-robot-7 [FAILED]: /tmp/.peppy/logs/build/reactive_policy_v1.log"
        );
    }

    const LOG_PATH: &str = "/logs/launch/launch_20260101_000000_000.log";

    fn end_of(outcome: StackGoalOutcome, cancel_sent: bool) -> Result<()> {
        report_end("Launch", Path::new(LOG_PATH), outcome, cancel_sent)
    }

    /// A failed stack goal ends the command with the kind and the message of
    /// its failure and the launch log, and exit code 1.
    #[test]
    fn a_failed_stack_goal_gives_its_kind_and_its_log() {
        let failure = core_node_api::DaemonFailure::new(
            config::failure_kind::PeppyFailureKind::Timeout,
            "the build of `arm:v1` produced no output for 180 s",
        );
        let error = end_of(StackGoalOutcome::Failed(failure), false).expect_err("failed");
        assert_eq!(error.exit_code(), 1);
        assert_eq!(
            error.to_string(),
            format!(
                "Launch failed (timeout): the build of `arm:v1` produced no output for 180 s. \
                 Log file: {LOG_PATH}"
            )
        );
    }

    /// After a Ctrl-C, the command says how the goal ended and exits with
    /// code 130, whichever way the goal ended.
    #[test]
    fn after_a_ctrl_c_the_command_says_how_the_goal_ended_and_exits_with_130() {
        let cancelled = end_of(
            StackGoalOutcome::Cancelled {
                reason: "cancelled by its caller".to_owned(),
            },
            true,
        )
        .expect_err("cancelled");
        assert_eq!(cancelled.exit_code(), 130);
        assert_eq!(
            cancelled.to_string(),
            format!("Launch was cancelled by its caller. Log file: {LOG_PATH}")
        );

        let completed = end_of(StackGoalOutcome::Completed(Default::default()), true)
            .expect_err("the Ctrl-C ends the command");
        assert_eq!(completed.exit_code(), 130);
        assert_eq!(
            completed.to_string(),
            format!("Launch completed before the cancel took effect. Log file: {LOG_PATH}")
        );
    }

    /// A goal that the daemon cancels for another reason, such as a stack
    /// reset, fails the command with that reason.
    #[test]
    fn a_goal_cancelled_by_a_reset_fails_the_command_with_the_reason() {
        let error = end_of(
            StackGoalOutcome::Cancelled {
                reason: "cancelled by a stack reset".to_owned(),
            },
            false,
        )
        .expect_err("cancelled");
        assert_eq!(error.exit_code(), 1);
        assert_eq!(
            error.to_string(),
            format!("Launch was cancelled by a stack reset. Log file: {LOG_PATH}")
        );
        assert!(end_of(StackGoalOutcome::Completed(Default::default()), false).is_ok());
    }

    /// A command that fell behind the output of its stack goal says that it
    /// cancelled the goal, and exits with code 1; after a Ctrl-C, a goal
    /// with no end exits with code 130, as every command that a Ctrl-C
    /// cancelled does.
    #[test]
    fn a_command_that_fell_behind_says_it_cancelled_the_goal() {
        let goal = FollowedGoal {
            doing: "Launch",
            log_path: Some(Path::new(LOG_PATH)),
        };

        let fell_behind = follow_error(
            &goal,
            FollowError::FellBehind {
                operation: "Launch",
                bound: 65_536,
            },
        );
        assert_eq!(fell_behind.exit_code(), 1);
        assert_eq!(
            fell_behind.to_string(),
            format!(
                "The command fell behind the output of the launch by more than 65536 unread \
                 messages and cancelled it. Log file: {LOG_PATH}"
            )
        );

        let gone = end_of_follow(
            true,
            follow_error(
                &goal,
                FollowError::DaemonGone {
                    operation: "Launch",
                },
            ),
        );
        assert_eq!(gone.exit_code(), 130);
        assert_eq!(
            gone.to_string(),
            "Launch ended without a result: its daemon is gone"
        );
    }

    /// A change of step ends the scrolling output of the step before it; the
    /// add, build and run steps write into one, the launcher step does not.
    #[test]
    fn show_resets_the_scrolling_output_on_a_step_change() {
        let mut output = GoalOutput::default();
        let steps = [
            (LaunchFeedbackStep::LauncherStep, false),
            (LaunchFeedbackStep::AddingNode, true),
            (LaunchFeedbackStep::AddingNode, true),
            (LaunchFeedbackStep::BuildingNode, true),
            (LaunchFeedbackStep::LauncherStep, false),
        ];
        for (step, has_scrolling_output) in steps {
            output.show(&LaunchOutput::stdout("line", step));
            assert_eq!(output.scrolling.is_some(), has_scrolling_output, "{step:?}");
            assert_eq!(output.step, Some(step));
        }
    }
}

/// The follow of a stack goal at a Ctrl-C, on a daemon's goal engine over
/// the in-process mock messaging. The presses come from the test, at fixed
/// points.
#[cfg(test)]
mod ctrl_c {
    use super::*;
    use crate::commands::action_poll::test_support::{daemon_exposing, next_goal, presses};
    use core_node_api::ActionGoal;
    use core_node_api::encoding::{LaunchGoalResponse, StackJoinGoal};
    use peppylib::messaging::{CancelCause, ConcurrentAction, GoalContext};
    use peppylib::types::Payload;

    const CORE_NODE: &str = "cn-stack-ctrl-c";
    const LOG_PATH: &str = "/logs/stack/join_20260101_000000_000.log";

    /// Phase budgets of one second, so the silence window of the watchdog
    /// is 61 s.
    fn budgets() -> StackBudgets {
        StackBudgets::new(1, 1, 1, None)
    }

    /// A join sent to a daemon that accepts it with [`LOG_PATH`]: the
    /// command's running goal, the daemon's context of it, and the daemon's
    /// engine, which holds the end of a goal for `retention`.
    async fn running_join(
        retention: Duration,
    ) -> (RunningStackGoal, GoalContext, ConcurrentAction) {
        let (messenger, engine) = daemon_exposing(CORE_NODE, StackJoinGoal::ID.name()).await;
        let mut engine = engine.with_result_retention_grace(retention);
        let goal = StackJoinGoal::new(
            "so101_sim:bravo".parse().expect("a copy reference"),
            budgets(),
        );
        let route = DaemonRoute {
            messenger: &messenger,
            caller_core_node: CORE_NODE,
            caller_instance_id: "peppy-cli-test",
            daemon_core_node: CORE_NODE,
        };
        let (running, context) = tokio::join!(stack_goal::send(route, &goal), async {
            next_goal(&mut engine)
                .await
                .accept(
                    LaunchGoalResponse::new(LOG_PATH)
                        .encode()
                        .expect("the goal response encodes"),
                )
                .await
                .expect("the daemon accepts the join")
        });
        (
            running.expect("the daemon admits the join"),
            context,
            engine,
        )
    }

    /// Follows the goal of `running` as the command does, under
    /// [`budgets`], with `count` presses given before the follow.
    async fn follow_with_presses(running: &mut RunningStackGoal, count: usize) -> Result<Followed> {
        follow_under_watchdog(
            running,
            &budgets(),
            &mut GoalOutput::default(),
            &mut presses(count),
        )
        .await
    }

    /// The daemon's end of a goal that its caller cancelled.
    async fn end_cancelled(context: &GoalContext) {
        context.cancel_signal().await;
        assert_eq!(context.cancel_cause(), Some(CancelCause::Caller));
        context
            .complete_cancelled(CancelCause::Caller.reason(), Payload::new())
            .await
            .expect("the daemon ends the goal cancelled");
    }

    /// The end of the command for a follow that gave `followed`.
    fn command_end(followed: Result<Followed>) -> Error {
        let Followed { end, cancel_sent } = match followed {
            Ok(followed) => followed,
            Err(error) => return error,
        };
        report_end("Join", Path::new(LOG_PATH), end.outcome, cancel_sent)
            .expect_err("the Ctrl-C ends the command")
    }

    /// A second Ctrl-C stops the wait at once, also while the cancel that
    /// the first one sent is on its way to the daemon: here the second press
    /// is there before the daemon answers the cancel. The daemon goes on
    /// with the cancel.
    #[tokio::test]
    async fn a_second_ctrl_c_stops_the_wait_while_the_cancel_is_on_its_way() {
        let (mut running, context, _engine) = running_join(Duration::from_secs(30)).await;

        let followed = follow_with_presses(&mut running, 2).await;

        let error = command_end(followed);
        assert_eq!(error.exit_code(), 130);
        assert_eq!(
            error.to_string(),
            format!(
                "Stopped waiting: the daemon goes on with the cancel of the join. Log file: \
                 {LOG_PATH}"
            )
        );
        tokio::time::timeout(Duration::from_secs(30), context.cancel_signal())
            .await
            .expect("the cancel that the first press sent reaches the daemon");
    }

    /// After a Ctrl-C the daemon rolls the change back, which can be silent
    /// for longer than the silence window of the watchdog: the command
    /// follows the goal to its end, says how it ended, and exits with code
    /// 130. The daemon is silent for two silence windows on the paused
    /// clock.
    #[tokio::test(start_paused = true)]
    async fn after_a_ctrl_c_a_silent_rollback_is_followed_to_its_end() {
        let (mut running, context, _engine) = running_join(Duration::from_secs(30)).await;
        let silence = stack_goal::silence_window(&budgets());

        let (followed, ()) = tokio::join!(follow_with_presses(&mut running, 1), async {
            context.cancel_signal().await;
            tokio::time::sleep(silence * 2).await;
            end_cancelled(&context).await;
        });

        let error = command_end(followed);
        assert_eq!(error.exit_code(), 130);
        assert_eq!(
            error.to_string(),
            format!("Join was cancelled by its caller. Log file: {LOG_PATH}")
        );
    }

    /// A goal that gives no end after a Ctrl-C ends the command with code
    /// 130 too: here the daemon holds the end of the cancelled goal for no
    /// time, so the end expired before the command asks for it.
    #[tokio::test]
    async fn after_a_ctrl_c_a_goal_with_no_end_exits_with_130() {
        let (mut running, context, _engine) = running_join(Duration::ZERO).await;

        let (followed, ()) = tokio::join!(
            follow_with_presses(&mut running, 1),
            end_cancelled(&context)
        );

        let error = command_end(followed);
        assert_eq!(error.exit_code(), 130);
        assert_eq!(
            error.to_string(),
            "Join result expired before it could be fetched"
        );
    }
}
