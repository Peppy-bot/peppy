use crate::messaging::{PyMessengerHandle, PyProducerRef};
use peppylib::runtime::CancellationToken;
use peppylib::runtime::{NodeBuilder, NodeRunner, Processor, StandaloneConfig};
use pyo3::exceptions::PyRuntimeError;
use pyo3::prelude::*;
use pyo3::types::{PyCFunction, PyDict};
use pythonize::{depythonize, pythonize};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

type SharedPyError = Arc<Mutex<Option<PyErr>>>;

/// The node's persistent asyncio event loop, shared with every
/// [`PyNodeRunner`] handed to user code. Filled by [`start_async_setup`] when
/// the setup function is async; stays `None` for synchronous-setup nodes.
/// Shutdown hooks read it to decide where a hook coroutine should run.
type SharedEventLoopSlot = Arc<Mutex<Option<Py<PyAny>>>>;

/// Handles produced by [`start_async_setup`] for the async-setup flow.
struct AsyncSetup {
    /// Signalled when the setup coroutine completes (any outcome).
    setup_complete_rx: tokio::sync::oneshot::Receiver<()>,
    /// The `concurrent.futures.Future` of the setup coroutine.
    setup_future: Py<PyAny>,
    /// Teardown handles for the loop thread, fired after `builder.run()`.
    event_loop_shutdown: EventLoopShutdown,
    /// Held by the setup phase; dropping it disarms the shutdown-monitor
    /// thread, which must not fire the loop drain once the runner's
    /// shutdown-hook phase owns it (see `start_async_setup`).
    monitor_disarm: tokio::sync::oneshot::Sender<()>,
}

/// How long the main thread waits for the asyncio event-loop thread to drain
/// on shutdown before giving up and letting the process exit anyway. Shared
/// with the daemon (via `config`) so its force-kill deadline always allows for
/// this join; see [`config::peppy_config::EVENT_LOOP_JOIN_BUDGET_SECS`].
fn event_loop_join_timeout_secs() -> f64 {
    config::peppy_config::EVENT_LOOP_JOIN_BUDGET_SECS as f64
}

/// Teardown handles for the persistent asyncio event-loop thread.
///
/// On shutdown the loop thread must be brought down deterministically. Its
/// background tasks may be executing native code (pycapnp serialization, a
/// pyo3 future) and, because the thread is a daemon, CPython would otherwise
/// kill it mid-call during interpreter finalization, segfaulting the process.
struct EventLoopShutdown {
    /// Pure-Python callable that stops the loop (cancelling any still-pending
    /// tasks first, on the loop thread) so its thread can exit. Idempotent and
    /// safe to call from any thread; a no-op once the loop is no longer running.
    stop_trigger: Py<PyAny>,
    /// The daemon thread running the event loop. Joined before `run` returns
    /// so no native call is in flight when the interpreter finalizes.
    thread: Py<PyAny>,
}

impl EventLoopShutdown {
    /// Cancel pending tasks, stop the loop, and join its thread (bounded by
    /// [`event_loop_join_timeout_secs`]). CPython releases the GIL while
    /// joining, so the loop thread can observe the cancellation and exit.
    /// Best-effort: if a background task refuses to cancel within the timeout,
    /// shutdown proceeds rather than hanging process exit.
    fn quiesce(self) {
        let join_timeout = event_loop_join_timeout_secs();
        let still_alive = Python::try_attach(|py| -> PyResult<bool> {
            // Schedule the loop stop (which cancels any straggler tasks first,
            // on the loop thread); the join below releases the GIL so the loop
            // thread can run it. Best-effort: the stop trigger swallows its own
            // errors and the join must run even if it somehow raises, so the
            // daemon thread cannot outlive `run`.
            let _ = self.stop_trigger.bind(py).call0();
            let thread = self.thread.bind(py);
            thread.call_method1("join", (join_timeout,))?;
            thread.call_method0("is_alive")?.is_truthy()
        });
        if let Some(Ok(true)) = still_alive {
            eprintln!(
                "peppy: asyncio event-loop thread did not stop within {join_timeout:.0}s; \
                 proceeding with shutdown (a background task may be ignoring cancellation)"
            );
        }
    }
}

/// Best-effort stop and join of the asyncio event-loop thread, used when
/// [`start_async_setup`] fails after the loop thread has started. The loop is
/// already running `run_forever`, and on an error path the [`EventLoopShutdown`]
/// handle is never returned to `run`, so nothing else stops it. Left running,
/// the daemon thread would outlive `run` and be killed mid native call when the
/// interpreter finalizes (the SIGSEGV this whole teardown exists to prevent).
fn abort_loop_thread(event_loop: &Bound<'_, PyAny>, thread: &Bound<'_, PyAny>) {
    // A bare loop.stop() from this (non-loop) thread only sets a flag and would
    // not wake the idle loop; schedule it cross-thread so the loop wakes and
    // run_forever returns. The join then releases the GIL so the loop thread can
    // run the stop and exit. No task cancellation: setup failed, so there is no
    // user cleanup to run, and the join is what actually prevents the SIGSEGV.
    if let Ok(stop) = event_loop.getattr("stop") {
        let _ = event_loop.call_method1("call_soon_threadsafe", (stop,));
    }
    let _ = thread.call_method1("join", (event_loop_join_timeout_secs(),));
}

fn peppy_io_err(message: impl Into<String>) -> peppylib::PeppyError {
    peppylib::PeppyError::Io(std::io::Error::other(message.into()))
}

/// Enable Python's faulthandler so a fatal signal (e.g. a SIGSEGV raised by a
/// native extension on a background thread) prints a traceback for every thread
/// to stderr instead of dying silently. Best-effort and idempotent; it only
/// fires on a crash, so it is safe to leave on in production.
fn enable_faulthandler(py: Python<'_>) {
    let _ = py
        .import("faulthandler")
        .and_then(|module| module.call_method0("enable"))
        .map(|_| ());
}

