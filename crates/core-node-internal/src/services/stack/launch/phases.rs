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

/// The flag that raises the idle budget of the phase a feedback step belongs
/// to. `None` for the launcher step, whose work (parse/resolve) has no
/// per-phase flag: the CLI watchdog alone bounds it.
///
/// The one place the step→flag mapping lives: both the daemon's per-phase
/// timeout message (below) and the CLI watchdog's timeout message consume it,
/// so a flag rename cannot leave the two giving different advice.
pub fn idle_timeout_flag(step: LaunchFeedbackStep) -> Option<&'static str> {
    match step {
        LaunchFeedbackStep::AddingNode => Some("--node-add-idle-timeout-secs"),
        LaunchFeedbackStep::BuildingNode => Some("--node-build-idle-timeout-secs"),
        LaunchFeedbackStep::RunningNode => Some("--node-run-idle-timeout-secs"),
        LaunchFeedbackStep::LauncherStep => None,
    }
}

/// The retry advice appended to an idle-timeout error, pointing a
/// slow-connection user at the flag that raises the budget. Shared by the
/// daemon and CLI timeout messages so the wording cannot drift.
pub fn slow_connection_hint(flag: &str) -> String {
    format!(
        "if this machine is on a slow connection, retry with a larger {flag} \
         (progress output resets this clock)"
    )
}

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
/// `change_deadline` is `Some`. Returns:
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
    change_deadline: Option<Instant>,
    cancel_and_drain: Option<CancellationToken>,
) -> PhaseOutcome<T>
where
    F: std::future::Future<Output = T>,
{
    match cancel_and_drain {
        None => {
            run_phase_drop_on_timeout(phase, activity_notify, idle_timeout, change_deadline).await
        }
        Some(token) => {
            run_phase_cancel_on_timeout(
                phase,
                activity_notify,
                idle_timeout,
                change_deadline,
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
    change_deadline: Option<Instant>,
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

    match change_deadline {
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
    change_deadline: Option<Instant>,
    cancel_token: CancellationToken,
) -> PhaseOutcome<T>
where
    F: std::future::Future<Output = T>,
{
    tokio::pin!(phase);

    // `sleep_until(past-instant)` resolves immediately, so we model "no
    // deadline" as a far-future sleep and let idle/phase race win.
    let deadline_sleep = async {
        match change_deadline {
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
    change_deadline: Option<Instant>,
    reset: &CancellationToken,
    log_file: &Arc<StdMutex<File>>,
    step: LaunchFeedbackStep,
    build_failure: impl FnOnce(String) -> T,
    cancel_and_drain: Option<CancellationToken>,
) -> T
where
    F: std::future::Future<Output = T>,
{
    let drain = cancel_and_drain.clone();
    let phase = run_phase_with_timeouts(
        phase,
        activity_notify,
        idle_timeout,
        change_deadline,
        cancel_and_drain,
    );
    tokio::pin!(phase);
    let outcome = tokio::select! {
        biased;
        _ = reset.cancelled() => {
            if let Some(token) = drain {
                token.cancel();
                let _ = tokio::time::timeout(COOPERATIVE_TEARDOWN_BUDGET, phase.as_mut()).await;
            }
            let reason = "stack operation cancelled by stack reset".to_owned();
            write_error_to_log(log_file, &reason);
            return build_failure(reason);
        }
        outcome = phase.as_mut() => outcome,
    };
    match outcome {
        PhaseOutcome::Completed(result) => result,
        PhaseOutcome::IdleTimeout => {
            let mut reason = format!(
                "timeout: {} idle timeout exceeded ({}s without output)",
                step.phase_label(),
                idle_timeout.as_secs()
            );
            if let Some(flag) = idle_timeout_flag(step) {
                reason = format!("{reason}; {}", slow_connection_hint(flag));
            }
            write_error_to_log(log_file, &reason);
            build_failure(reason)
        }
        PhaseOutcome::MaxTimeout => {
            let reason = "timeout: max timeout exceeded".to_string();
            write_error_to_log(log_file, &reason);
            build_failure(reason)
        }
    }
}

/// Regression tests for the run-phase cancel-and-drain contract.
///
/// The invariant under test: when a run-phase timeout fires,
/// `run_phase_cancel_on_timeout` must signal the cancel token *and* drive the
/// phase future to completion so its cleanup runs, not drop it. Using
/// `tokio::time::pause()` + manual advancement so these tests are
/// deterministic (no wall-clock dependency, no risk of CI flake).
#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    /// Builds a phase future that signals `cleanup_ran` if it observes the
    /// cancel token, simulating `run_node_run`'s `abort_started` branch.
    /// If instead the outer runner drops this future, the flag stays false
    /// and the test fails, matching the real-world orphan bug.
    async fn cancellable_phase(
        cancel: CancellationToken,
        cleanup_ran: Arc<AtomicBool>,
    ) -> &'static str {
        tokio::select! {
            _ = cancel.cancelled() => {
                cleanup_ran.store(true, Ordering::SeqCst);
                "cleaned_up"
            }
            _ = std::future::pending::<()>() => unreachable!("phase should never complete on its own in these tests"),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn cancel_on_timeout_awaits_cleanup_on_idle_timeout() {
        let notify = Arc::new(Notify::new());
        let token = CancellationToken::new();
        let cleanup_ran = Arc::new(AtomicBool::new(false));

        let outcome = run_phase_cancel_on_timeout(
            cancellable_phase(token.clone(), Arc::clone(&cleanup_ran)),
            Arc::clone(&notify),
            Duration::from_millis(100),
            None,
            token,
        )
        .await;

        assert!(
            matches!(outcome, PhaseOutcome::IdleTimeout),
            "idle timeout branch expected",
        );
        assert!(
            cleanup_ran.load(Ordering::SeqCst),
            "phase future must be awaited after cancel so cleanup runs; \
             dropping it would leave this flag false (the orphan-process bug)",
        );
    }

    #[tokio::test(start_paused = true)]
    async fn cancel_on_timeout_awaits_cleanup_on_max_deadline() {
        let notify = Arc::new(Notify::new());
        let token = CancellationToken::new();
        let cleanup_ran = Arc::new(AtomicBool::new(false));

        let deadline = Instant::now() + Duration::from_millis(50);
        let outcome = run_phase_cancel_on_timeout(
            cancellable_phase(token.clone(), Arc::clone(&cleanup_ran)),
            Arc::clone(&notify),
            // Idle much larger than max so only the deadline branch can fire.
            Duration::from_secs(600),
            Some(deadline),
            token,
        )
        .await;

        assert!(
            matches!(outcome, PhaseOutcome::MaxTimeout),
            "max timeout branch expected",
        );
        assert!(
            cleanup_ran.load(Ordering::SeqCst),
            "phase future must be awaited after max-deadline cancel so cleanup runs",
        );
    }

    #[tokio::test(start_paused = true)]
    async fn cancel_on_timeout_returns_value_when_phase_completes_first() {
        let notify = Arc::new(Notify::new());
        let token = CancellationToken::new();
        let cleanup_ran = Arc::new(AtomicBool::new(false));
        let cleanup_ran_for_phase = Arc::clone(&cleanup_ran);

        // Phase completes immediately with a value; no timeout should fire.
        let phase = async move {
            // Reset-like ping proves we keep the happy-path contract (no cancel signal).
            let _ = cleanup_ran_for_phase;
            "ok"
        };

        let outcome = run_phase_cancel_on_timeout(
            phase,
            Arc::clone(&notify),
            Duration::from_millis(100),
            Some(Instant::now() + Duration::from_millis(100)),
            token.clone(),
        )
        .await;

        match outcome {
            PhaseOutcome::Completed(v) => assert_eq!(v, "ok"),
            _ => panic!("phase should complete before any timeout fires"),
        }
        assert!(
            !token.is_cancelled(),
            "happy path must not cancel the token",
        );
    }
}
