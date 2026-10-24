//! One stack change of the bridge, an addition or a removal, from the read
//! of the stack to the end of the daemon's work, and the call that waits
//! for it.
//!
//! A task of the bridge owns the change ([`start_change`]): it holds the
//! bridge's lock when the change takes it, reads the stack and refuses or
//! sends the goal, then follows the goal until the daemon's work ends. The
//! call's handler only waits on that task ([`wait_for_change`]), so the
//! change goes on, and the lock stays held, when the handler stops waiting
//! or the runtime drops it: a closed call, or a whole-goal deadline of the
//! document. The task releases the lock before it reports the end of the
//! change, so a call that hears of the end finds the lock free.
//!
//! The call ends:
//!
//! - refused when the bridge or the daemon refuses the goal before it
//!   runs, with the failure kind of the refusal: `precondition_failed`,
//!   "the daemon refused the addition of bravo: a stack change is in
//!   progress on this daemon";
//! - with an empty result when the daemon completed the change: the copy is
//!   on the stack, or off it;
//! - failed when the daemon failed the change, with its failure kind and
//!   the log files: `attempt_failed`, "the addition of bravo failed:
//!   {error}. Log files: {logs}";
//! - cancelled when the client cancels: the bridge sends the daemon a
//!   cancel, the daemon stops the change at the step it reached and undoes
//!   it as a failure there does, and the call ends with the daemon's reason
//!   once the daemon ended the change. A change the daemon ended first keeps
//!   its end;
//! - failed when the change ends without an end, for example when the
//!   daemon is gone;
//! - failed after one progress window of silence: "the daemon sent nothing
//!   for 660 s; the addition of bravo can still be running". The bridge sends
//!   no cancel and goes on following the change.

use crate::bridges::{Silence, TaskSurface};
use crate::daemon_bridge::OwnDaemon;
use config::runtime::{CopyName, Name};
use core_node_api::encoding::{LaunchFeedbackStep, LaunchOutput};
use peppy_mcp_runtime::{ActionExit, CancelledGoal, ProviderFailure};
use peppylib::messaging::Failure;
use serde_json::{Value, json};
use stack_goal::{
    FollowError, RunningStackGoal, SendError, StackGoal, StackGoalEnd, StackGoalEvent,
    StackGoalOutcome,
};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::sync::{OwnedMutexGuard, mpsc, oneshot};
use tokio_util::sync::CancellationToken;

/// What a change of the bridge does, as every message about it names it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Change {
    /// A copy `name` of `option` joins the stack.
    Addition { name: CopyName, option: Name },
    /// Copy `name` leaves the stack.
    Removal { name: CopyName },
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

    /// The progress line of a cancel the client asked for.
    fn cancelling(&self) -> String {
        let launch = LaunchFeedbackStep::LauncherStep.phase_label();
        format!(
            "{launch}: the client cancelled {}; the daemon stops it and undoes it",
            self.subject()
        )
    }

    /// The message of a change that failed for `error`, with the log files
    /// that say more. An `error` that ends its own sentence keeps one period.
    fn failed(&self, error: &str, logs: &[&Path]) -> String {
        let logs: Vec<String> = logs.iter().map(|log| log.display().to_string()).collect();
        format!(
            "{} failed: {}. Log files: {}",
            self.subject(),
            error.trim_end_matches('.'),
            logs.join(", ")
        )
    }

    /// The failure of a change that the daemon failed with `failure`: its
    /// failure kind, and a message with the log files that say more.
    fn failed_with(&self, failure: &Failure, logs: &[&Path]) -> ProviderFailure {
        ProviderFailure::new(failure.kind().clone(), self.failed(failure.message(), logs))
    }

    /// The end of a call whose change the daemon refused before it ran: the
    /// failure kind and the reason of the refusal.
    fn refused(&self, refusal: &Failure) -> ActionExit {
        ActionExit::Refused(ProviderFailure::new(
            refusal.kind().clone(),
            format!(
                "the daemon refused {}: {}",
                self.subject(),
                refusal.message()
            ),
        ))
    }

    /// The end of the call of a change that ended with `end`, whose log file
    /// is `log_path`: an empty result when the daemon completed it, else the
    /// failure, with its kind and the log files that say more, or the
    /// cancel, with its reason.
    fn outcome(&self, end: &StackGoalEnd, log_path: &Path) -> Result<Value, ActionExit> {
        match &end.outcome {
            StackGoalOutcome::Completed(_) => Ok(json!({})),
            StackGoalOutcome::Failed(failure) => {
                let mut logs = vec![log_path];
                logs.extend(end.node_logs.failed());
                Err(ActionExit::Failed(self.failed_with(failure, &logs)))
            }
            StackGoalOutcome::Cancelled { reason } => Err(ActionExit::Cancelled(CancelledGoal {
                result: json!({}),
                reason: format!("{} was {reason}", self.subject()),
            })),
        }
    }

    /// The end of a call the daemon sent nothing to for `window`.
    fn silent(&self, window: Duration) -> ActionExit {
        ActionExit::Error(format!(
            "the daemon sent nothing for {} s; {} can still be running",
            seconds(window),
            self.subject()
        ))
    }
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
    /// the end of the call.
    Refused(ActionExit),
    /// The daemon admitted the goal, whose log file is `log_path`.
    Accepted { log_path: PathBuf },
}

