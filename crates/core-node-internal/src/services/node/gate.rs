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
//! decisions never await. Unlike a poll-based design, the gate does not retain
//! the completed goal's context: the work task calls
//! [`peppylib::messaging::GoalContext::complete`] and drops the context, and the
//! SDK keeps the result fetchable for its retention grace, which comfortably
//! covers the client's immediate single fetch.
//!
//! ## Concurrency model
//!
//! Admission ([`try_admit`](ConcurrencyGate::try_admit)), the pre-spawn release
//! ([`clear_running`](ConcurrencyGate::clear_running)), and task registration
//! ([`set_task`](ConcurrencyGate::set_task)) all run on the action's single
//! accept loop (`run_action_loop`), which handles one goal to completion before
//! pulling the next, so they never race one another — the slot they touch can
//! only be the goal currently being handled. The one gate access that *is*
//! concurrent is the in-flight work task freeing its slot when it finishes.
//!
//! A `--force` goal can abort that task and admit a replacement while the
//! aborted task is still winding down — `JoinHandle::abort()` is cooperative and
//! does not interrupt a synchronous stretch between `.await`s — so a naive
//! release could clear the *replacement's* slot and break the single-goal
//! invariant. Each admission therefore carries a monotonic `generation`: the
//! work task holds a [`GoalSlotGuard`] for its generation, and the guard's drop
//! frees the slot only while that generation is still current. A superseded
//! task's release is thus a safe no-op.

use parking_lot::Mutex;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

/// Outcome of [`ConcurrencyGate::try_admit`].
pub(crate) enum Admission {
    /// The new goal was admitted; the gate clock has been started. The
    /// `generation` identifies this admission — the work task hands it to its
    /// [`GoalSlotGuard`] so the slot is freed only while this admission is still
    /// the current one (a later `--force` goal bumps the generation).
    Admitted { generation: u64 },
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
    /// Bumped on every admission. The in-flight work task captures the
    /// generation it was admitted under; its [`GoalSlotGuard`] clears the slot
    /// only while the generation still matches, so a task that was
    /// force-superseded can never clobber the slot the `--force` goal installed.
    generation: u64,
}

impl GateState {
    /// Clears the running slot and any stored task handle/token.
    fn clear(&mut self) {
        self.running = None;
        self.running_task = None;
        self.cancel_token = None;
    }
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

    /// Admits a new goal, starting the gate clock and returning the admission's
    /// generation. When a goal is already in flight and `force` is true, the
    /// previous task is cancelled and aborted before admission proceeds
    /// (aborting drops its `GoalContext`, which closes the old goal's feedback
    /// stream and evicts its registry slot). Otherwise the in-flight goal's
    /// `remaining_secs` is returned and the gate is left unchanged so the caller
    /// can encode a rejection.
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
        state.generation = state.generation.wrapping_add(1);
        Admission::Admitted {
            generation: state.generation,
        }
    }

    /// Clears the running slot. Used on the rare failure path where a goal was
    /// admitted but couldn't be started (e.g. log-file creation failed), so a
    /// retry isn't wrongly rejected.
    ///
    /// Unconditional by design: this runs on the sequential accept loop together
    /// with [`try_admit`](Self::try_admit), so no other admission can interleave
    /// between this goal's admission and its `clear_running` — the slot is
    /// necessarily this goal's own. Only the post-spawn release runs
    /// concurrently and needs the generation check; that is [`GoalSlotGuard`].
    pub(crate) fn clear_running(&self) {
        self.state.lock().clear();
    }

    /// Stores the spawned task handle and its cancellation token so a future
    /// `--force` call can cancel and abort it. Like [`clear_running`], this runs
    /// on the sequential accept loop, so the admission cannot have changed since
    /// `try_admit` and the slot is unconditionally this goal's.
    ///
    /// [`clear_running`]: Self::clear_running
    pub(crate) fn set_task(&self, task: JoinHandle<()>, cancel_token: CancellationToken) {
        let mut state = self.state.lock();
        state.running_task = Some(task);
        state.cancel_token = Some(cancel_token);
    }

    /// Builds the running-slot guard for a spawned work task, consuming a gate
    /// clone (the task needs no other gate handle once it holds the guard). The
    /// `generation` must be the one returned by the [`try_admit`](Self::try_admit)
    /// that admitted this task.
    pub(crate) fn into_slot_guard(self, generation: u64) -> GoalSlotGuard {
        GoalSlotGuard {
            gate: self,
            generation,
        }
    }

    /// Frees the running slot, but only while `generation` is still the current
    /// admission. Called solely by [`GoalSlotGuard`] on drop; a stale generation
    /// (a later `--force` goal took over) makes it a no-op so the new owner's
    /// slot is preserved.
    fn release(&self, generation: u64) {
        let mut state = self.state.lock();
        if state.generation == generation {
            state.clear();
        }
    }
}

