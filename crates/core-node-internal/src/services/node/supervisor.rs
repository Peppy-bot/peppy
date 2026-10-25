//! The restarts of an instance whose policy starts it again: the wait
//! between an exit and the next start, the admission onto the stack and the
//! respawn.

use super::run::{NodeRunActionContext, Respawn, assemble_runtime_config, run_node_respawn};
use super::stack_log::{StackLog, StackLogEvent};
use crate::services::action_loop::WorkStop;
use crate::services::federation::RestartAdmission;
use config::failure_kind::PeppyFailureKind;
use config::runtime::Name;
use core_node_api::ExitCause;
use core_node_api::encoding::NodeRunResult;
use node_stack::{ActionLog, Attempt, EntityHandle, NodeEntity, RespawnInputs};
use peppylib::messaging::Failure;
use std::num::NonZeroU32;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

/// Why `process_node_run` ended, for a new instance's run and for the
/// supervisor's respawn.
pub(crate) enum RunOutcome {
    /// The process reached `running`.
    Running {
        pid: u32,
        endpoints: Vec<core_node_api::InstanceEndpoint>,
    },
    /// Refused before the spawn: no process was started and none will be.
    Refused(Failure),
    /// The start failed: the spawn, a probe, an exit or a commit error, a
    /// panic of the run before its commit, an Apptainer that did not
    /// initialize for a container start, a respawning reader's clock
    /// publisher awaiting its own restart, or a new instance's pairing
    /// delivery after its commit, which leaves the process running.
    StartFailed(String),
    /// A cancel of the start gave it up before its commit: nothing to
    /// record; the text is the run's failure for a caller that waits on it.
    Cancelled(String),
    /// The slot vanished, or a respawn panicked past its commit, whose exit
    /// takes the next attempt: nothing to record; the text is the run's
    /// failure for a caller that waits on it.
    Gone(String),
}

impl RunOutcome {
    /// The outcome as the result of a `node_run` goal: the result of a
    /// process that reached `running`, or the stop of the start. A refused
    /// start gives the failure of its refusal, a start that failed is
    /// `attempt_failed`, a start whose slot is gone is
    /// `resource_unavailable`, and a start that a cancel gave up is
    /// [`WorkStop::Cancelled`].
    pub(crate) fn into_result(self) -> Result<NodeRunResult, WorkStop> {
        match self {
            Self::Running { pid, endpoints } => {
                Ok(NodeRunResult::started(pid).with_endpoints(endpoints))
            }
            Self::Refused(failure) => Err(WorkStop::Failed(failure)),
            Self::StartFailed(msg) => Err(WorkStop::Failed(Failure::new(
                PeppyFailureKind::AttemptFailed,
                msg,
            ))),
            Self::Cancelled(msg) => Err(WorkStop::Cancelled(Failure::new(
                PeppyFailureKind::AttemptFailed,
                msg,
            ))),
            Self::Gone(msg) => Err(WorkStop::Failed(Failure::new(
                PeppyFailureKind::ResourceUnavailable,
                msg,
            ))),
        }
    }
}

/// The instance a restart is scheduled for, and what its respawn needs.
pub(crate) struct RestartTarget {
    pub(crate) entity_handle: EntityHandle,
    pub(crate) instance_id: Name,
    pub(crate) node_name: String,
    pub(crate) node_tag: String,
    pub(crate) action: NodeRunActionContext,
    pub(crate) stack_log: StackLog,
}

/// Logs the backoff's say: the attempt and its wait, counted from now, or
/// the attempt the cap refused, which ends the restarts. A stop that claimed
/// the slot since the attempt was taken owns it, and nothing is logged.
pub(crate) fn log_next_attempt(stack_log: &StackLog, next: &Attempt) {
    if next.token().is_cancelled() {
        return;
    }
    match next {
        Attempt::Scheduled { attempt, delay, .. } => stack_log.record(&StackLogEvent::Restarting {
            attempt: attempt.get(),
            delay: *delay,
        }),
        Attempt::Capped { max_restarts, .. } => {
            stack_log.record(&StackLogEvent::MaxRestartsReached {
                max_restarts: max_restarts.get(),
            });
        }
    }
}

