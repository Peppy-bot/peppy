//! Goal-admission concurrency gate shared by the built-in single-goal actions
//! (`node_add`, `node_build`, `node_run`, `stack_launch`, `repo_refresh`).
//!
//! Each of those actions allows only one in-flight task at a time and rejects
//! concurrent goals with `"action already in progress (times out in Xs)"`.
//! `node_add` and `node_build` additionally support `--force`, which aborts
//! the in-flight task before admitting the new one. Actions without force
//! simply never pass `force = true` to [`ConcurrencyGate::try_admit`].
//!
//! All gate state lives behind a single `parking_lot::Mutex`, so admission
//! decisions never await.

use parking_lot::Mutex;
use peppylib::messaging::GoalContext;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

/// Outcome of [`ConcurrencyGate::try_admit`].
pub(crate) enum Admission {
    /// The new goal was admitted; the gate clock has been started.
    Admitted,
    /// A goal is already in flight and force was not requested.
    AlreadyRunning { remaining_secs: u64 },
}

/// Timing for the in-flight goal, used to report `remaining_secs` on rejection.
struct RunningInfo {
    started_at: Instant,
    timeout_secs: u64,
}

#[derive(Default)]
struct GateState {
    /// `Some` while a goal is in flight (drives admission decisions).
    running: Option<RunningInfo>,
    /// Handle to the in-flight task and its cancellation token. Only populated
    /// by handlers that support `--force`; others leave them `None`.
    running_task: Option<JoinHandle<()>>,
    cancel_token: Option<CancellationToken>,
    /// The most recently completed goal's context, retained so its result
    /// stays fetchable until the next goal is admitted. Dropping it evicts the
    /// goal's registry slot in the SDK.
    retained: Option<GoalContext>,
}

/// Single-task admission gate. Cheap to clone (`Arc` inside).
#[derive(Clone, Default)]
pub(crate) struct ConcurrencyGate {
    state: Arc<Mutex<GateState>>,
}

impl ConcurrencyGate {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Admits a new goal, starting the gate clock. When a goal is already in
    /// flight and `force` is true, the previous task is cancelled and aborted
    /// before admission proceeds. Otherwise the in-flight goal's
    /// `remaining_secs` is returned and the gate is left unchanged so the
    /// caller can encode a rejection.
    ///
    /// Admitting a new goal drops the previously retained context (evicting its
    /// registry slot): once a new goal starts, the old result is superseded.
    pub(crate) fn try_admit(&self, timeout_secs: u64, force: bool) -> Admission {
        let mut state = self.state.lock();
        if let Some(running) = &state.running {
            if !force {
                let remaining = Duration::from_secs(running.timeout_secs)
                    .saturating_sub(running.started_at.elapsed())
                    .as_secs();
                return Admission::AlreadyRunning {
                    remaining_secs: remaining,
                };
            }
            // Force: signal cancellation so the spawned task can run cleanup,
            // then hard-abort it.
            if let Some(token) = state.cancel_token.take() {
                token.cancel();
            }
            if let Some(handle) = state.running_task.take() {
                handle.abort();
            }
        }
        state.running = Some(RunningInfo {
            started_at: Instant::now(),
            timeout_secs,
        });
        state.running_task = None;
        state.cancel_token = None;
        state.retained = None;
        Admission::Admitted
    }

    /// Clears the running slot without retaining a context. Used on the rare
    /// failure path where a goal was admitted but couldn't be started (e.g. the
    /// per-goal context failed to register), so a retry isn't wrongly rejected.
    pub(crate) fn clear_running(&self) {
        let mut state = self.state.lock();
        state.running = None;
        state.running_task = None;
        state.cancel_token = None;
    }

    /// Stores the spawned task handle and its cancellation token so a future
    /// `--force` call can cancel and abort it.
    pub(crate) fn set_task(&self, task: JoinHandle<()>, cancel_token: CancellationToken) {
        let mut state = self.state.lock();
        state.running_task = Some(task);
        state.cancel_token = Some(cancel_token);
    }

    /// Marks the in-flight goal finished: clears the running slot so the next
    /// goal can be admitted, and retains the goal's context so its result stays
    /// fetchable (via the SDK result rendezvous) until the next goal arrives.
    pub(crate) fn finish(&self, ctx: GoalContext) {
        let mut state = self.state.lock();
        state.running = None;
        state.running_task = None;
        state.cancel_token = None;
        state.retained = Some(ctx);
    }
}