fn call_setup_function(
    py: Python<'_>,
    setup_fn: &Py<PyAny>,
    params: &serde_json::Value,
    node_runner: &Arc<NodeRunner>,
    event_loop_slot: &SharedEventLoopSlot,
) -> PyResult<Py<PyAny>> {
    let py_params = pythonize(py, params)
        .map_err(|e| PyRuntimeError::new_err(format!("failed to convert params to Python: {e}")))?
        .unbind();
    let py_params = hydrate_parameters(py, py_params)?;
    let py_runner = Py::new(
        py,
        PyNodeRunner::with_event_loop_slot(Arc::clone(node_runner), Arc::clone(event_loop_slot)),
    )
    .map_err(|e| {
        PyRuntimeError::new_err(format!("failed to create NodeRunner Python wrapper: {e}"))
    })?;
    setup_fn.call1(py, (py_params, py_runner))
}

/// Converts a plain Python dict into the generated `Parameters` dataclass
/// instance by importing `peppygen.parameters.Parameters` and calling its
/// `from_dict` classmethod.
fn hydrate_parameters(py: Python<'_>, params: Py<PyAny>) -> PyResult<Py<PyAny>> {
    let module = py.import("peppygen.parameters")?;
    let params_cls = module.getattr("Parameters")?;
    let instance = params_cls.call_method1("from_dict", (params.bind(py),))?;
    Ok(instance.unbind())
}

fn is_awaitable(value: &Bound<'_, PyAny>) -> PyResult<bool> {
    value.hasattr("__await__")
}

/// Coerce an awaitable into a coroutine object.
///
/// Both schedulers used for shutdown hooks (`asyncio.run_coroutine_threadsafe`
/// and `asyncio.run`) reject awaitables that are not coroutine objects, such
/// as Tasks, Futures, and custom `__await__` classes. Anything that
/// `asyncio.iscoroutine` does not accept is wrapped in a pure-Python coroutine
/// that simply awaits it. Pure Python (not a PyCFunction) for the same reason
/// as `create_event_loop_helpers`: the wrapper body runs on an event loop
/// thread and must not put `catch_unwind` in the call path.
fn coerce_to_coroutine<'py>(
    py: Python<'py>,
    awaitable: Bound<'py, PyAny>,
) -> PyResult<Bound<'py, PyAny>> {
    let asyncio = py.import("asyncio")?;
    if asyncio
        .call_method1("iscoroutine", (&awaitable,))?
        .is_truthy()?
    {
        return Ok(awaitable);
    }
    let wrapper = PyModule::from_code(
        py,
        c"
async def wrap_awaitable(awaitable):
    return await awaitable
",
        c"_peppy_awaitable_wrapper.py",
        c"_peppy_awaitable_wrapper",
    )?;
    wrapper.call_method1("wrap_awaitable", (awaitable,))
}

/// Print a Python error raised by a shutdown hook. Hooks are contained: one
/// failing hook must not stop the remaining ones, so errors are printed (with
/// traceback) rather than propagated.
fn print_shutdown_hook_error(py: Python<'_>, err: &PyErr) {
    eprintln!("peppy: shutdown hook raised:");
    err.print(py);
}

/// Bridge a `concurrent.futures.Future`'s completion into a tokio oneshot, so
/// async Rust can await it without holding the GIL.
fn notify_on_future_done(
    py: Python<'_>,
    future: &Bound<'_, PyAny>,
) -> PyResult<tokio::sync::oneshot::Receiver<()>> {
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    let tx = Mutex::new(Some(tx));
    let done_cb = PyCFunction::new_closure(
        py,
        Some(c"_peppy_future_done"),
        None,
        move |_args, _kwargs| {
            if let Ok(mut guard) = tx.lock()
                && let Some(tx) = guard.take()
            {
                let _ = tx.send(());
            }
            Ok::<(), PyErr>(())
        },
    )?;
    future.call_method1("add_done_callback", (done_cb,))?;
    Ok(rx)
}

/// Where (and whether) a hook callback's returned awaitable still needs to be
/// driven after the initial GIL-attached call.
enum HookContinuation {
    /// Synchronous callback (or scheduling failed): nothing left to do.
    Done,
    /// Awaitable scheduled on the node's running asyncio loop; wait for the
    /// returned `concurrent.futures.Future` and then surface its exception.
    OnNodeLoop(tokio::sync::oneshot::Receiver<()>, Py<PyAny>),
    /// No running node loop (synchronous-setup node): the awaitable must be
    /// driven on a dedicated one-off loop via `asyncio.run`.
    NeedsOwnLoop(Py<PyAny>),
}

