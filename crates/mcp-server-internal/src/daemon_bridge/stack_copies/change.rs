//! One stack change of the bridge, an addition or a removal, from the read
//! of the stack to the end of the daemon's work, and the call that waits
//! for it.
//!
//! A task of the bridge owns the change ([`start_change`]): it holds the
//! bridge's lock when the change takes it, reads the stack and refuses or
//! sends the goal, then follows the goal until the daemon's work ends. The
//! call's handler only waits on that task ([`wait_for_change`]), so the
//! change goes on, and the lock stays held, when the handler stops waiting
//! or the runtime drops it: a cancel, a closed call, or a whole-goal
//! deadline of the document. The task releases the lock before it reports
//! the end of the change, so a call that hears of the end finds the lock
//! free.
//!
//! The call ends:
//!
//! - refused (`failed` in the call record) when the bridge or the daemon
//!   refuses the goal before it runs;
//! - with the result the daemon ended the change with: `success` true and
//!   "bravo (so101_sim) is on the stack", or `success` false and "the
//!   addition of bravo failed: {error}. Log files: {logs}";
//! - failed when the change ends without a result, for example when the
//!   daemon is gone;
//! - cancelled, at once, when the client cancels after the daemon accepted
//!   the change: "stopped waiting; the addition of bravo continues on the
//!   stack". The bridge sends the daemon nothing: a stack change ignores a
//!   cancel. A cancel that comes before the daemon answered the goal waits
//!   for that answer, which the read of the stack and the admission of the
//!   goal bound, so the call ends refused when the change never ran;
//! - failed after one progress window of silence: "the daemon sent nothing
//!   for 660 s; the addition of bravo can still be running". The bridge sends
//!   no cancel and goes on following the change.

use crate::bridges::{Silence, TaskSurface};
use crate::daemon_bridge::OwnDaemon;
use config::runtime::Name;
use core_node_api::encoding::{LaunchFeedback, LaunchFeedbackStep, LaunchResult};
use peppy_mcp_runtime::{ActionExit, CancelledGoal};
use serde_json::{Value, json};
use stack_goal::{FollowError, RunningStackGoal, SendError, StackGoal, StackGoalEvent};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::sync::{OwnedMutexGuard, mpsc, oneshot};

/// What a change of the bridge does, as every message about it names it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Change {
    /// A copy `name` of `option` joins the stack.
    Addition { name: Name, option: Name },
    /// Copy `name` leaves the stack.
    Removal { name: Name },
}

impl Change {
    /// "the addition of bravo", "the removal of bravo".
    fn subject(&self) -> String {
        match self {
            Self::Addition { name, .. } => format!("the addition of {name}"),
            Self::Removal { name } => format!("the removal of {name}"),
        }
    }

    /// The first progress of a change the daemon accepted.
    fn accepted(&self) -> String {
        let launch = LaunchFeedbackStep::LauncherStep.phase_label();
        match self {
            Self::Addition { option, .. } => format!(
                "{launch}: the daemon accepted {} ({option})",
                self.subject()
            ),
            Self::Removal { .. } => format!("{launch}: the daemon accepted {}", self.subject()),
        }
    }

    /// The message of a change that succeeded.
    fn succeeded(&self) -> String {
        match self {
            Self::Addition { name, option } => format!("{name} ({option}) is on the stack"),
            Self::Removal { name } => format!("{name} is off the stack"),
        }
    }

    /// The message of a change that failed for `error`, with the log files
    /// that say more.
    fn failed(&self, error: &str, logs: &[&Path]) -> String {
        let logs: Vec<String> = logs.iter().map(|log| log.display().to_string()).collect();
        format!(
            "{} failed: {error}. Log files: {}",
            self.subject(),
            logs.join(", ")
        )
    }

    /// The result the daemon ended the change with, as the call completes.
    fn outcome(&self, result: &LaunchResult) -> Value {
        if result.success {
            return json!({ "success": true, "message": self.succeeded() });
        }
        let error = result
            .error_message
            .as_deref()
            .unwrap_or("the daemon gave no reason");
        let mut logs = vec![result.log_path.as_path()];
        logs.extend(failed_node_logs(result));
        json!({ "success": false, "message": self.failed(error, &logs) })
    }

    /// The end of a call whose client cancelled after the daemon accepted
    /// the change.
    fn stopped_waiting(&self) -> ActionExit {
        ActionExit::Cancelled(CancelledGoal {
            result: json!({
                "success": false,
                "message": format!("stopped waiting; {} continues on the stack", self.subject()),
            }),
            reason: None,
        })
    }