/// Spawns the background task of the instance that waits until a scheduled
/// attempt is due and respawns the instance; a stop that claimed the slot
/// since ends it.
pub(crate) fn spawn_next_attempt(target: RestartTarget, next: Attempt) {
    if let Attempt::Scheduled {
        attempt,
        delay,
        scheduled_at,
        token,
    } = next
        && !token.is_cancelled()
    {
        let tasks = target.action.node_stack.instance_tasks().clone();
        let instance_id = target.instance_id.clone();
        tasks.spawn(
            &instance_id,
            restart_task(target, attempt, scheduled_at, delay, token),
        );
    }
}

/// Waits, on the time of the stack, until `delay` after `scheduled_at`, is
/// admitted onto the stack, and starts the next process of the instance; a
/// failed start takes the backoff's next attempt, a refusal ends the
/// restarts.
async fn restart_task(
    target: RestartTarget,
    attempt: NonZeroU32,
    scheduled_at: Instant,
    delay: Duration,
    restart_token: CancellationToken,
) {
    let reset = target.action.slice_ownership.stack.cancellation();
    let time = target.action.node_stack.time().clone();
    tokio::select! {
        biased;
        _ = target.action.shutdown_token.cancelled() => return,
        _ = reset.cancelled() => return,
        _ = restart_token.cancelled() => return,
        _ = time.sleep_from(scheduled_at, delay) => {}
    }
    let Some(admission) = target
        .action
        .slice_ownership
        .admit_restart(&restart_token, &target.instance_id)
        .await
    else {
        return;
    };
    // A reset that ran between the wait and the admission tore the instance
    // down.
    if reset.is_cancelled() {
        return;
    }
    let Some(inputs) = NodeEntity::respawn_inputs_of(&target.entity_handle, &target.instance_id)
    else {
        return;
    };
    let outcome =
        start_next_process(&admission, &target, inputs, attempt, &restart_token, &reset).await;
    drop(admission);

    match outcome {
        // Nothing to schedule: a running instance restarts from its next
        // exit, and a cancelled start or a vanished slot has no restart.
        RunOutcome::Running { .. } | RunOutcome::Cancelled(_) | RunOutcome::Gone(_) => {}
        RunOutcome::StartFailed(reason) => {
            // The run log holds the whole failure, a stderr tail included.
            let reason = reason.lines().next().unwrap_or_default().to_owned();
            // A stop that claimed the slot mid-start ends it itself.
            let Some(next) = NodeEntity::mark_start_failed(
                &target.entity_handle,
                &target.instance_id,
                ExitCause::Message(reason.clone()),
                &target.action.restart_backoff,
                target.action.node_stack.time().now(),
            ) else {
                return;
            };
            target
                .stack_log
                .record(&StackLogEvent::StartFailed { reason });
            log_next_attempt(&target.stack_log, &next);
            spawn_next_attempt(target, next);
        }
        RunOutcome::Refused(failure) => {
            if NodeEntity::mark_restart_refused(&target.entity_handle, &target.instance_id) {
                target.stack_log.record(&StackLogEvent::RestartRefused {
                    reason: failure.message().to_owned(),
                });
            }
        }
    }
}