/// Run one registered Python shutdown hook to completion.
///
/// Calls the callback under the GIL; if it returns an awaitable, drives it on
/// the node's asyncio event loop when one is running (async-setup nodes, where
/// the loop outlives user hooks by design: the loop drain is the final hook),
/// or on a one-off `asyncio.run` loop otherwise. The GIL is released while
/// waiting, so the loop thread can execute the coroutine. Errors are printed
/// and swallowed; the surrounding hook phase is bounded by the runner's grace
/// window.
async fn run_python_shutdown_hook(callback: Py<PyAny>, event_loop_slot: SharedEventLoopSlot) {
    let continuation = crate::py_future::try_attach_gated(|py| {
        let result = match callback.bind(py).call0() {
            Ok(result) => result,
            Err(err) => {
                print_shutdown_hook_error(py, &err);
                return HookContinuation::Done;
            }
        };
        match is_awaitable(&result) {
            Ok(false) => return HookContinuation::Done,
            Ok(true) => {}
            Err(err) => {
                print_shutdown_hook_error(py, &err);
                return HookContinuation::Done;
            }
        }
        // Both scheduling branches below require a coroutine object, so wrap
        // any other awaitable (Task, Future, custom __await__) first.
        let result = match coerce_to_coroutine(py, result) {
            Ok(coroutine) => coroutine,
            Err(err) => {
                print_shutdown_hook_error(py, &err);
                return HookContinuation::Done;
            }
        };

        let node_loop = event_loop_slot
            .lock()
            .ok()
            .and_then(|slot| slot.as_ref().map(|l| l.clone_ref(py)));
        let Some(node_loop) = node_loop else {
            return HookContinuation::NeedsOwnLoop(result.unbind());
        };
        let node_loop = node_loop.into_bound(py);
        let loop_is_running = node_loop
            .call_method0("is_running")
            .and_then(|v| v.is_truthy())
            .unwrap_or(false);
        if !loop_is_running {
            return HookContinuation::NeedsOwnLoop(result.unbind());
        }

        let scheduled = (|| -> PyResult<HookContinuation> {
            let asyncio = py.import("asyncio")?;
            let future = asyncio.call_method1("run_coroutine_threadsafe", (&result, &node_loop))?;
            let done_rx = notify_on_future_done(py, &future)?;
            Ok(HookContinuation::OnNodeLoop(done_rx, future.unbind()))
        })();
        scheduled.unwrap_or_else(|err| {
            print_shutdown_hook_error(py, &err);
            HookContinuation::Done
        })
    });

    match continuation {
        None | Some(HookContinuation::Done) => {}
        Some(HookContinuation::OnNodeLoop(done_rx, future)) => {
            let _ = done_rx.await;
            let _ = crate::py_future::try_attach_gated(|py| {
                // `exception()` itself raises if the future was cancelled
                // (loop torn down mid-hook); both shapes are just printed.
                match future.bind(py).call_method0("exception") {
                    Ok(exc) if !exc.is_none() => {
                        eprintln!("peppy: shutdown hook raised:");
                        let _ = py
                            .import("traceback")
                            .and_then(|tb| tb.call_method1("print_exception", (exc,)));
                    }
                    Ok(_) => {}
                    Err(err) => print_shutdown_hook_error(py, &err),
                }
            });
        }
        Some(HookContinuation::NeedsOwnLoop(awaitable)) => {
            // A blocking task keeps the GIL wait off the runtime worker that
            // is driving the hook phase.
            let _ = tokio::task::spawn_blocking(move || {
                let _ = crate::py_future::try_attach_gated(|py| {
                    let outcome = py
                        .import("asyncio")
                        .and_then(|asyncio| asyncio.call_method1("run", (awaitable.bind(py),)));
                    if let Err(err) = outcome {
                        print_shutdown_hook_error(py, &err);
                    }
                });
            })
            .await;
        }
    }
}

/// Create a Python module containing pure-Python helpers for the asyncio event
/// loop thread.
///
/// The two closures that run on the event loop thread (the exception handler and
/// the `run_forever` wrapper) **must** be plain Python functions — not PyO3
/// `PyCFunction` closures.  PyO3 wraps every `PyCFunction` invocation in
/// `catch_unwind`, and Rust's `catch_unwind` cannot intercept foreign (non-Rust)
/// exceptions such as those raised by C/C++ extensions (e.g. pycapnp).  If such
/// an exception propagates through `catch_unwind`, the process aborts with the
/// opaque message *"Rust cannot catch foreign exceptions"* instead of showing the
/// actual traceback.
///
/// By defining these helpers as pure Python (via `PyModule::from_code`), we keep
/// `catch_unwind` out of the call path entirely, letting Python's own `try/except
/// BaseException` handle any exception — foreign or otherwise.
fn create_event_loop_helpers<'py>(py: Python<'py>) -> PyResult<Bound<'py, PyModule>> {
    PyModule::from_code(
        py,
        c"
import sys
import traceback

def make_exception_handler(cancel_token):
    def _handler(loop, context):
        try:
            exc = context.get('exception')
            if exc is not None:
                msg = ''.join(traceback.format_exception(exc))
                print(f'Unhandled exception in async task:\\n{msg}', file=sys.stderr, flush=True)
            elif (message := context.get('message')) is not None:
                print(f'Unhandled exception in async task: {message}', file=sys.stderr, flush=True)
        except BaseException as fmt_err:
            print(f'Error formatting async exception: {fmt_err}', file=sys.stderr, flush=True)
        finally:
            cancel_token.cancel()
    return _handler

def make_run_loop(event_loop, asyncio_mod, cancel_token):
    def _run():
        try:
            asyncio_mod.set_event_loop(event_loop)
            event_loop.run_forever()
        except BaseException as exc:
            try:
                msg = ''.join(traceback.format_exception(exc))
                print(f'Fatal error in peppy asyncio event loop:\\n{msg}', file=sys.stderr, flush=True)
            except BaseException:
                print(f'Fatal error in peppy asyncio event loop: {exc}', file=sys.stderr, flush=True)
            cancel_token.cancel()
    return _run

def make_loop_teardown(event_loop, asyncio_mod):
    def _cancel_other_tasks():
        # Cancel every task except the caller's own and return them. Called from
        # the drain coroutine (current_task is the drain itself, correctly
        # excluded) and from the on-loop stop callback (current_task is None, so
        # nothing is excluded and every task is cancelled). Runs only on the loop
        # thread, where task.cancel() is safe.
        current = asyncio_mod.current_task()
        pending = [task for task in asyncio_mod.all_tasks() if task is not current]
        for task in pending:
            task.cancel()
        return pending

    async def _drain():
        # Cancel pending tasks and wait for their cancellation (and any finally
        # cleanup) to finish, then RETURN without stopping the loop. The loop is
        # stopped separately, by the stop trigger that quiesce fires after the
        # shutdown-hook phase. Because nothing stops the loop here, the future
        # this coroutine completes is set through the ordinary path while the
        # loop is still running, so it can never be orphaned. Removing the loop
        # stop from the awaited coroutine is the whole point of splitting drain
        # from stop. See tests/test_shutdown_drain_deadlock.py.
        pending = _cancel_other_tasks()
        if pending:
            await asyncio_mod.gather(*pending, return_exceptions=True)

    def _drain_trigger():
        # Run the drain on the loop and return its concurrent.futures.Future so
        # the caller can await completion. A no-op (returns None) once the loop
        # is no longer running. Safe to call from any thread and idempotent: a
        # drain with nothing pending completes its future immediately.
        if not event_loop.is_running():
            return None
        return asyncio_mod.run_coroutine_threadsafe(_drain(), event_loop)

    def _stop_loop():
        # Runs ON the loop thread (scheduled by _stop_trigger via
        # call_soon_threadsafe). Cancels any task the drain hook never reached
        # (the grace-timeout path, where a slow user hook exhausted the window
        # so the drain hook was abandoned before it cancelled and gathered),
        # then defers loop.stop() one hop so those cancellations run their
        # finally cleanup before the loop exits. This call_soon(stop) is NOT the
        # orphaning hazard the old in-drain stop was: nothing awaits a future
        # across it (quiesce only joins the loop thread), so the batch ordering
        # here is a best-effort cleanup nicety, not a correctness dependency. No
        # gather: an uncancellable task must never be able to block the stop.
        _cancel_other_tasks()
        event_loop.call_soon(event_loop.stop)

    def _stop_trigger():
        # Stop the loop so its thread can exit before the process tears down.
        # Called from the main thread, so the stop work is marshalled onto the
        # loop thread with call_soon_threadsafe, which also wakes a loop blocked
        # in select; a bare loop.stop() from another thread only sets a flag and
        # would leave an idle loop running. Best-effort and idempotent: a no-op
        # once the loop is no longer running, and any error (the loop closed
        # between the check and the schedule) is swallowed so the caller always
        # proceeds to join the loop thread.
        if not event_loop.is_running():
            return
        try:
            event_loop.call_soon_threadsafe(_stop_loop)
        except RuntimeError:
            pass

    return _drain_trigger, _stop_trigger
",
        c"_peppy_event_loop_helpers.py",
        c"_peppy_event_loop_helpers",
    )
}

