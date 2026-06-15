"""Regression test for the node-shutdown drain deadlock.

Behavioral mirror of the async event-loop teardown in
`crates/peppylib-py/src/runtime.rs` (`make_shutdown_trigger`). On shutdown the
drain hook schedules `_drain()` with `run_coroutine_threadsafe` and awaits the
returned `concurrent.futures.Future` before the process is allowed to exit.

The bug: if `_drain` ends with a direct `event_loop.stop()`, CPython runs the
current callback batch and exits `run_forever`, dropping the callback that
`run_coroutine_threadsafe` schedules to complete that future. The future never
completes, its done-callback never fires, and the hook (which awaits it) blocks
for the whole shutdown-grace window, so the daemon force-kills the node.

The fix defers the stop one scheduling hop (`event_loop.call_soon(event_loop.stop)`)
so the loop runs one more full ready-queue drain, in which the future's
set-state callback fires, before the stop takes effect.

These tests reproduce both arms in isolation: the fixed drain must complete the
awaited future; the buggy drain must orphan it. Keep this mirror in sync with
`make_shutdown_trigger` if its teardown shape changes.
"""

import asyncio
import threading


def _start_loop():
    loop = asyncio.new_event_loop()

    def run():
        asyncio.set_event_loop(loop)
        loop.run_forever()

    thread = threading.Thread(target=run, name="peppy-asyncio-loop", daemon=True)
    thread.start()
    return loop, thread


def _drain_factory(loop, *, defer_stop):
    """Build a `_drain` coroutine mirroring runtime.rs, with the stop strategy
    selectable so both the fixed and buggy arms can be exercised."""

    async def _drain():
        current = asyncio.current_task()
        pending = [task for task in asyncio.all_tasks(loop) if task is not current]
        for task in pending:
            task.cancel()
        if pending:
            await asyncio.gather(*pending, return_exceptions=True)
        if defer_stop:
            loop.call_soon(loop.stop)  # the fix
        else:
            loop.stop()  # the bug

    return _drain


def _spawn_worker(loop, finally_ran):
    """A long-lived task like a node's emit loop, with a finally block whose
    execution we can observe (it runs while the tokio runtime is still alive)."""

    async def worker():
        try:
            while True:
                await asyncio.sleep(0.05)
        finally:
            finally_ran.set()

    return asyncio.run_coroutine_threadsafe(worker(), loop)


def test_deferred_stop_completes_the_awaited_future():
    """The fix: the future the drain hook awaits must complete, and the
    cancelled task's finally cleanup must have run."""
    loop, thread = _start_loop()
    finally_ran = threading.Event()
    worker_future = _spawn_worker(loop, finally_ran)
    # Let the worker reach its await before we drain.
    assert not worker_future.done()

    drain_future = asyncio.run_coroutine_threadsafe(
        _drain_factory(loop, defer_stop=True)(), loop
    )

    # The Rust hook bridges this completion into the oneshot it awaits
    # (notify_on_future_done -> add_done_callback).
    done = threading.Event()
    drain_future.add_done_callback(lambda _f: done.set())

    assert done.wait(timeout=2.0), (
        "drain future never completed: event_loop.call_soon(event_loop.stop) "
        "must let the loop run one more drain so run_coroutine_threadsafe can "
        "complete the future the shutdown hook awaits"
    )
    assert finally_ran.is_set(), "cancelled task's finally cleanup did not run"
    thread.join(timeout=2.0)
    assert not thread.is_alive(), "event-loop thread did not exit after stop"


def test_direct_stop_orphans_the_awaited_future():
    """The bug, pinned so nobody reverts `call_soon(stop)` back to `stop()`:
    a direct `loop.stop()` as the drain's last statement leaves the awaited
    future uncompleted, which is exactly what deadlocks the shutdown hook."""
    loop, thread = _start_loop()
    finally_ran = threading.Event()
    _spawn_worker(loop, finally_ran)

    drain_future = asyncio.run_coroutine_threadsafe(
        _drain_factory(loop, defer_stop=False)(), loop
    )
    done = threading.Event()
    drain_future.add_done_callback(lambda _f: done.set())

    assert not done.wait(timeout=0.5), (
        "expected the buggy direct-stop drain to orphan its future; if this now "
        "completes, the asyncio stop() semantics changed and the deferred-stop "
        "fix may no longer be necessary"
    )
    # The loop did stop and the thread exited; only the future signal was lost.
    thread.join(timeout=2.0)
    assert not thread.is_alive()