/// One event of a change the daemon admitted.
#[derive(Debug)]
enum ChangeEvent {
    /// One line of the change's progress.
    Progress(LaunchOutput),
    /// The end the daemon gave the change.
    Ended(StackGoalEnd),
    /// The change ended without an end.
    NoEnd(FollowError),
}

impl From<Result<StackGoalEvent, FollowError>> for ChangeEvent {
    fn from(event: Result<StackGoalEvent, FollowError>) -> Self {
        match event {
            Ok(StackGoalEvent::Output(output)) => Self::Progress(output),
            Ok(StackGoalEvent::Ended(end)) => Self::Ended(end),
            Err(error) => Self::NoEnd(error),
        }
    }
}

/// The call's end of a change that a task of the bridge owns.
pub(super) struct StartedChange {
    start: oneshot::Receiver<Start>,
    events: mpsc::UnboundedReceiver<ChangeEvent>,
    /// Cancelled by the call when its client cancels: the task sends the
    /// daemon a cancel.
    cancel: CancellationToken,
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
/// returned [`StartedChange`] reads, sends the daemon a cancel when the call
/// asks for one, and goes on to the end when the call stops reading.
pub(super) fn start_change<G>(
    daemon: OwnDaemon,
    locks: Option<JoinLocks>,
    change: Change,
    goal: impl Future<Output = Result<G, ActionExit>> + Send + 'static,
) -> StartedChange
where
    G: StackGoal + Send + Sync + 'static,
{
    let (start_sender, start) = oneshot::channel();
    let (event_sender, events) = mpsc::unbounded_channel();
    let cancel = CancellationToken::new();
    let cancel_requested = cancel.clone();
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
        follow_change(running, lock, &change, event_sender, cancel_requested).await;
    });
    StartedChange {
        start,
        events,
        cancel,
    }
}

/// Reads the stack and sends the goal of `change`, or gives the end of the
/// call that refuses it: the bridge's refusal, the daemon's with its failure
/// kind, or why the goal did not reach the daemon.
async fn admit_goal<G: StackGoal>(
    daemon: &OwnDaemon,
    change: &Change,
    goal: impl Future<Output = Result<G, ActionExit>>,
) -> Result<RunningStackGoal, ActionExit> {
    let goal = goal.await?;
    stack_goal::send(daemon.route(), &goal)
        .await
        .map_err(|error| match error {
            SendError::Refused { failure, .. } => change.refused(&failure),
            other => ActionExit::Error(format!(
                "the daemon did not take {}: {other}",
                change.subject()
            )),
        })
}