/// Start an async setup function on a persistent Python event loop.
///
/// Creates a dedicated asyncio event loop in a background thread and submits
/// the setup coroutine. Returns a channel receiver and future handle so the
/// caller can wait for completion **after releasing the GIL** — the event loop
/// thread needs the GIL to run the coroutine.
///
/// The event loop stays alive after setup returns so that background tasks
/// created via `asyncio.create_task()` continue running.
///
/// On node shutdown (cancellation token triggered), the event loop is stopped
/// and its thread exits. Uncaught exceptions in background tasks cancel the
/// node via the event loop's exception handler.
fn start_async_setup(
    py: Python<'_>,
    setup_awaitable: &Bound<'_, PyAny>,
    node_runner: &Arc<NodeRunner>,
    event_loop_slot: &SharedEventLoopSlot,
) -> PyResult<AsyncSetup> {
    let asyncio = py.import("asyncio")?;
    let threading = py.import("threading")?;

    // 1. Create a new event loop
    let event_loop = asyncio.call_method0("new_event_loop")?;

    // 2. Create pure-Python helpers (see `create_event_loop_helpers` doc comment
    //    for why these must NOT be PyCFunction closures).
    let helpers = create_event_loop_helpers(py)?;
    let cancel_token = Py::new(
        py,
        PyCancellationToken {
            inner: node_runner.cancellation_token().clone(),
        },
    )?;

    // 3. Set exception handler: log traceback + cancel node on uncaught task errors
    let exception_handler = helpers
        .getattr("make_exception_handler")?
        .call1((&cancel_token,))?;
    event_loop.call_method1("set_exception_handler", (exception_handler,))?;

    // 4. Start the event loop in a background thread
    let run_loop =
        helpers
            .getattr("make_run_loop")?
            .call1((&event_loop, &asyncio, &cancel_token))?;

    let thread_kwargs = PyDict::new(py);
    thread_kwargs.set_item("target", run_loop)?;
    thread_kwargs.set_item("name", "peppy-asyncio-loop")?;
    thread_kwargs.set_item("daemon", true)?;
    let thread = threading.call_method("Thread", (), Some(&thread_kwargs))?;
    thread.call_method0("start")?;

    // 5. Publish the loop to the slot shared with every PyNodeRunner, so
    //    shutdown hooks registered from user code know where to run their
    //    coroutines. Done before the setup coroutine is submitted, so any
    //    hook registered during setup sees the loop.
    if let Ok(mut slot) = event_loop_slot.lock() {
        *slot = Some(event_loop.clone().unbind());
    }

    // Steps 6-9 are all fallible and run while the loop thread (started above)
    // is already in `run_forever`, before the `EventLoopShutdown` handle that
    // `quiesce` uses to stop it has been returned. If any step fails, stop and
    // join the loop thread here, or it outlives `run` and is killed mid native
    // call when the interpreter finalizes.
    build_async_setup(
        py,
        &helpers,
        &event_loop,
        &asyncio,
        node_runner,
        setup_awaitable,
        &thread,
    )
    .inspect_err(|_| abort_loop_thread(&event_loop, &thread))
}