    /// The end of a call the daemon sent nothing to for `window`.
    fn silent(&self, window: Duration) -> ActionExit {
        ActionExit::Failed(format!(
            "the daemon sent nothing for {} s; {} can still be running",
            seconds(window),
            self.subject()
        ))
    }
}

/// The log file of each node phase that failed: the add, build or run log
/// of the node that failed the change.
fn failed_node_logs(result: &LaunchResult) -> impl Iterator<Item = &Path> {
    let add = result
        .node_add_logs
        .iter()
        .filter(|entry| entry.failed)
        .map(|entry| entry.log_path.as_path());
    let build = result
        .node_build_logs
        .iter()
        .filter(|entry| entry.failed)
        .map(|entry| entry.log_path.as_path());
    let run = result
        .node_run_logs
        .iter()
        .filter(|entry| entry.failed)
        .map(|entry| entry.log_path.as_path());
    add.chain(build).chain(run)
}

/// `window` in whole seconds when it is a whole number of them, else with
/// its fraction: "660", "1.5".
fn seconds(window: Duration) -> String {
    let millis = window.as_millis();
    if millis.is_multiple_of(1000) {
        (millis / 1000).to_string()
    } else {
        window.as_secs_f64().to_string()
    }
}

/// How the start of a change ended.
#[derive(Debug)]
enum Start {
    /// The bridge refused the change, or the daemon did not admit its goal:
    /// the refusal to report.
    Refused(String),
    /// The daemon admitted the goal, whose log file is `log_path`.
    Accepted { log_path: PathBuf },
}

/// One event of a change the daemon admitted.
#[derive(Debug)]
enum ChangeEvent {
    /// One line of the change's progress.
    Progress(LaunchFeedback),
    /// The result the daemon ended the change with.
    Ended(LaunchResult),
    /// The change ended without a result.
    NoResult(FollowError),
}

impl From<Result<StackGoalEvent, FollowError>> for ChangeEvent {
    fn from(event: Result<StackGoalEvent, FollowError>) -> Self {
        match event {
            Ok(StackGoalEvent::Feedback(feedback)) => Self::Progress(feedback),
            Ok(StackGoalEvent::Ended(result)) => Self::Ended(result),
            Err(error) => Self::NoResult(error),
        }
    }
}

/// The call's end of a change that a task of the bridge owns.
pub(super) struct StartedChange {
    start: oneshot::Receiver<Start>,
    events: mpsc::UnboundedReceiver<ChangeEvent>,
}

/// The locks of the bridge that a `join` holds: `join` until the daemon's
/// work ends, and `admission` until the daemon admitted or refused the goal.
pub(super) struct JoinLocks {
    pub(super) join: OwnedMutexGuard<()>,
    pub(super) admission: OwnedMutexGuard<()>,
}

/// Starts `change` in a task of its own, which holds the `locks` of a
/// `join`, each for as long as [`JoinLocks`] says. The task awaits `goal`,
/// which reads the stack and gives the goal to send or a refusal, sends the
/// goal to `daemon`, and follows it to its end. It reports to the call the
/// returned [`StartedChange`] reads, and goes on to the end when the call
/// stops reading.
pub(super) fn start_change<G>(
    daemon: OwnDaemon,
    locks: Option<JoinLocks>,
    change: Change,
    goal: impl Future<Output = Result<G, String>> + Send + 'static,
) -> StartedChange
where
    G: StackGoal + Send + Sync + 'static,
{
    let (start_sender, start) = oneshot::channel();
    let (event_sender, events) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        let (lock, admission) = locks
            .map(|JoinLocks { join, admission }| (join, admission))
            .unzip();
        let admitted = admit_goal(&daemon, &change, goal).await;
        drop(admission);
        let running = match admitted {
            Ok(running) => running,
            Err(refusal) => {
                drop(lock);
                let _ = start_sender.send(Start::Refused(refusal));
                return;
            }
        };
        let log_path = running.log_path().to_owned();
        let _ = start_sender.send(Start::Accepted { log_path });
        follow_change(running, lock, &change, event_sender).await;
    });
    StartedChange { start, events }
}

