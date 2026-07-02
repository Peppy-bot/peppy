use crate::services::node::gate::COOPERATIVE_TEARDOWN_BUDGET;
use crate::services::node::write_error_to_log;
use core_node_api::encoding::LaunchFeedbackStep;
use parking_lot::Mutex as StdMutex;
use std::fs::File;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Notify;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

/// Watches for an idle period: returns when no `notify_one()` arrives for `idle_timeout`.
/// Each call to `notify_one()` on `notify` resets the clock.
async fn watch_idle(notify: Arc<Notify>, idle_timeout: Duration) {
    loop {
        match tokio::time::timeout(idle_timeout, notify.notified()).await {
            Ok(()) => continue,
            Err(_) => return,
        }
    }
}

/// Outcome of a per-phase operation wrapped with idle + (optional) launch-deadline enforcement.
pub(super) enum PhaseOutcome<T> {
    Completed(T),
    IdleTimeout,
    MaxTimeout,
}

/// Wraps a phase future with idle-timeout enforcement and an optional whole-launch deadline.
///
/// The idle watcher always runs (idle protection is always on); the deadline only wraps when
/// `launch_deadline` is `Some`. Returns:
/// - `Completed(T)` if the phase finished within both bounds
/// - `IdleTimeout` if `idle_timeout` elapsed without subprocess activity
/// - `MaxTimeout` if the launch deadline fired
///
/// Cancellation semantics differ per phase:
/// - **add** relies on git2's progress callback returning the cancellation status when the
///   future is dropped, and on the http downloader's drop-safe streaming reader.
/// - **build** relies on `stream_child_output`'s `KillGuard` (in
///   `node-stack-internal/src/build_io.rs`), which SIGKILLs the child process group on drop.
/// - **run** cannot rely on drop alone: `prepare_and_spawn` returns a raw
///   `tokio::process::Child` held on the phase future's stack with no `kill_on_drop`, so
///   dropping it leaves the OS process and its `Starting` stack entry behind. Callers that
///   need run-phase cancellation pass `cancel_and_drain = Some(token)`; on timeout the
///   runner signals the token and awaits the phase future's cooperative cleanup (bounded by
///   `COOPERATIVE_TEARDOWN_BUDGET`) instead of dropping it.
async fn run_phase_with_timeouts<F, T>(
    phase: F,
    activity_notify: Arc<Notify>,
    idle_timeout: Duration,
    launch_deadline: Option<Instant>,
    cancel_and_drain: Option<CancellationToken>,
) -> PhaseOutcome<T>
where
    F: std::future::Future<Output = T>,
{
    match cancel_and_drain {
        None => {
            run_phase_drop_on_timeout(phase, activity_notify, idle_timeout, launch_deadline).await
        }
        Some(token) => {
            run_phase_cancel_on_timeout(
                phase,
                activity_notify,
                idle_timeout,
                launch_deadline,
                token,
            )
            .await
        }
    }
}

/// Timeout behavior for phases whose futures are cancellation-safe via `Drop`
/// (currently: add, build).
async fn run_phase_drop_on_timeout<F, T>(
    phase: F,
    activity_notify: Arc<Notify>,
    idle_timeout: Duration,
    launch_deadline: Option<Instant>,
) -> PhaseOutcome<T>
where
    F: std::future::Future<Output = T>,
{
    let inner = async {
        tokio::select! {
            biased;
            _ = watch_idle(activity_notify, idle_timeout) => None,
            result = phase => Some(result),
        }
    };

    match launch_deadline {
        Some(deadline) => match tokio::time::timeout_at(deadline, inner).await {
            Ok(Some(value)) => PhaseOutcome::Completed(value),
            Ok(None) => PhaseOutcome::IdleTimeout,
            Err(_) => PhaseOutcome::MaxTimeout,
        },
        None => match inner.await {
            Some(value) => PhaseOutcome::Completed(value),
            None => PhaseOutcome::IdleTimeout,
        },
    }
}

/// Timeout behavior for phases that own resources (e.g. a spawned child
/// process) not reaped by `Drop`. On timeout, signals `cancel_token` and
/// awaits the phase future for up to `COOPERATIVE_TEARDOWN_BUDGET` so it
/// can run its own teardown (SIGKILL the child, unregister the `Starting`
/// instance, remove temp files) before we return the timeout outcome.
pub(super) async fn run_phase_cancel_on_timeout<F, T>(
    phase: F,
    activity_notify: Arc<Notify>,
    idle_timeout: Duration,
    launch_deadline: Option<Instant>,
    cancel_token: CancellationToken,
) -> PhaseOutcome<T>
where
    F: std::future::Future<Output = T>,
{
    tokio::pin!(phase);

    // `sleep_until(past-instant)` resolves immediately, so we model "no
    // deadline" as a far-future sleep and let idle/phase race win.
    let deadline_sleep = async {
        match launch_deadline {
            Some(deadline) => tokio::time::sleep_until(deadline).await,
            None => std::future::pending::<()>().await,
        }
    };
    tokio::pin!(deadline_sleep);

    let timeout_kind = tokio::select! {
        biased;
        result = &mut phase => return PhaseOutcome::Completed(result),
        _ = watch_idle(activity_notify, idle_timeout) => PhaseOutcome::IdleTimeout,
        _ = &mut deadline_sleep => PhaseOutcome::MaxTimeout,
    };

    // Timeout fired. Ask the phase to tear itself down, then drive it to
    // completion so its cleanup (kill child, remove `Starting` entry, delete
    // instance dir) actually runs. If cleanup stalls past the budget we drop
    // the future as a last resort; still strictly better than today, since
    // the run phase would have been dropped immediately in that branch.
    cancel_token.cancel();
    let _ = tokio::time::timeout(COOPERATIVE_TEARDOWN_BUDGET, phase.as_mut()).await;

    timeout_kind
}

/// Runs a phase future under idle + (optional) deadline bounds and, on timeout, writes the
/// reason to `log_file` and builds a caller-specified failure result. `build_failure`
/// receives the same string that was logged so phase-specific failure types (differing in
/// whether they carry a `log_path`) can embed it verbatim.
///
/// `cancel_and_drain` controls what happens to the phase future on timeout; see
/// [`run_phase_with_timeouts`] for the per-phase rationale.
#[allow(clippy::too_many_arguments)] // All args serve distinct, unrelated roles; grouping them adds noise.
pub(super) async fn run_phase<F, T>(
    phase: F,
    activity_notify: Arc<Notify>,
    idle_timeout: Duration,
    launch_deadline: Option<Instant>,
    log_file: &Arc<StdMutex<File>>,
    step: LaunchFeedbackStep,
    build_failure: impl FnOnce(String) -> T,
    cancel_and_drain: Option<CancellationToken>,
) -> T
where
    F: std::future::Future<Output = T>,
{
    match run_phase_with_timeouts(
        phase,
        activity_notify,
        idle_timeout,
        launch_deadline,
        cancel_and_drain,
    )
    .await
    {
        PhaseOutcome::Completed(result) => result,
        PhaseOutcome::IdleTimeout => {
            let reason = format!(
                "timeout: {} idle timeout exceeded ({}s without output)",
                step.phase_label(),
                idle_timeout.as_secs()
            );
            write_error_to_log(log_file, &reason);
            build_failure(reason)
        }
        PhaseOutcome::MaxTimeout => {
            let reason = "timeout: max launch timeout exceeded".to_string();
            write_error_to_log(log_file, &reason);
            build_failure(reason)
        }
    }
}