/// Steps 6-9 of [`start_async_setup`], split out so any failure can be caught by
/// the caller and turned into a stop+join of the already-running loop thread.
/// Builds the loop teardown, registers the loop-drain shutdown hook, submits the
/// setup coroutine, and spawns the setup-scoped shutdown monitor.
fn build_async_setup(
    py: Python<'_>,
    helpers: &Bound<'_, PyModule>,
    event_loop: &Bound<'_, PyAny>,
    asyncio: &Bound<'_, PyModule>,
    node_runner: &Arc<NodeRunner>,
    setup_awaitable: &Bound<'_, PyAny>,
    thread: &Bound<'_, PyAny>,
) -> PyResult<AsyncSetup> {
    // 6. Build the loop teardown: two pure-Python callables. The drain trigger
    //    cancels pending tasks and gathers them WITHOUT stopping the loop, and
    //    returns the drain's future (or None when the loop is already stopped);
    //    it is fired by the drain hook below and by the setup-scoped shutdown
    //    monitor. The stop trigger stops the loop (cancelling any stragglers
    //    first) and is fired only by the main thread in `quiesce`, after
    //    builder.run() returns. Keeping the stop out of the drain is what lets
    //    the loop outlive the user shutdown hooks and the drain hook's awaited
    //    future complete with the loop still running, never orphaned.
    let teardown = helpers
        .getattr("make_loop_teardown")?
        .call1((event_loop, asyncio))?;
    let drain_trigger = teardown.get_item(0)?;
    let stop_trigger = teardown.get_item(1)?;

    // 7. Register the loop drain as a shutdown hook NOW, before the setup
    //    coroutine can register any user hook. Hooks run in reverse
    //    registration order, so this one runs last: user hooks execute with
    //    the loop (and the node's tokio runtime and messenger) still fully
    //    alive, and only then are the remaining asyncio tasks cancelled and
    //    gathered. Awaiting the drain inside the hook phase keeps the tokio
    //    runtime alive while cancelled tasks run their `finally` cleanup. The
    //    loop itself is stopped later by `quiesce`, not here.
    let drain_trigger_for_hook = drain_trigger.clone().unbind();
    node_runner.on_shutdown(async move {
        let drain_done = crate::py_future::try_attach_gated(|py| {
            let drain_future = match drain_trigger_for_hook.bind(py).call0() {
                Ok(future) => future,
                Err(err) => {
                    print_shutdown_hook_error(py, &err);
                    return None;
                }
            };
            if drain_future.is_none() {
                // Loop already stopped; nothing to wait for.
                return None;
            }
            notify_on_future_done(py, &drain_future)
                .inspect_err(|err| print_shutdown_hook_error(py, err))
                .ok()
        })
        .flatten();
        if let Some(done_rx) = drain_done {
            let _ = done_rx.await;
        }
    });

    // 8. Submit the setup coroutine and bridge its completion into a tokio
    //    oneshot. The caller awaits it with the GIL released (the event loop
    //    thread needs the GIL to run the coroutine) and without blocking its
    //    tokio worker, so the runner's select stays responsive to shutdown
    //    requests and cancellation arriving mid-setup.
    let future = asyncio.call_method1("run_coroutine_threadsafe", (setup_awaitable, event_loop))?;
    let setup_complete_rx = notify_on_future_done(py, &future)?;
    let future_ref = future.unbind();

    // 9. Schedule the shutdown monitor, scoped to the setup window. In
    //    standalone mode the runner awaits the setup future directly, with no
    //    select against cancellation, so a cancelled setup (uncaught task
    //    error, process signal, programmatic cancel) would leave the runner
    //    waiting on a coroutine that nothing else will cancel; this thread
    //    fires the loop teardown to unstick it. Daemon mode observes
    //    cancellation in the runner's select and drops the setup future,
    //    which retires the monitor by dropping the `monitor_disarm` sender:
    //    from then on the drain hook registered above owns the teardown, and
    //    the monitor must not cancel tasks out from under user shutdown
    //    hooks. The `biased` order makes the disarm win a race. The monitor
    //    fires the DRAIN trigger only: cancelling the setup task is what
    //    unsticks the runner, and the loop is stopped later by `quiesce`.
    let drain_trigger_for_monitor = drain_trigger.clone().unbind();
    let cancel_for_shutdown = node_runner.cancellation_token().clone();
    let (disarm_tx, mut disarm_rx) = tokio::sync::oneshot::channel::<()>();
    let rt_handle = tokio::runtime::Handle::current();
    std::thread::Builder::new()
        .name("peppy-asyncio-shutdown".to_string())
        .spawn(move || {
            let cancelled_during_setup = rt_handle.block_on(async {
                tokio::select! {
                    biased;
                    _ = &mut disarm_rx => false,
                    _ = cancel_for_shutdown.cancelled() => true,
                }
            });
            if !cancelled_during_setup {
                return;
            }
            // Gated attach: if this thread is scheduled so late that the
            // attach gate has closed, the main thread has already stopped the
            // loop via `quiesce` (whose stop trigger also cancels), so skipping
            // is safe.
            let _ = crate::py_future::try_attach_gated(|py| -> PyResult<()> {
                drain_trigger_for_monitor.bind(py).call0()?;
                Ok(())
            });
        })
        .map_err(|e| PyRuntimeError::new_err(format!("failed to start shutdown monitor: {e}")))?;

    Ok(AsyncSetup {
        setup_complete_rx,
        setup_future: future_ref,
        event_loop_shutdown: EventLoopShutdown {
            stop_trigger: stop_trigger.unbind(),
            thread: thread.clone().unbind(),
        },
        monitor_disarm: disarm_tx,
    })
}

