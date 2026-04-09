//! Goal-admission concurrency gate shared by `node_add`, `node_build`, and
//! `node_start`.
//!
//! Each of those actions allows only one in-flight task at a time and rejects
//! concurrent goals with `"action already in progress (times out in Xs)"`.
//! `node_add` and `node_build` additionally support `--force`, which aborts
//! the in-flight task before admitting the new one. `node_start` does not
//! support force, so its handler simply never calls [`ConcurrencyGate::force_abort`].
//!
//! All gate state lives behind a single `parking_lot::Mutex`, so admission
//! decisions never await — replacing four sequential `tokio::Mutex` lock
//! awaits in the previous per-handler implementation.

use super::super::action_loop::{ActionResult, ActionState};
use parking_lot::Mutex;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::task::JoinHandle;

/// Outcome of [`ConcurrencyGate::try_admit`].
pub(crate) enum Admission {
    /// The new goal was admitted. State has been transitioned to `Running` and
    /// the gate clock started.
    Admitted,
    /// A goal is already in flight and force was not requested. State is
    /// left unchanged.
    AlreadyRunning { remaining_secs: u64 },
}

#[derive(Default)]
struct GateState {
    /// `(started_at, timeout_secs)` of the in-flight task, if any.
    running_since: Option<(Instant, u64)>,
    /// Handle to the in-flight task. Only populated by handlers that support
    /// `--force` (add, build); `node_start` leaves it `None`.
    running_task: Option<JoinHandle<()>>,
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

    /// Computes the remaining seconds for the in-flight task. Returns 0 if no
    /// task is recorded or the timeout has already elapsed. Used by handlers
    /// to populate the rejection message *before* deciding whether to admit
    /// the new goal.
    pub(crate) fn remaining_secs(&self) -> u64 {
        let g = self.state.lock();
        g.running_since
            .map(|(started_at, timeout_secs)| {
                Duration::from_secs(timeout_secs)
                    .saturating_sub(started_at.elapsed())
                    .as_secs()
            })
            .unwrap_or(0)
    }

    /// Aborts the in-flight task if one is recorded. Called by add/build's
    /// `--force` path before admitting a replacement goal.
    pub(crate) fn force_abort(&self) {
        if let Some(handle) = self.state.lock().running_task.take() {
            handle.abort();
        }
    }

    /// Records `(now, timeout_secs)` as the new in-flight admission. Any
    /// previous `running_task` handle is dropped (caller is expected to have
    /// already aborted it via [`Self::force_abort`] if needed).
    pub(crate) fn mark_running(&self, timeout_secs: u64) {
        let mut g = self.state.lock();
        g.running_since = Some((Instant::now(), timeout_secs));
        g.running_task = None;
    }

    /// Stores the spawned task handle so a future `--force` call can abort it.
    pub(crate) fn set_task(&self, task: JoinHandle<()>) {
        self.state.lock().running_task = Some(task);
    }

    /// Admits a new goal by transitioning the caller-held `ActionState` to
    /// `Running` and recording the gate clock. When a goal is already in
    /// flight and `force_abort_if_running` is true, the previous task is
    /// aborted before admission proceeds. Otherwise the in-flight goal's
    /// `remaining_secs` is returned and the state is left unchanged so the
    /// caller can encode a rejection.
    pub(crate) fn try_admit<R: ActionResult>(
        &self,
        state_guard: &mut ActionState<R>,
        timeout_secs: u64,
        force_abort_if_running: bool,
    ) -> Admission {
        if matches!(*state_guard, ActionState::Running) {
            if !force_abort_if_running {
                return Admission::AlreadyRunning {
                    remaining_secs: self.remaining_secs(),
                };
            }
            self.force_abort();
        }
        *state_guard = ActionState::Running;
        self.mark_running(timeout_secs);
        Admission::Admitted
    }
}