/// Starts the next process of the instance from what its slot kept, under
/// the restart's `admission`. A stop of the instance (`restart_token`) and a
/// reset of the stack (`reset`) cancel the start alike, as they cancel a
/// `node run` goal's.
async fn start_next_process(
    _admission: &RestartAdmission,
    target: &RestartTarget,
    inputs: RespawnInputs,
    attempt: NonZeroU32,
    restart_token: &CancellationToken,
    reset: &CancellationToken,
) -> RunOutcome {
    let RespawnInputs {
        goal,
        held_pairs,
        restart,
        first_start_goal,
        restart_count,
        previous_run_log,
    } = inputs;
    let runtime_config = match assemble_runtime_config(&goal, &target.action).await {
        Ok(runtime_config) => runtime_config,
        Err(reason) => {
            return RunOutcome::Refused(Failure::new(PeppyFailureKind::Rejected, reason));
        }
    };
    let log = match ActionLog::for_run(
        &target.action.peppy_dirs,
        target.action.node_stack.log_exporter().clone(),
        &target.node_name,
        &target.node_tag,
        target.instance_id.as_str(),
        goal.launch_id.as_deref(),
    ) {
        Ok(log) => log,
        Err(reason) => return RunOutcome::StartFailed(reason),
    };
    log.note_restart(attempt.get(), previous_run_log.as_deref());
    NodeEntity::record_respawn_log(&target.entity_handle, &target.instance_id, log.path());
    // Nothing reads a respawn's feedback, so its receiver is dropped; the
    // forwarder and the drain count lines regardless.
    let (feedback_tx, _) = mpsc::unbounded_channel();
    let (run_token, link) = child_cancelled_on_reset(restart_token, reset);
    let outcome = super::gate::finish_on_reset(
        run_node_respawn(
            goal,
            runtime_config,
            target.action.clone(),
            feedback_tx,
            log,
            run_token,
            Respawn {
                held_pairs,
                attempt,
                restart,
                first_start_goal,
                restart_count,
            },
        ),
        reset,
        super::gate::reset_drain_budget(target.action.node_stack.shutdown_grace()),
        || RunOutcome::Gone("the stack was reset".to_owned()),
    )
    .await;
    link.abort();
    outcome
}

/// A child of `token` that `reset` cancels too, and the task linking the
/// two, which the caller aborts once the child is no longer read.
fn child_cancelled_on_reset(
    token: &CancellationToken,
    reset: &CancellationToken,
) -> (CancellationToken, JoinHandle<()>) {
    let child = token.child_token();
    let link = tokio::spawn({
        let child = child.clone();
        let reset = reset.clone();
        async move {
            reset.cancelled().await;
            child.cancel();
        }
    });
    (child, link)
}

#[cfg(test)]
mod tests {
    use super::*;
    use daemon_config::consts::PeppyDirs;
    use log_export::LogExporter;

    /// The scheduled attempt and the capped one, each under `token`.
    fn attempts(token: &CancellationToken) -> [Attempt; 2] {
        [
            Attempt::Scheduled {
                attempt: NonZeroU32::MIN,
                delay: Duration::from_secs(1),
                scheduled_at: Instant::now(),
                token: token.clone(),
            },
            Attempt::Capped {
                max_restarts: NonZeroU32::MIN,
                token: token.clone(),
            },
        ]
    }

    /// The backoff's say is logged, unless a stop claimed the slot since the
    /// attempt was taken: its cancelled token leaves the stack log alone.
    #[test]
    fn an_attempt_a_stop_claimed_logs_nothing() {
        let root = tempfile::tempdir().unwrap();
        let dirs = PeppyDirs::new(root.path());
        let (exporter, _records) = LogExporter::channel(None);
        let stack_log =
            StackLog::for_instance(&dirs, exporter, "cn-a", "arm_inst", "arm", "v1", None);
        let read = || std::fs::read_to_string(dirs.stack_log_path()).unwrap_or_default();

        let claimed = CancellationToken::new();
        claimed.cancel();
        for next in attempts(&claimed) {
            log_next_attempt(&stack_log, &next);
        }
        assert_eq!(read(), "", "a claimed attempt logs nothing");

        for next in attempts(&CancellationToken::new()) {
            log_next_attempt(&stack_log, &next);
        }
        let logged = read();
        assert!(logged.contains("restarting in 1 s (attempt 1)"), "{logged}");
        assert!(logged.contains("`max_restarts` reached: 1 "), "{logged}");
    }
}