fn store_python_error(error_slot: &SharedPyError, err: PyErr) {
    if let Ok(mut guard) = error_slot.lock()
        && guard.is_none()
    {
        *guard = Some(err);
    }
}

fn take_python_error(error_slot: &SharedPyError) -> Option<PyErr> {
    error_slot.lock().ok().and_then(|mut guard| guard.take())
}

/// Python wrapper for CancellationToken.
#[pyclass(name = "CancellationToken")]
pub struct PyCancellationToken {
    inner: CancellationToken,
}

#[pymethods]
impl PyCancellationToken {
    /// Returns true if the token has been cancelled.
    fn is_cancelled(&self) -> bool {
        self.inner.is_cancelled()
    }

    /// Cancel the token, notifying all listeners.
    fn cancel(&self) {
        self.inner.cancel();
    }

    /// Wait until the token is cancelled.
    ///
    /// Async counterpart of polling `is_cancelled()`: completes when the node
    /// is asked to shut down (daemon stop, daemon-liveness loss, or Ctrl+C in
    /// standalone mode), or immediately if the token is already cancelled.
    /// Mirrors the Rust `CancellationToken::cancelled()` awaitable.
    fn cancelled<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let token = self.inner.clone();
        crate::py_future::future_into_py(py, async move {
            token.cancelled().await;
            Ok(())
        })
    }
}

/// Python wrapper for NodeRunner.
#[pyclass(name = "NodeRunner")]
pub struct PyNodeRunner {
    pub(crate) inner: Arc<NodeRunner>,
    /// Cached messenger handle — cloning `MessengerHandle` is a cheap `Arc`
    /// bump, but we avoid re-wrapping it on every `messenger()` call.
    cached_messenger: PyMessengerHandle,
    /// The node's persistent asyncio loop, filled once async setup starts.
    /// Read by shutdown hooks to run hook coroutines on the node's loop.
    event_loop_slot: SharedEventLoopSlot,
}

impl PyNodeRunner {
    fn new(node_runner: Arc<NodeRunner>) -> Self {
        Self::with_event_loop_slot(node_runner, Arc::new(Mutex::new(None)))
    }

    fn with_event_loop_slot(
        node_runner: Arc<NodeRunner>,
        event_loop_slot: SharedEventLoopSlot,
    ) -> Self {
        let cached_messenger = PyMessengerHandle {
            inner: node_runner.messenger().clone(),
        };
        Self {
            inner: node_runner,
            cached_messenger,
            event_loop_slot,
        }
    }
}

#[pymethods]
impl PyNodeRunner {
    /// Build a `NodeRunner` in standalone mode from a peppy.json5 path and a
    /// `StandaloneConfig`. Mirrors the Rust-side
    /// `Processor::new_standalone(...)` + `NodeRunner::new(...)` flow used by
    /// `crates/peppylib/tests/core_node/common.rs` so Python integration tests
    /// can stand up a runner without going through `NodeBuilder::run`.
    #[staticmethod]
    fn new_standalone<'py>(
        py: Python<'py>,
        peppy_config_path: String,
        standalone_config: &PyStandaloneConfig,
    ) -> PyResult<Bound<'py, PyAny>> {
        let config = standalone_config.inner.clone();
        crate::py_future::future_into_py(py, async move {
            let processor = Processor::new_standalone(PathBuf::from(peppy_config_path), &config)
                .map_err(|e| PyRuntimeError::new_err(e.to_string()))?;
            let runner = NodeRunner::new(processor)
                .await
                .map_err(|e| PyRuntimeError::new_err(e.to_string()))?;
            Ok(PyNodeRunner::new(Arc::new(runner)))
        })
    }

    /// Get the cancellation token for graceful shutdown coordination.
    fn cancellation_token(&self) -> PyCancellationToken {
        PyCancellationToken {
            inner: self.inner.cancellation_token().clone(),
        }
    }

    /// Register a cleanup callback to run when the node shuts down.
    ///
    /// The callback is called with no arguments after the node's cancellation
    /// token fires, on every stop path: `peppy node stop`, daemon teardown,
    /// SIGINT/SIGTERM (handled by the runtime), daemon-liveness loss, and a
    /// setup error. It may be a plain function or a coroutine function
    /// (`async def`); a returned awaitable is run to completion on the node's
    /// asyncio event loop. Messaging is still connected while hooks run, so
    /// cleanup can use the datastore and messenger. This is the guaranteed
    /// place for hardware teardown, lock release, and state flushing.
    ///
    /// Hooks run sequentially in reverse registration order, all bounded by
    /// one grace window (`peppy_config.lifecycle.shutdown_grace_secs`). The
    /// window is enforced at await points: a callback that blocks without
    /// awaiting cannot be interrupted, so keep synchronous work brief. The
    /// node's background tasks are cancelled only after every hook has
    /// finished. An exception raised by a hook is printed and the remaining
    /// hooks still run. Register hooks during setup; a hook registered after
    /// shutdown has begun may never run.
    fn on_shutdown(&self, callback: Py<PyAny>) {
        let event_loop_slot = Arc::clone(&self.event_loop_slot);
        self.inner
            .on_shutdown(run_python_shutdown_hook(callback, event_loop_slot));
    }

    /// Get the messenger handle for pub/sub and service communication.
    fn messenger(&self) -> PyMessengerHandle {
        self.cached_messenger.clone()
    }

    /// Get the core node this instance is bound to.
    fn bound_core_node(&self) -> &str {
        self.inner.processor().bound_core_node()
    }

    /// Get the instance ID this node is bound to.
    fn bound_instance_id(&self) -> &str {
        self.inner.processor().bound_instance_id()
    }

    /// Get the node name.
    fn node_name(&self) -> &str {
        self.inner.processor().node_name()
    }

    /// Get the node tag.
    fn node_tag(&self) -> &str {
        self.inner.processor().node_tag()
    }

    /// The [`ProducerRef`](peppylib::messaging::ProducerRef) pinned for
    /// `link_id`, when the consumer's slot resolves to a single producer
    /// ([`peppylib::messaging::ConsumerFilter::Pin`]); `None` for every
    /// other variant (multi-pin, wildcard-with-excludes, pure wildcard).
    /// Python codegen splices this at consumed subscribe / poll /
    /// send_goal call sites as the single `target` / `from_producer`
    /// argument; pinned slots therefore address their producer directly
    /// and skip discovery, while multi-bound and unbound `from_any` slots
    /// resolve to `None` and fall back to wildcard discovery. The same
    /// `ProducerRef` type is what consumed-topic callbacks return, so a
    /// received identity can be passed straight back here.
    ///
    /// Renamed from the pre-`ProducerRef` `pinned_target_for` (which
    /// returned the instance_id alone) so stale generated Python fails
    /// loudly with `AttributeError` instead of silently half-addressing.
    fn pinned_producer_for(&self, link_id: &str) -> Option<PyProducerRef> {
        self.inner
            .processor()
            .pinned_producer_for(link_id)
            .map(PyProducerRef::from)
    }
}

