"""Regression test for the node-shutdown drain/stop split.

Behavioral mirror of the async event-loop teardown in
`crates/peppylib-py/src/runtime.rs` (`make_loop_teardown`). On shutdown the
drain hook schedules `_drain()` with `run_coroutine_threadsafe` and awaits the
returned `concurrent.futures.Future`; the loop is stopped separately, by the
stop trigger that `quiesce()` fires from the main thread after the shutdown-hook
phase has finished.

The original bug (now removed): the drain used to end with `event_loop.stop()`.
Stopping the loop from inside the very coroutine whose future the hook awaited
cross-thread orphaned that future, so the hook blocked for the whole grace
window and the daemon force-killed the node. The fix splits the teardown into a
drain (cancel + gather, never stops the loop) and a stop (scheduled separately,
on the loop thread, via `call_soon_threadsafe` so it also wakes an idle loop).

These tests pin the new shape: the drain completes its awaited future with the
loop still running; the stop trigger brings the loop thread down from another
thread; and the stop trigger is the grace-timeout backstop that cancels tasks
the drain hook never reached. Keep this mirror in sync with `make_loop_teardown`
if the teardown shape changes.
"""

import asyncio
import threading
import time


def _start_loop():
    loop = asyncio.new_event_loop()

    def run():
        asyncio.set_event_loop(loop)
        loop.run_forever()

    thread = threading.Thread(target=run, name="peppy-asyncio-loop", daemon=True)
    thread.start()
    return loop, thread


def _wait_running(loop):
    """Block until the loop is running and processing callbacks. Doubles as a
    check that cross-thread scheduling onto the loop works."""
    ready = threading.Event()
    loop.call_soon_threadsafe(ready.set)
    assert ready.wait(timeout=2.0), "event loop did not start"


def _make_teardown(loop):
    """Mirror of `make_loop_teardown` in runtime.rs: a drain trigger (cancel +
    gather, no stop) and a stop trigger (cancel + stop, marshalled onto the loop
    thread). Kept faithful so a divergence in the real teardown shows up here."""

    def _cancel_other_tasks():
        current = asyncio.current_task()
        pending = [task for task in asyncio.all_tasks() if task is not current]
        for task in pending:
            task.cancel()
        return pending

    async def _drain():
        pending = _cancel_other_tasks()
        if pending:
            await asyncio.gather(*pending, return_exceptions=True)

    def _drain_trigger():
        if not loop.is_running():
            return None
        return asyncio.run_coroutine_threadsafe(_drain(), loop)

    def _stop_loop():
        _cancel_other_tasks()
        loop.call_soon(loop.stop)

    def _stop_trigger():
        if not loop.is_running():
            return
        try:
            loop.call_soon_threadsafe(_stop_loop)
        except RuntimeError:
            pass

    return _drain_trigger, _stop_trigger


def _spawn_worker(loop, started, finally_ran):
    """A long-lived task like a node's emit loop, with a finally block whose
    execution we can observe (it runs while the tokio runtime is still alive).

    `started` is set once the task is suspended at its await inside the try, so
    callers can cancel a genuinely-running task. This mirrors the real teardown,
    where background tasks have been running since setup; a task cancelled before
    it enters its try would skip the finally, which is a test artifact, not the
    behavior under test."""

    async def worker():
        try:
            started.set()
            while True:
                await asyncio.sleep(0.05)
        finally:
            finally_ran.set()

    return asyncio.run_coroutine_threadsafe(worker(), loop)


def test_drain_completes_its_future_with_the_loop_still_running():
    """The fix: the drain cancels + gathers and completes the future the hook
    awaits THROUGH THE ORDINARY PATH, with the loop still running. Stopping the
    loop is a separate step owned by quiesce, so the future is never orphaned."""
    loop, thread = _start_loop()
    _wait_running(loop)
    drain_trigger, stop_trigger = _make_teardown(loop)
    started, finally_ran = threading.Event(), threading.Event()
    worker_future = _spawn_worker(loop, started, finally_ran)
    # Let the worker reach its await inside its try before we drain.
    assert started.wait(timeout=2.0)
    assert not worker_future.done()

    drain_future = drain_trigger()

    # The Rust hook bridges this completion into the oneshot it awaits
    # (notify_on_future_done -> add_done_callback).
    done = threading.Event()
    drain_future.add_done_callback(lambda _f: done.set())

    assert done.wait(timeout=2.0), (
        "drain future never completed: the drain must finish its cancel + gather "
        "and complete the future the shutdown hook awaits"
    )
    assert finally_ran.is_set(), "cancelled task's finally cleanup did not run"
    # The defining property of the split: draining must NOT stop the loop.
    # Stopping it from inside the awaited coroutine is exactly what orphaned the
    # future in the original bug.
    assert loop.is_running(), "drain must leave the loop running; the stop is quiesce's job"

    # quiesce stops the loop separately, from another thread.
    stop_trigger()
    thread.join(timeout=2.0)
    assert not thread.is_alive(), "stop trigger did not stop the event-loop thread"


def test_bare_cross_thread_stop_leaves_an_idle_loop_running():
    """Pinned so nobody replaces `call_soon_threadsafe(stop)` with a bare
    `loop.stop()` from the main thread: `loop.stop()` only sets a flag and does
    not wake a loop blocked in select, so an idle loop keeps running. The stop
    trigger marshals the stop onto the loop thread via `call_soon_threadsafe`,
    which writes the loop's self-pipe and wakes it."""
    loop, thread = _start_loop()
    _wait_running(loop)
    # Let the loop finish the wake-up iteration and settle back into a blocking
    # select() with nothing scheduled. Without this, the bare stop() below races
    # the loop's post-iteration _stopping check (which would observe it and exit),
    # whereas the point being pinned is that a bare stop cannot wake a loop that
    # is already idle in select().
    time.sleep(0.1)

    # Bare cross-thread stop on an idle loop: sets _stopping but does not wake it.
    loop.stop()
    thread.join(timeout=0.5)
    assert thread.is_alive(), (
        "expected a bare cross-thread loop.stop() to leave an idle loop running; "
        "if this now stops it, asyncio stop() semantics changed and the "
        "call_soon_threadsafe marshalling may no longer be necessary"
    )

    # The correct cross-thread stop does wake and stop it.
    loop.call_soon_threadsafe(loop.stop)
    thread.join(timeout=2.0)
    assert not thread.is_alive(), "call_soon_threadsafe(stop) did not stop the loop"


def test_stop_trigger_cancels_tasks_the_drain_hook_never_reached():
    """Backstop for the grace-timeout path: if a slow user shutdown hook
    exhausted the grace window so the drain hook was abandoned before it could
    cancel and gather, the stop trigger is the FIRST and ONLY thing to bring the
    loop down. It must cancel the still-pending tasks (running their finally
    cleanup) before stopping, and it must stop the loop regardless. Without the
    cancel, loop.stop() would exit run_forever with a task possibly mid native
    call, the SIGSEGV-at-finalization this teardown exists to prevent."""
    loop, thread = _start_loop()
    _wait_running(loop)
    _drain_trigger, stop_trigger = _make_teardown(loop)
    started, finally_ran = threading.Event(), threading.Event()
    _spawn_worker(loop, started, finally_ran)  # a live task the drain hook never reached
    assert started.wait(timeout=2.0)

    stop_trigger()

    thread.join(timeout=2.0)
    assert not thread.is_alive(), "stop trigger did not bring the loop thread down"
    assert finally_ran.is_set(), (
        "stop trigger must cancel the pending task and let its finally cleanup "
        "run before stopping the loop"
    )