/// Follows an admitted change to its end, and hands each event to the call
/// while it reads them. A cancel the call asks for (`cancel`) reaches the
/// daemon, and the change is followed to the end the daemon gives it. The
/// lock goes before the last event, so a call that hears of the end finds
/// the lock free. Once the call stops reading, the change is followed to its
/// end all the same, and its end is logged.
async fn follow_change(
    mut running: RunningStackGoal,
    lock: Option<OwnedMutexGuard<()>>,
    change: &Change,
    events: mpsc::UnboundedSender<ChangeEvent>,
    cancel: CancellationToken,
) {
    let mut cancel_sent = false;
    loop {
        let event = tokio::select! {
            biased;
            () = cancel.cancelled(), if !cancel_sent => {
                cancel_sent = true;
                if let Err(error) = running.cancel().await {
                    tracing::warn!(%error, "{} was not cancelled", change.subject());
                }
                continue;
            }
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
        ChangeEvent::Ended(end) => match &end.outcome {
            StackGoalOutcome::Completed(_) => {
                tracing::info!("{subject}, which no call waits for, succeeded");
            }
            StackGoalOutcome::Failed(failure) => tracing::warn!(
                %failure,
                "{subject}, which no call waits for, failed"
            ),
            StackGoalOutcome::Cancelled { reason } => {
                tracing::warn!(reason, "{subject}, which no call waits for, was cancelled");
            }
        },
        ChangeEvent::NoEnd(error) => {
            tracing::warn!(%error, "{subject}, which no call waits for, ended without a result");
        }
        ChangeEvent::Progress(_) => {}
    }
}

/// Waits, on behalf of a call, for a change that a task of the bridge owns
/// (see the module's documentation for how the call ends). Each line of the
/// change's progress reaches the client through `surface` as "{step}:
/// {line}", the first one as soon as the daemon admits the change. A cancel
/// of the client reaches the daemon, and the call waits for the end the
/// daemon then gives the change. `window` is the most silence the call waits
/// through, `None` under a whole-goal deadline that the runtime holds the
/// call to.
pub(super) async fn wait_for_change(
    started: StartedChange,
    change: &Change,
    surface: &impl TaskSurface,
    window: Option<Duration>,
) -> Result<Value, ActionExit> {
    let StartedChange {
        start,
        mut events,
        cancel,
    } = started;
    let log_path = match start.await {
        Ok(Start::Accepted { log_path }) => log_path,
        Ok(Start::Refused(refusal)) => return Err(refusal),
        Err(_) => {
            return Err(ActionExit::Error(format!(
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
            () = surface.cancel_requested(), if !cancel.is_cancelled() => {
                cancel.cancel();
                surface.report_feedback(change.cancelling());
                continue;
            }
            event = events.recv() => event,
            window = lasted_a_window(&mut silence) => return Err(change.silent(window)),
        };
        match event {
            Some(ChangeEvent::Progress(output)) => {
                surface.report_feedback(format!("{}: {}", output.step.phase_label(), output.line));
                if let Some(silence) = &mut silence {
                    silence.restart();
                }
            }
            Some(ChangeEvent::Ended(end)) => return change.outcome(&end, &log_path),
            Some(ChangeEvent::NoEnd(error)) => {
                return Err(ActionExit::Error(
                    change.failed(&error.to_string(), &[log_path.as_path()]),
                ));
            }
            None => {
                return Err(ActionExit::Error(format!(
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
    use config::failure_kind::PeppyFailureKind;
    use core_node_api::encoding::{
        LaunchResult, NodeAddLogEntry, NodeBuildLogEntry, NodeLogs, NodeRunLogEntry,
    };

    fn name(name: &str) -> Name {
        Name::new(name).expect("a name")
    }

    fn copy_name(name: &str) -> CopyName {
        CopyName::new(name).expect("a copy name")
    }

    fn addition() -> Change {
        Change::Addition {
            name: copy_name("bravo"),
            option: name("so101_sim"),
        }
    }

    fn removal() -> Change {
        Change::Removal {
            name: copy_name("bravo"),
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
        assert_eq!(
            addition().cancelling(),
            "launch: the client cancelled the addition of bravo; the daemon stops it and undoes it"
        );
        let window = Duration::from_secs(660);
        assert_eq!(
            addition().silent(window),
            ActionExit::Error(
                "the daemon sent nothing for 660 s; the addition of bravo can still be running"
                    .to_owned()
            )
        );
        assert_eq!(
            removal().silent(window),
            ActionExit::Error(
                "the daemon sent nothing for 660 s; the removal of bravo can still be running"
                    .to_owned()
            )
        );
        let busy = Failure::new(
            PeppyFailureKind::PreconditionFailed,
            "a stack change is in progress on this daemon",
        );
        assert_eq!(
            removal().refused(&busy),
            ActionExit::Refused(ProviderFailure::new(
                PeppyFailureKind::PreconditionFailed,
                "the daemon refused the removal of bravo: a stack change is in progress on this \
                 daemon"
            ))
        );
    }

    fn ended(outcome: StackGoalOutcome, node_logs: NodeLogs) -> StackGoalEnd {
        StackGoalEnd { outcome, node_logs }
    }

    fn failed(kind: PeppyFailureKind, message: &str) -> StackGoalOutcome {
        StackGoalOutcome::Failed(Failure::new(kind, message))
    }

    /// A failure names its kind, the goal's log, then the log of the node
    /// phase that failed, whichever phase it is; a log that did not fail is
    /// not named.
    #[test]
    fn a_failure_names_its_kind_the_stack_log_then_the_log_of_the_phase_that_failed() {
        let join_log = Path::new("/logs/stack/join.log");
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
        let at_start = ended(
            failed(PeppyFailureKind::AttemptFailed, "it exited during setup"),
            NodeLogs {
                node_add_logs: vec![add(false)],
                node_build_logs: vec![build(false)],
                node_run_logs: vec![run("bravo_arm_inst", false), run("bravo_init_inst", true)],
            },
        );
        assert_eq!(
            addition().outcome(&at_start, join_log),
            Err(ActionExit::Failed(ProviderFailure::new(
                PeppyFailureKind::AttemptFailed,
                "the addition of bravo failed: it exited during setup. Log files: \
                 /logs/stack/join.log, /logs/run/bravo_init_inst.log"
            )))
        );

        let at_build = ended(
            failed(PeppyFailureKind::Timeout, "the build went quiet"),
            NodeLogs {
                node_add_logs: vec![add(false)],
                node_build_logs: vec![build(true)],
                node_run_logs: Vec::new(),
            },
        );
        assert_eq!(
            addition().outcome(&at_build, join_log),
            Err(ActionExit::Failed(ProviderFailure::new(
                PeppyFailureKind::Timeout,
                "the addition of bravo failed: the build went quiet. Log files: \
                 /logs/stack/join.log, /logs/build/robot_initializer.log"
            )))
        );

        let not_live = ended(
            failed(
                PeppyFailureKind::ResourceUnavailable,
                "these core nodes are not live on the federation: `cn-gone`. Live right now: `cn`.",
            ),
            NodeLogs::default(),
        );
        assert_eq!(
            addition().outcome(&not_live, join_log),
            Err(ActionExit::Failed(ProviderFailure::new(
                PeppyFailureKind::ResourceUnavailable,
                "the addition of bravo failed: these core nodes are not live on the federation: \
                 `cn-gone`. Live right now: `cn`. Log files: /logs/stack/join.log"
            )))
        );
    }

    /// A completed change answers an empty result; a cancelled one ends the
    /// call cancelled, with the daemon's reason.
    #[test]
    fn a_completed_change_answers_an_empty_result_and_a_cancelled_one_its_reason() {
        let log = Path::new("/logs/stack/remove.log");
        assert_eq!(
            removal().outcome(
                &ended(
                    StackGoalOutcome::Completed(LaunchResult::default()),
                    NodeLogs::default()
                ),
                log
            ),
            Ok(json!({}))
        );
        assert_eq!(
            addition().outcome(
                &ended(
                    StackGoalOutcome::Cancelled {
                        reason: "cancelled by its caller".to_owned(),
                    },
                    NodeLogs::default()
                ),
                log
            ),
            Err(ActionExit::Cancelled(CancelledGoal {
                result: json!({}),
                reason: "the addition of bravo was cancelled by its caller".to_owned(),
            }))
        );
    }
}