/// Python wrapper for StandaloneConfig.
#[pyclass(name = "StandaloneConfig", skip_from_py_object)]
#[derive(Clone)]
pub struct PyStandaloneConfig {
    inner: StandaloneConfig,
}

#[pymethods]
impl PyStandaloneConfig {
    #[new]
    fn new() -> Self {
        Self {
            inner: StandaloneConfig::new(),
        }
    }

    /// Set runtime parameters from a Python dict or dataclass instance.
    fn with_parameters(&self, py: Python<'_>, params: Py<PyAny>) -> PyResult<Self> {
        let params = params.bind(py);

        // If the input is a dataclass instance, convert it to a dict so
        // that depythonize (which requires the mapping protocol) can handle it.
        let dataclasses = py.import("dataclasses")?;
        let params = if dataclasses
            .call_method1("is_dataclass", (params,))?
            .is_truthy()?
            && !params.is_instance_of::<pyo3::types::PyType>()
        {
            dataclasses.call_method1("asdict", (params,))?
        } else {
            params.clone()
        };

        let value: serde_json::Value = depythonize(&params)?;
        Ok(Self {
            inner: self.inner.clone().with_parameters_json(value),
        })
    }

    /// Set both messaging host and port.
    fn with_messaging(&self, host: String, port: u16) -> Self {
        Self {
            inner: self.inner.clone().with_messaging(host, port),
        }
    }

    /// Set the instance ID.
    fn with_instance_id(&self, id: String) -> Self {
        Self {
            inner: self.inner.clone().with_instance_id(id),
        }
    }

    /// Set the node name override.
    fn with_node_name(&self, name: String) -> Self {
        Self {
            inner: self.inner.clone().with_node_name(name),
        }
    }
}

/// Python wrapper for NodeBuilder.
#[pyclass(name = "NodeBuilder")]
pub struct PyNodeBuilder {
    standalone_config: Option<StandaloneConfig>,
    config_path: Option<PathBuf>,
}

#[pymethods]
impl PyNodeBuilder {
    #[new]
    fn new() -> Self {
        Self {
            standalone_config: None,
            config_path: None,
        }
    }

    /// Configure standalone mode with custom settings.
    fn standalone(&self, config: &PyStandaloneConfig) -> Self {
        Self {
            standalone_config: Some(config.inner.clone()),
            config_path: self.config_path.clone(),
        }
    }

    /// Use a custom peppy.json5 path.
    fn with_config_path(&self, path: String) -> Self {
        Self {
            standalone_config: self.standalone_config.clone(),
            config_path: Some(PathBuf::from(path)),
        }
    }