/// Reads the stack and sends the goal of `change`, or gives the refusal:
/// the bridge's, the daemon's reason when it rejects the goal, or why the
/// goal did not reach it.
async fn admit_goal<G: StackGoal>(
    daemon: &OwnDaemon,
    change: &Change,
    goal: impl Future<Output = Result<G, String>>,
) -> Result<RunningStackGoal, String> {
    let goal = goal.await?;
    stack_goal::send(daemon.route(), &goal)
        .await
        .map_err(|error| match error {
            SendError::Rejected { reason, .. } => reason,
            other => format!("the daemon did not take {}: {other}", change.subject()),
        })
}

/// Follows an admitted change to its end, and hands each event to the call
/// while it reads them. The lock goes before the last event, so a call that
/// hears of the end finds the lock free. Once the call stops reading, the
/// change is followed to its end all the same, and its end is logged.
async fn follow_change(
    mut running: RunningStackGoal,
    lock: Option<OwnedMutexGuard<()>>,
    change: &Change,
    events: mpsc::UnboundedSender<ChangeEvent>,
) {
    loop {
        let event = tokio::select! {
            biased;
            event = running.next() => ChangeEvent::from(event),
            () = events.closed() => break,
        };
        if let ChangeEvent::Progress(_) = event {
            if events.send(event).is_err() {
                break;
            }
            continue;
        }
        drop(lock);
        if let Err(unread) = events.send(event) {
            log_unwatched_end(change, &unread.0);
        }
        return;
    }
    let end = ChangeEvent::from(running.follow_to_end().await.map(StackGoalEvent::Ended));
    drop(lock);
    log_unwatched_end(change, &end);
}

/// Logs the end of a change that no call waits for any more.
fn log_unwatched_end(change: &Change, end: &ChangeEvent) {
    let subject = change.subject();
    match end {
        ChangeEvent::Ended(result) if result.success => {
            tracing::info!("{subject}, which no call waits for, succeeded");
        }
        ChangeEvent::Ended(result) => tracing::warn!(
            error = result.error_message.as_deref().unwrap_or_default(),
            log = %result.log_path.display(),
            "{subject}, which no call waits for, failed"
        ),
        ChangeEvent::NoResult(error) => {
            tracing::warn!(%error, "{subject}, which no call waits for, ended without a result");
        }
        ChangeEvent::Progress(_) => {}
    }
}

/// Waits, on behalf of a call, for a change that a task of the bridge owns
/// (see the module's documentation for how the call ends). Each line of the
/// change's progress reaches the client through `surface` as "{step}:
/// {line}", the first one as soon as the daemon admits the change. `window`
/// is the most silence the call waits through, `None` under a whole-goal
/// deadline that the runtime holds the call to.
pub(super) async fn wait_for_change(
    started: StartedChange,
    change: &Change,
    surface: &impl TaskSurface,
    window: Option<Duration>,
) -> Result<Value, ActionExit> {
    let StartedChange { start, mut events } = started;
    let log_path = match start.await {
        Ok(Start::Accepted { log_path }) => log_path,
        Ok(Start::Refused(refusal)) => return Err(ActionExit::Failed(refusal)),
        Err(_) => {
            return Err(ActionExit::Failed(format!(
                "{} ended before the daemon answered its goal",
                change.subject()
            )));
        }
    };
    surface.report_feedback(change.accepted());
    let mut silence = window.map(Silence::new);
    loop {
        let event = tokio::select! {
            biased;
            () = surface.cancel_requested() => return Err(change.stopped_waiting()),
            event = events.recv() => event,
            window = lasted_a_window(&mut silence) => return Err(change.silent(window)),
        };
        match event {
            Some(ChangeEvent::Progress(feedback)) => {
                surface.report_feedback(format!(
                    "{}: {}",
                    feedback.step.phase_label(),
                    feedback.line
                ));
                if let Some(silence) = &mut silence {
                    silence.restart();
                }
            }
            Some(ChangeEvent::Ended(result)) => return Ok(change.outcome(&result)),
            Some(ChangeEvent::NoResult(error)) => {
                return Err(ActionExit::Failed(
                    change.failed(&error.to_string(), &[log_path.as_path()]),
                ));
            }
            None => {
                return Err(ActionExit::Failed(format!(
                    "{} ended without its last event",
                    change.subject()
                )));
            }
        }
    }
}