/// RAII release for a spawned work task: dropping it frees the gate's running
/// slot so the next goal can be admitted — but only while the gate is still on
/// the guard's generation.
///
/// Held by the work task for its whole lifetime, it fires on every exit path —
/// normal completion, an early return, a panic unwinding the task, or a
/// `--force` `JoinHandle::abort` dropping the future — so the gate is never left
/// stuck "in progress" (the failure mode every future goal would then hit with
/// "action already in progress" until the daemon restarts). When a later
/// `--force` goal has already taken over, bumping the generation, the drop is a
/// safe no-op and cannot clobber the new owner's slot — the single-goal
/// invariant the generation protects.
///
/// The work task completes and drops its `GoalContext` separately; the SDK
/// retains the result for its grace window so the client's fetch still resolves.
#[must_use = "the guard must be held for the task's lifetime; dropping it immediately frees the gate"]
pub(crate) struct GoalSlotGuard {
    gate: ConcurrencyGate,
    generation: u64,
}

impl Drop for GoalSlotGuard {
    fn drop(&mut self) {
        self.gate.release(self.generation);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn is_running(gate: &ConcurrencyGate) -> bool {
        gate.state.lock().running.is_some()
    }

    fn current_generation(gate: &ConcurrencyGate) -> u64 {
        gate.state.lock().generation
    }

    fn admit(gate: &ConcurrencyGate, force: bool) -> u64 {
        match gate.try_admit(60, force) {
            Admission::Admitted { generation } => generation,
            Admission::AlreadyRunning { .. } => panic!("expected admission"),
        }
    }

    #[test]
    fn first_admit_starts_generation_at_one() {
        let gate = ConcurrencyGate::new();
        assert_eq!(admit(&gate, false), 1);
        assert!(is_running(&gate));
    }

    #[test]
    fn rejects_concurrent_goal_without_force() {
        let gate = ConcurrencyGate::new();
        admit(&gate, false);
        match gate.try_admit(60, false) {
            Admission::AlreadyRunning { remaining_secs } => assert!(remaining_secs <= 60),
            Admission::Admitted { .. } => panic!("expected rejection while running"),
        }
    }

    #[test]
    fn force_readmits_and_bumps_generation() {
        let gate = ConcurrencyGate::new();
        let g1 = admit(&gate, false);
        let g2 = admit(&gate, true);
        assert_eq!(g2, g1 + 1);
        assert!(is_running(&gate));
    }

    #[test]
    fn guard_with_current_generation_frees_slot() {
        let gate = ConcurrencyGate::new();
        let generation = admit(&gate, false);
        let guard = gate.clone().into_slot_guard(generation);
        assert!(is_running(&gate));
        drop(guard);
        assert!(!is_running(&gate));
    }

    #[test]
    fn stale_guard_drop_does_not_clobber_newer_admission() {
        // The documented force-clobber race: goal A is admitted, a `--force`
        // goal B takes over (bumping the generation), then A's late release must
        // be a no-op rather than wiping B's freshly installed slot.
        let gate = ConcurrencyGate::new();
        let gen_a = admit(&gate, false);
        let guard_a = gate.clone().into_slot_guard(gen_a);

        let gen_b = admit(&gate, true); // B force-admits over A.
        assert_ne!(gen_a, gen_b);

        drop(guard_a); // A's task finally winds down, after B was admitted.

        assert!(is_running(&gate), "B's slot must survive A's stale release");
        assert_eq!(current_generation(&gate), gen_b);
    }

    #[test]
    fn clear_running_frees_slot() {
        let gate = ConcurrencyGate::new();
        admit(&gate, false);
        gate.clear_running();
        assert!(!is_running(&gate));
    }

    #[test]
    fn next_goal_admits_after_guard_release() {
        let gate = ConcurrencyGate::new();
        let generation = admit(&gate, false);
        drop(gate.clone().into_slot_guard(generation));
        // Slot is free again, so a fresh goal is admitted rather than rejected.
        assert_eq!(admit(&gate, false), generation + 1);
    }
}
