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
//! decisions never await.

use super::super::action_loop::{ActionResult, ActionState};
use parking_lot::Mutex;
use std::sync::Arc;
use std::time::Instant;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

/// Outcome of [`ConcurrencyGate::try_admit`].
pub(crate) enum Admission {
    /// The new goal was admitted. State has been transitioned to `Running` and
    /// the gate clock started.
    Admitted,
    /// A goal is already in flight and force was not requested. State is
    /// left unchanged.
    AlreadyRunning { remaining_secs: u64 },
}

/// Handle to the in-flight task and its cancellation token. Only populated by
/// handlers that support `--force` (add, build); `node_start` leaves it `None`.
#[derive(Default)]
struct GateState {
    running_task: Option<JoinHandle<()>>,
    cancel_token: Option<CancellationToken>,
}

/// Single-task admission gate. Cheap to clone (`Arc` inside).
///
/// Timing data (started_at, timeout_secs) lives on [`ActionState::Running`]
/// so the gate itself only manages the abort-task handle.
#[derive(Clone, Default)]
pub(crate) struct ConcurrencyGate {
    state: Arc<Mutex<GateState>>,
}

impl ConcurrencyGate {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Cancels and aborts the in-flight task if one is recorded. Called by
    /// add/build's `--force` path before admitting a replacement goal.
    ///
    /// The cancellation token is signalled first so that `tokio::select!`
    /// branches in the spawned task can run cleanup (e.g. rolling back an
    /// entity stuck in `Building` state) before the hard abort fires.
    fn force_abort(&self) {
        let mut state = self.state.lock();
        if let Some(token) = state.cancel_token.take() {
            token.cancel();
        }
        if let Some(handle) = state.running_task.take() {
            handle.abort();
        }
    }

    /// Stores the spawned task handle and its cancellation token so a future
    /// `--force` call can cancel and abort it.
    pub(crate) fn set_task(&self, task: JoinHandle<()>, cancel_token: CancellationToken) {
        let mut state = self.state.lock();
        state.running_task = Some(task);
        state.cancel_token = Some(cancel_token);
    }

    /// Admits a new goal by transitioning the caller-held `ActionState` to
    /// `Running` and recording the admission clock. When a goal is already in
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
        if matches!(*state_guard, ActionState::Running { .. }) {
            if !force_abort_if_running {
                return Admission::AlreadyRunning {
                    remaining_secs: state_guard.remaining_secs(),
                };
            }
            self.force_abort();
        }
        *state_guard = ActionState::Running {
            started_at: Instant::now(),
            timeout_secs,
        };
        self.state.lock().running_task = None;
        Admission::Admitted
    }
}
