//! Driving one stack goal to its end: sending it and following it through
//! the shared goal path, rendering its feedback, holding it to the
//! operator's budgets, cancelling it at a Ctrl-C, and reporting how it ended
//! and the log files it wrote.

use std::path::Path;
use std::time::Duration;

use core_node::{idle_timeout_flag, slow_connection_hint};
use core_node_api::encoding::{
    LaunchFeedbackStep, LaunchOutput, NodeAddLogEntry, NodeBuildLogEntry, NodeLogs,
    NodeRunLogEntry, StackBudgets,
};
use stack_goal::{
    DAEMON_RESPONSE_GRACE, DaemonRoute, RunningStackGoal, SendError, StackGoal, StackGoalEnd,
    StackGoalEvent, StackGoalOutcome,
};
use tokio::time::Instant;
use tracing::{info, warn};

use crate::commands::action_poll::{CtrlC, Interrupts};
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
    let followed = follow_under_watchdog(&mut running, budgets, &mut output, &mut CtrlC).await;
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
/// is followed to the end the daemon gives it; a second press stops the wait
/// at once, with [`Error::Interrupted`].
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
        let silent_at = Instant::now().checked_add(silence_window);
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
                if cancel_sent {
                    output.clear();
                    return Err(Error::Interrupted(format!(
                        "Stopped waiting: the daemon goes on with the cancel of the {}. Log file: {}",
                        running.operation().to_lowercase(),
                        running.log_path().display()
                    )));
                }
                cancel_sent = true;
                output.clear();
                warn!(
                    "Cancelling the {}; press Ctrl-C again to stop waiting for its end",
                    running.operation().to_lowercase()
                );
                if let Err(error) = running.cancel().await {
                    warn!("{error}");
                }
                continue;
            }
            event = watched => event,
        };
        let Ok(event) = event else {
            return Err(watchdog_error(
                running,
                silence_window,
                output.step,
                ceiling.is_some_and(|ceiling| Instant::now() >= ceiling),
            ));
        };
        match event.map_err(|error| Error::ExecutionFailed(error.to_string()))? {
            StackGoalEvent::Output(line) => output.show(&line),
            StackGoalEvent::Ended(end) => return Ok(Followed { end, cancel_sent }),
        }
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
