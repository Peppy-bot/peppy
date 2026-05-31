//! Goal-admission concurrency gate shared by the built-in single-goal actions
//! (`node_add`, `node_build`, `node_run`, `stack_launch`, `repo_refresh`).
//!
//! Each of those actions allows only one in-flight task at a time and rejects
//! concurrent goals with `"action already in progress (times out in Xs)"`.
//! `node_add` and `node_build` additionally support `--force`, which cancels
//! the in-flight task before admitting the new one. On force, `try_admit`
//! signals the old task's cancel token and hands its `JoinHandle` back to the
//! caller (see [`Admission::Admitted::superseded`]); the caller decides whether
//! to `abort()` it (`node_add`, which overwrites the entity) or `await` its
//! cooperative teardown (`node_build`, which reuses the staged working dir).
//! Actions without force simply never pass `force = true`.
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
//! A `--force` goal supersedes that task and admits a replacement while the
//! superseded task is still winding down (whether it is later `abort()`ed or
//! awaited to cooperative completion), so a naive release could clear the
//! *replacement's* slot and break the single-goal invariant. Each admission
//! therefore carries a monotonic `generation`: the work task holds a
//! [`GoalSlotGuard`] for its generation, and the guard's drop frees the slot
//! only while that generation is still current. A superseded task's release is
//! thus a safe no-op.

use parking_lot::Mutex;
use peppylib::messaging::GoalContext;
use peppylib::types::Payload;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

/// Bounded wait for a gate-cancelled task's cooperative teardown after its
/// `CancellationToken` has been signaled. Both single-goal callers that signal
/// a cancel and then await the doomed task rely on this same budget:
///
/// - `services/node/builder.rs` on a `--force` `node_build`: after `try_admit`
///   signals the displaced build's token, it awaits the superseded handle for
///   up to this long so the old build SIGKILLs + reaps its child and rolls the
///   entity back to `Added` before the new build starts.
/// - `services/stack/launch.rs` on a run-phase idle/max timeout: after signaling
///   the run-phase token it awaits the phase future for up to this long so it
///   can SIGKILL the child, unregister the `Starting` instance, and clear temp
///   files before the launch failure is returned.
///
/// On expiry both callers fall back to dropping the future and surface a
/// transient/timeout failure rather than wedging.
pub(crate) const COOPERATIVE_TEARDOWN_BUDGET: Duration = Duration::from_secs(30);