/// Resolves with its window once `silence` has lasted a whole window; never
/// without one.
async fn lasted_a_window(silence: &mut Option<Silence>) -> Duration {
    match silence {
        Some(silence) => {
            silence.lasted_a_window().await;
            silence.window()
        }
        None => std::future::pending().await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core_node_api::encoding::{NodeAddLogEntry, NodeBuildLogEntry, NodeRunLogEntry};

    fn name(name: &str) -> Name {
        Name::new(name).expect("a name")
    }

    fn addition() -> Change {
        Change::Addition {
            name: name("bravo"),
            option: name("so101_sim"),
        }
    }

    fn removal() -> Change {
        Change::Removal {
            name: name("bravo"),
        }
    }

    #[test]
    fn a_window_is_said_in_seconds() {
        assert_eq!(seconds(Duration::from_secs(660)), "660");
        assert_eq!(seconds(Duration::from_millis(1500)), "1.5");
        assert_eq!(seconds(Duration::from_millis(1)), "0.001");
    }

    /// Every message names the change, with "addition" or "removal".
    #[test]
    fn the_messages_of_an_addition_and_a_removal() {
        assert_eq!(
            addition().accepted(),
            "launch: the daemon accepted the addition of bravo (so101_sim)"
        );
        assert_eq!(
            removal().accepted(),
            "launch: the daemon accepted the removal of bravo"
        );
        assert_eq!(addition().succeeded(), "bravo (so101_sim) is on the stack");
        assert_eq!(removal().succeeded(), "bravo is off the stack");
        let window = Duration::from_secs(660);
        assert_eq!(
            addition().silent(window),
            ActionExit::Failed(
                "the daemon sent nothing for 660 s; the addition of bravo can still be running"
                    .to_owned()
            )
        );
        assert_eq!(
            removal().silent(window),
            ActionExit::Failed(
                "the daemon sent nothing for 660 s; the removal of bravo can still be running"
                    .to_owned()
            )
        );
        for (change, subject) in [(addition(), "addition"), (removal(), "removal")] {
            assert_eq!(
                change.stopped_waiting(),
                ActionExit::Cancelled(CancelledGoal {
                    result: json!({
                        "success": false,
                        "message": format!(
                            "stopped waiting; the {subject} of bravo continues on the stack"
                        ),
                    }),
                    reason: None,
                })
            );
        }
    }

    /// A failure names the goal's log, then the log of the node phase that
    /// failed, whichever phase it is; a log that did not fail is not named.
    #[test]
    fn a_failure_names_the_stack_log_then_the_log_of_the_phase_that_failed() {
        let add = |failed| NodeAddLogEntry {
            node_label: "robot_initializer:v1".to_owned(),
            log_path: PathBuf::from("/logs/add/robot_initializer.log"),
            failed,
            core_node: "cn".to_owned(),
        };
        let build = |failed| NodeBuildLogEntry {
            node_label: "robot_initializer:v1".to_owned(),
            log_path: PathBuf::from("/logs/build/robot_initializer.log"),
            failed,
            core_node: "cn".to_owned(),
        };
        let run = |instance: &str, failed| NodeRunLogEntry {
            instance_id: instance.to_owned(),
            node_label: "robot_initializer:v1".to_owned(),
            log_path: PathBuf::from(format!("/logs/run/{instance}.log")),
            failed,
            core_node: "cn".to_owned(),
        };
        let at_start = LaunchResult::failure("/logs/stack/join.log", "it exited during setup")
            .with_node_logs(
                vec![add(false)],
                vec![build(false)],
                vec![run("bravo_arm_inst", false), run("bravo_init_inst", true)],
            );
        assert_eq!(
            addition().outcome(&at_start),
            json!({
                "success": false,
                "message": "the addition of bravo failed: it exited during setup. Log files: \
                            /logs/stack/join.log, /logs/run/bravo_init_inst.log",
            })
        );

        let at_build = LaunchResult::failure("/logs/stack/join.log", "the build failed")
            .with_node_logs(vec![add(false)], vec![build(true)], Vec::new());
        assert_eq!(
            addition().outcome(&at_build)["message"],
            "the addition of bravo failed: the build failed. Log files: /logs/stack/join.log, \
             /logs/build/robot_initializer.log"
        );

        let refused = LaunchResult::failure(
            "/logs/stack/remove.log",
            "copy `bravo` is absent; peppy stack list shows the copies on the stack",
        );
        assert_eq!(
            removal().outcome(&refused),
            json!({
                "success": false,
                "message": "the removal of bravo failed: copy `bravo` is absent; peppy stack list \
                            shows the copies on the stack. Log files: /logs/stack/remove.log",
            })
        );

        assert_eq!(
            addition().outcome(&LaunchResult::success("/logs/stack/join.log")),
            json!({ "success": true, "message": "bravo (so101_sim) is on the stack" })
        );
    }
}
