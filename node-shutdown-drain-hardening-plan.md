# Plan: hardening the shutdown-drain completion signal

Follow-up to the force-kill fix (commit `75a8451`). Scope: the one fragility in
the Issue 1 fix — the drain hook in `crates/peppylib-py/src/runtime.rs` relies
on CPython internals to complete the future it awaits.

## The concern

The fix makes `_drain` end with `event_loop.call_soon(event_loop.stop)` instead
of a direct `event_loop.stop()`, so the `run_coroutine_threadsafe` future the
drain hook awaits gets completed before the loop exits. That correctness rests
on two CPython behaviors, neither of which is a documented contract:

1. `_run_once` runs the ready-batch it snapshotted at entry and `run_forever`
   checks `_stopping` only **after** that batch, so the deferred `stop` and the
   future's set-state callback (scheduled in the same prior iteration) both run
   in one batch before the loop exits.
2. `run_coroutine_threadsafe`'s `_chain_future` completes a
   `concurrent.futures.Future` destination **synchronously** (its `dest_loop` is
   `None`), so completion is one scheduling hop, not two.

If a future CPython release changed either, the awaited future could be orphaned
again.

## Why the severity is low (the key fact)

The Issue 2 fix is itself a structural backstop. `force_kill_deadline(grace) =
grace + event-loop-join + finalize` is **strictly greater than `grace`**, and
the loop is stopped by `call_soon(stop)` independently of whether the future
completes. So if the future were orphaned again:

- the drain hook blocks, `run_shutdown_hooks` times out at `grace`;
- the loop thread has already exited (the stop is independent of the future), so
  `quiesce()`'s join returns immediately;
- the process exits at roughly `grace`, which is **below** the daemon's
  `force_kill_deadline` — so the daemon reports it **graceful, not force-killed**.

So the original bug (force-kill on every shutdown) **cannot recur** while
`deadline > grace`. A regression here degrades only to *slow* shutdown (instant
→ ~`grace`), and that latency regression is already caught by
`crates/peppylib-py/tests/test_runner.py::test_async_node_shuts_down_promptly_not_at_grace_boundary`
plus the two-arm `test_shutdown_drain_deadlock.py`.

This keeps the urgency low — but the goal here is the cleanest implementation
that cannot break in the future, so the recommendation removes the fragility at
its source rather than leaning on the backstop.

## Options

### Option A (recommended) — two-phase teardown: drain and stop become separate steps

Make `_drain` cancel the pending tasks and `await gather(...)`, then **return
without stopping the loop**. Its `run_coroutine_threadsafe` future then completes
through the ordinary path while the loop is still running, so there is nothing to
orphan. The loop stop moves to `quiesce()`, which already owns the thread join:
it schedules `loop.stop()` and joins. Concretely, `make_shutdown_trigger` is
split into a *drain* operation (cancel + gather, no stop) that the hook awaits
and a *stop* operation that `quiesce()` invokes before joining.

- Effort: medium (restructures `make_shutdown_trigger` and `quiesce()`; the Rust
  hook is essentially unchanged). Risk: low. Blast radius: the async-setup
  teardown only. Maintenance: low — each piece has a single responsibility
  (drain = wait for cleanup; quiesce = stop and join the loop thread).
- Why it is the cleanest and most future-proof: it removes the root anti-pattern
  (stopping the loop from inside the very coroutine whose completion is awaited
  cross-thread). Both CPython behaviors the current fix leans on — the
  `_run_once` stop-batch ordering and `_chain_future`'s synchronous completion —
  stop being load-bearing entirely. What remains is only the foundational
  guarantee that *a running event loop completes a scheduled coroutine's future*,
  which every `future_into_py` / messaging call in the codebase already depends
  on, so it adds **no new risk surface** and **no new shutdown-time GIL-attach
  code** (the delicate area `py_future.rs` exists to protect).

### Option B — explicit completion event

Keep the stop inside `_drain`, but have it set a pure-Python `threading.Event`
in a `finally` before stopping; the Rust hook awaits that event (`spawn_blocking`
through the attach gate) instead of the future. Robust — the signal is set
synchronously, independent of the loop — but it adds bespoke shutdown-time
GIL-attach bridging in the exact area `py_future.rs` was written to make safe,
and leaves the now-ignored future and the stop-in-drain anti-pattern in place.
Smaller diff than A, but more custom machinery and not as clean structurally.

### Option C — document the backstop + keep the latency guard in CI

No drain rework. Document that `force_kill_deadline > grace` downgrades any
regression to slow-but-graceful (see severity above) and ensure the
prompt-shutdown test runs in CI. Cheapest, but leaves the internals reliance in
place — manages the risk rather than removing it.

### Option D — bound the drain-hook await

Wrap the hook's `done_rx.await` in a timeout and proceed on expiry. Largely
redundant given the `force_kill_deadline` backstop, and a bound shorter than
`grace` would risk cutting legitimate cleanup the drain is meant to wait for.
Not recommended.

## Recommendation

**Option A (two-phase teardown).** It is the cleanest design and the only one
that eliminates the CPython-internals reliance outright instead of compensating
for it: shutdown completion rides the same future-completion path the whole
system already depends on, and it introduces no new shutdown-time GIL machinery.
The `force_kill_deadline > grace` backstop means there is no urgency, so this can
be scheduled rather than rushed — but it is the right end state. B is the
fallback if a minimal diff is ever preferred over the cleaner structure; C and D
only manage the risk.

## Validation

- Restructure `make_shutdown_trigger` into drain + stop; route the hook to the
  drain and `quiesce()` to the stop. Confirm `quiesce()` still stops and joins
  the loop on every async-setup exit path (normal stop, setup error, signal).
- Keep `test_shutdown_drain_deadlock.py`, but update it to mirror the new shape
  (drain returns without stopping; the future completes with the loop running),
  and keep an arm that would fail if the drain ever stops the loop before its
  future completes again.
- Re-run on the pinned interpreter:
  `test_runner.py::test_async_node_shuts_down_promptly_not_at_grace_boundary`,
  the Rust `shutdown_grace_margin.rs`, and the `listen_for_node_stop.rs`
  graceful-after-grace test.
- Add a test that a node with an `on_shutdown` hook still sees the hook run (and
  its cancelled tasks' `finally` blocks run) before the loop is stopped, since A
  moves the stop out of the drain.

## Decision needed

Confirm **Option A** (two-phase teardown), or pick B if a minimal diff is
preferred over the cleaner structure.