    /// Run the node with a setup callback.
    ///
    /// The callback receives `(params: Parameters, node_runner: NodeRunner)` and
    /// may be either synchronous or async.  `params` is the generated
    /// `peppygen.parameters.Parameters` dataclass instance (hydrated from the
    /// runtime config dict).
    ///
    /// - **sync** `def setup(params: Parameters, node_runner: NodeRunner): ...` — runs directly.
    /// - **async** `async def setup(params: Parameters, node_runner: NodeRunner) -> list[asyncio.Task] | None: ...`
    ///   — runs on a persistent asyncio event loop. Return background tasks
    ///   created with `asyncio.create_task()` so the framework holds strong
    ///   references, preventing garbage collection.
    ///
    /// This method blocks until the node exits (shutdown or Ctrl+C).
    /// Must be called from a thread (not from the async event loop).
    fn run(&self, py: Python<'_>, setup_fn: Py<PyAny>) -> PyResult<()> {
        // Print a per-thread traceback if a fatal signal (e.g. a native
        // extension SIGSEGV on a background thread) kills the process.
        enable_faulthandler(py);

        let standalone_config = self.standalone_config.clone();
        let config_path = self.config_path.clone();
        let setup_error: SharedPyError = Arc::new(Mutex::new(None));
        let setup_error_for_run = Arc::clone(&setup_error);

        // Release the GIL while blocking so other Python threads can proceed
        py.detach(|| {
            let mut builder = NodeBuilder::<serde_json::Value>::new();

            if let Some(config) = standalone_config {
                builder = builder.standalone(config);
            }
            if let Some(path) = config_path {
                builder = builder.with_config_path(path);
            }

            // Hold the setup return value (e.g. a list of asyncio.Tasks) to
            // prevent garbage collection.  The outer Arc lives until
            // `builder.run()` returns (node shutdown), keeping a strong
            // reference to the Python object for the entire node lifetime.
            let setup_return_value: Arc<Mutex<Option<Py<PyAny>>>> = Arc::new(Mutex::new(None));
            let setup_return_for_run = Arc::clone(&setup_return_value);
            let event_loop_handle: Arc<Mutex<Option<EventLoopShutdown>>> =
                Arc::new(Mutex::new(None));
            let event_loop_for_run = Arc::clone(&event_loop_handle);
            // Shared with every PyNodeRunner (and so with every registered
            // shutdown hook); filled by start_async_setup when setup is async.
            let hook_loop_slot: SharedEventLoopSlot = Arc::new(Mutex::new(None));

            let run_result = builder.run(
                move |params: serde_json::Value, node_runner: Arc<NodeRunner>| {
                    let setup_error = Arc::clone(&setup_error_for_run);
                    let setup_return = setup_return_for_run;
                    let event_loop_slot = event_loop_for_run;
                    let hook_loop_slot = hook_loop_slot;
                    async move {
                        // Phase 1: call setup and start async event loop (holds GIL)
                        let async_handle =
                            Python::try_attach(|py| -> PyResult<Option<AsyncSetup>> {
                                let setup_result = call_setup_function(
                                    py,
                                    &setup_fn,
                                    &params,
                                    &node_runner,
                                    &hook_loop_slot,
                                )?;
                                let setup_bound = setup_result.bind(py);

                                if is_awaitable(setup_bound)? {
                                    Ok(Some(start_async_setup(
                                        py,
                                        setup_bound,
                                        &node_runner,
                                        &hook_loop_slot,
                                    )?))
                                } else {
                                    Ok(None)
                                }
                            });

                        match async_handle {
                            Some(Ok(Some(async_setup))) => {
                                // Held until this setup phase ends (any path);
                                // dropping it retires the shutdown monitor,
                                // handing the loop teardown to the drain hook.
                                let _monitor_disarm = async_setup.monitor_disarm;

                                // Store the loop teardown handle so the main
                                // thread can quiesce and join it after
                                // builder.run() returns.
                                if let Ok(mut guard) = event_loop_slot.lock() {
                                    *guard = Some(async_setup.event_loop_shutdown);
                                }

                                // Phase 2: await with the GIL released so the
                                // event loop thread can run the setup
                                // coroutine, and without blocking this tokio
                                // worker so the runner's select still observes
                                // shutdown requests and cancellation while
                                // setup is in flight.
                                async_setup
                                    .setup_complete_rx
                                    .await
                                    .map_err(|_| peppy_io_err("async setup channel closed"))?;

                                // Phase 3: check for exceptions and capture
                                // the return value (re-acquires GIL)
                                match Python::try_attach(|py| -> PyResult<()> {
                                    let result =
                                        async_setup.setup_future.bind(py).call_method0("result")?;
                                    // Store the return value to prevent GC of
                                    // returned tasks.
                                    if !result.is_none()
                                        && let Ok(mut guard) = setup_return.lock()
                                    {
                                        *guard = Some(result.unbind());
                                    }
                                    Ok(())
                                }) {
                                    Some(Ok(())) => Ok(()),
                                    Some(Err(err)) => {
                                        store_python_error(&setup_error, err);
                                        Err(peppy_io_err("async setup raised an exception"))
                                    }
                                    None => Err(peppy_io_err("failed to attach to Python GIL")),
                                }
                            }
                            Some(Ok(None)) => Ok(()),
                            Some(Err(err)) => {
                                store_python_error(&setup_error, err);
                                Err(peppy_io_err("setup callback raised an exception"))
                            }
                            None => Err(peppy_io_err("failed to attach to Python GIL")),
                        }
                    }
                },
            );

            // Quiesce the asyncio event loop before returning: stop the loop
            // (cancelling any straggler tasks first) and JOIN its thread.
            // Joining is what prevents the SIGSEGV; without it the daemon loop
            // thread can be killed while inside a native call (pycapnp
            // serialization, a pyo3 future) during interpreter finalization.
            // The drain hook and/or the shutdown monitor may have already
            // cancelled the tasks, but the stop trigger is idempotent. This is
            // the only place the loop is stopped, so it runs on every async-setup
            // exit path, including when a slow user hook exhausted the grace
            // window and the drain hook never completed.
            if let Some(shutdown) = event_loop_handle.lock().ok().and_then(|mut g| g.take()) {
                shutdown.quiesce();
            }

            // `setup_return_value` is dropped here after `builder.run()`
            // returns (node shutdown), releasing the Python reference.
            drop(setup_return_value);

            if let Some(err) = take_python_error(&setup_error) {
                return Err(err);
            }

            run_result.map_err(|e| {
                if let peppylib::PeppyError::NodeArgumentsValidation(
                    config::NodeArgumentsError::MissingParameters(ref params),
                ) = e
                {
                    return PyErr::new::<pyo3::exceptions::PyRuntimeError, _>(format!(
                        "missing required parameter(s) for standalone mode: {}. \
                         Provide them via StandaloneConfig().with_parameters()",
                        params.join(", ")
                    ));
                }
                PyErr::new::<pyo3::exceptions::PyRuntimeError, _>(e.to_string())
            })
        })
    }
}

/// Register the runtime submodule
pub(crate) fn register(parent_module: &Bound<'_, PyModule>) -> PyResult<()> {
    let runtime_module = PyModule::new(parent_module.py(), "runtime")?;
    runtime_module.add_class::<PyCancellationToken>()?;
    runtime_module.add_class::<PyNodeRunner>()?;
    runtime_module.add_class::<PyStandaloneConfig>()?;
    runtime_module.add_class::<PyNodeBuilder>()?;
    parent_module.add_submodule(&runtime_module)?;
    Ok(())
}