/// Outcome of [`ConcurrencyGate::try_admit`].
pub(crate) enum Admission {
    /// The new goal was admitted; the gate clock has been started. The
    /// `generation` identifies this admission — the work task hands it to its
    /// [`GoalSlotGuard`] so the slot is freed only while this admission is still
    /// the current one (a later `--force` goal bumps the generation).
    Admitted {
        generation: u64,
        /// The task this admission force-displaced, if any. Its cancel token has
        /// already been signaled inside `try_admit`; the caller awaits this
        /// handle (bounded) so the old task's cooperative teardown (SIGKILL +
        /// reap the build child, roll the entity back to `Added`, re-attach the
        /// working dir) finishes before the new build starts. `None` on a cold
        /// admission (nothing was running).
        superseded: Option<JoinHandle<()>>,
    },
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
    /// previous task's cancel token is signaled and its `JoinHandle` is *handed
    /// back* in [`Admission::Admitted::superseded`]; it is deliberately NOT
    /// aborted, because `JoinHandle::abort` drops the task future and skips the
    /// cooperative teardown (kill+reap the build child, roll the entity back to
    /// `Added`). The caller awaits the returned handle (bounded) so that
    /// teardown completes before the new build starts. Otherwise the in-flight
    /// goal's `remaining_secs` is returned and the gate is left unchanged so the
    /// caller can encode a rejection.
    pub(crate) fn try_admit(&self, timeout_secs: u64, force: bool) -> Admission {
        let mut state = self.state.lock();
        let mut superseded = None;
        if let Some(running) = &state.running {
            if !force {
                let remaining = Duration::from_secs(running.timeout_secs)
                    .saturating_sub(running.started_at.elapsed())
                    .as_secs();
                return Admission::AlreadyRunning {
                    remaining_secs: remaining,
                };
            }
            // Force: signal cancellation so the spawned task runs its cooperative
            // teardown, then hand its handle back to the caller to await. Do NOT
            // abort; that would drop the future and skip the teardown. Bumping
            // the generation below makes the old task's `GoalSlotGuard` drop a
            // no-op, so its eventual release cannot clobber this admission.
            if let Some(token) = state.cancel_token.take() {
                token.cancel();
            }
            superseded = state.running_task.take();
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
            superseded,
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
/// On the normal path the work task releases it explicitly just before
/// completing the goal, via [`release_then_complete`](Self::release_then_complete):
/// completion is what lets the client observe the goal as done, so the slot must
/// already be free by then. As an RAII guard it also fires on every *other* exit
/// path — an early return, a panic unwinding the task, or a `--force`
/// `JoinHandle::abort` dropping the future — so the gate is never left stuck
/// "in progress" (the failure mode every future goal would then hit with
/// "action already in progress" until the daemon restarts). When a later
/// `--force` goal has already taken over, bumping the generation, the drop is a
/// safe no-op and cannot clobber the new owner's slot — the single-goal
/// invariant the generation protects.
///
/// The work task completes and drops its `GoalContext` separately; the SDK
/// retains the result for its grace window so the client's fetch still resolves.
#[must_use = "the guard frees the gate when dropped; release it via release_then_complete or hold it for the task's lifetime"]
pub(crate) struct GoalSlotGuard {
    gate: ConcurrencyGate,
    generation: u64,
}

impl GoalSlotGuard {
    /// Releases this slot, then completes the goal — in that order, which is the
    /// whole point of bundling them. [`GoalContext::complete`] closes the goal's
    /// feedback stream and publishes its fetchable result, so a client draining
    /// feedback learns the goal is done and can fire its next single-goal action
    /// the instant `complete` returns. Were the slot still held then, that next
    /// action would be rejected with `"action already in progress"` for the
    /// window until this task unwound to the guard's end-of-scope drop — a race
    /// the client can win because `complete` notifies it (in-process) before the
    /// task winds down. Releasing first closes the window.
    ///
    /// The release is generation-checked like any guard drop, so it stays a safe
    /// no-op when a later `--force` goal has already taken over.
    pub(crate) async fn release_then_complete(self, goal_ctx: &GoalContext, payload: Payload) {
        // Explicit drop: without it `self` would live until the end of this
        // function — i.e. until after `complete` — which is exactly the ordering
        // this method exists to avoid.
        drop(self);
        let _ = goal_ctx.complete(payload).await;
    }
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
            Admission::Admitted { generation, .. } => generation,
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

    /// A `--force` admission must hand the displaced task's `JoinHandle` back to
    /// the caller (so it can await cooperative teardown) instead of aborting it.
    /// The token is signaled, so awaiting the returned handle resolves `Ok(())`,
    /// proving the task finished cooperatively rather than being `abort()`ed
    /// (which would make the join return a cancelled `JoinError`).
    #[tokio::test]
    async fn force_supersede_returns_old_handle_without_abort() {
        let gate = ConcurrencyGate::new();

        // Cold admission, then register a real task that runs until its token
        // fires, mirroring how a build handler installs its task via `set_task`.
        let gen_a = admit(&gate, false);
        let token = CancellationToken::new();
        let token_for_task = token.clone();
        let task = tokio::spawn(async move {
            token_for_task.cancelled().await;
        });
        gate.set_task(task, token);

        // Force-admit over it.
        let (gen_b, superseded) = match gate.try_admit(60, true) {
            Admission::Admitted {
                generation,
                superseded,
            } => (generation, superseded),
            Admission::AlreadyRunning { .. } => panic!("force admission must succeed"),
        };
        assert_eq!(gen_b, gen_a + 1, "force admission bumps the generation");
        assert!(is_running(&gate));

        let old_task = superseded.expect("the displaced task must be handed back");
        assert!(
            !old_task.is_finished(),
            "the displaced task must not be aborted by `try_admit`"
        );
        let join = old_task.await;
        assert!(
            join.is_ok(),
            "the displaced task should finish cooperatively after its token was signaled, \
             not be aborted (a cancelled join would be `Err`)"
        );
    }
}
