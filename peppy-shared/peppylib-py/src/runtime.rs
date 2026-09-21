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
    /// Pure-Python callable that attaches a failure watcher to every task
    /// returned by setup (marshalled onto the loop thread). A returned task
    /// that dies with an exception records the error (so `run` re-raises
    /// it) and cancels the node; see `build_async_setup`.
    task_failure_attach: Py<PyAny>,
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

def make_task_failure_attacher(event_loop, report_failure):
    def _on_task_done(task):
        if task.cancelled():
            return
        exc = task.exception()
        if exc is None:
            return
        try:
            msg = ''.join(traceback.format_exception(exc))
            print(f'Node background task failed:\\n{msg}', file=sys.stderr, flush=True)
        except BaseException as fmt_err:
            print(f'Node background task failed: {fmt_err}', file=sys.stderr, flush=True)
        report_failure(exc)

    def _attach_on_loop(tasks):
        for task in tasks:
            task.add_done_callback(_on_task_done)

    def _attach(setup_result):
        # The setup return value is documented as a list of asyncio.Tasks,
        # but any object is legal: accept a single Task/Future or any
        # iterable of them; ignore everything else.
        if hasattr(setup_result, 'add_done_callback'):
            tasks = [setup_result]
        else:
            try:
                items = list(setup_result)
            except TypeError:
                return
            tasks = [t for t in items if hasattr(t, 'add_done_callback')]
        if not tasks:
            return
        # Called from the runner thread. add_done_callback is not
        # thread-safe, and on an already-done task it degrades to a bare
        # call_soon, which from a foreign thread never wakes an idle loop.
        # Marshalling the attach onto the loop thread avoids both.
        try:
            event_loop.call_soon_threadsafe(_attach_on_loop, tasks)
        except RuntimeError:
            pass  # loop already closed: the node is shutting down anyway
    return _attach

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
/// node via the event loop's exception handler; tasks *returned* by setup are
/// watched directly (their exception is recorded in `setup_error` and
/// re-raised out of `run`), because holding them for the node's lifetime
/// keeps the GC-time unretrieved-exception path from ever firing.
fn start_async_setup(
    py: Python<'_>,
    setup_awaitable: &Bound<'_, PyAny>,
    node_runner: &Arc<NodeRunner>,
    event_loop_slot: &SharedEventLoopSlot,
    setup_error: &SharedPyError,
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
        setup_error,
    )
    .inspect_err(|_| abort_loop_thread(&event_loop, &thread))
}

/// Steps 6-9 of [`start_async_setup`], split out so any failure can be caught by
/// the caller and turned into a stop+join of the already-running loop thread.
/// Builds the loop teardown, registers the loop-drain shutdown hook, submits the
/// setup coroutine, and spawns the setup-scoped shutdown monitor.
#[allow(clippy::too_many_arguments)]
fn build_async_setup(
    py: Python<'_>,
    helpers: &Bound<'_, PyModule>,
    event_loop: &Bound<'_, PyAny>,
    asyncio: &Bound<'_, PyModule>,
    node_runner: &Arc<NodeRunner>,
    setup_awaitable: &Bound<'_, PyAny>,
    thread: &Bound<'_, PyAny>,
    setup_error: &SharedPyError,
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

    // 10. Build the failure attacher that Phase 3 of `PyNodeBuilder::run`
    //     invokes with the setup return value. The returned tasks are held
    //     for the node's lifetime (to keep them alive), so asyncio's GC-time
    //     "Task exception was never retrieved" report — and with it the loop
    //     exception handler that would cancel the node — can never fire for
    //     them. Without this watcher, a returned task that dies leaves a
    //     half-alive node that still answers health probes. On a
    //     non-cancelled exception the watcher records the error (re-raised
    //     out of `run`, so the process exits non-zero and the daemon records
    //     a terminal `Failed` instance) and cancels the node. The attach and
    //     watcher bodies are pure Python (see `create_event_loop_helpers`);
    //     only this trivial recording leaf is a PyCFunction, mirroring
    //     `notify_on_future_done`.
    let error_slot = Arc::clone(setup_error);
    let cancel_on_failure = node_runner.cancellation_token().clone();
    let report_failure = PyCFunction::new_closure(
        py,
        Some(c"_peppy_report_task_failure"),
        None,
        move |args, _kwargs| {
            if let Ok(exc) = args.get_item(0) {
                store_python_error(&error_slot, PyErr::from_value(exc));
            }
            cancel_on_failure.cancel();
            Ok::<(), PyErr>(())
        },
    )?;
    let task_failure_attach = helpers
        .getattr("make_task_failure_attacher")?
        .call1((event_loop, report_failure))?
        .unbind();

    Ok(AsyncSetup {
        setup_complete_rx,
        setup_future: future_ref,
        event_loop_shutdown: EventLoopShutdown {
            stop_trigger: stop_trigger.unbind(),
            thread: thread.clone().unbind(),
        },
        monitor_disarm: disarm_tx,
        task_failure_attach,
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

impl PyCancellationToken {
    /// Clone of the wrapped token, for bindings that thread it into
    /// runtime primitives (e.g. the bound-set subscription's shutdown
    /// signal).
    pub(crate) fn inner_token(&self) -> CancellationToken {
        self.inner.clone()
    }
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

    /// The cooperative-shutdown grace window, in seconds, that bounds the
    /// registered shutdown hooks (daemon-resolved, or the built-in default in
    /// standalone mode). The value a lifecycle-owning caller should pass to
    /// [`run_shutdown_hooks`](Self::run_shutdown_hooks).
    fn shutdown_grace_secs(&self) -> f64 {
        self.inner.processor().shutdown_grace().as_secs_f64()
    }

    /// Run the registered shutdown hooks, bounded collectively by
    /// `grace_secs`. `NodeBuilder().run(...)` does this itself on every exit
    /// path; a caller that owns the runner's lifecycle directly (the
    /// `peppylib.testing` harness over
    /// [`new_standalone`](Self::new_standalone)) is responsible for this
    /// convergence step. Takes the hooks out of the registry, so a second
    /// call is harmless.
    fn run_shutdown_hooks<'py>(
        &self,
        py: Python<'py>,
        grace_secs: f64,
    ) -> PyResult<Bound<'py, PyAny>> {
        let grace = std::time::Duration::try_from_secs_f64(grace_secs).map_err(|e| {
            pyo3::exceptions::PyValueError::new_err(format!("invalid grace_secs: {e}"))
        })?;
        // Under `NodeBuilder().run(...)` the hook loop slot was published when
        // the runtime started the node's own loop. A lifecycle-owning caller
        // (the `peppylib.testing` harness over `new_standalone`) drove `setup`
        // on its own loop instead and the slot is still empty; publish the
        // calling loop so hook coroutines run where setup's tasks and futures
        // live, not on a one-off `asyncio.run` loop.
        if let Ok(mut slot) = self.event_loop_slot.lock()
            && slot.is_none()
            && let Ok(asyncio) = py.import("asyncio")
            && let Ok(running) = asyncio.call_method0("get_running_loop")
        {
            *slot = Some(running.unbind());
        }
        let runner = Arc::clone(&self.inner);
        crate::py_future::future_into_py(py, async move {
            runner.run_shutdown_hooks(grace).await;
            Ok(())
        })
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

    /// The runtime-resolved, immutable, ordered producer set bound to the
    /// consumer slot at `link_id`, in application declaration order. Its
    /// validated size is the slot's declared cardinality (exactly one for
    /// `one`, at most one for `zero_or_one`, at least one for `one_or_more`,
    /// any size for `zero_or_more`); node startup rejects anything else, so
    /// this cannot fail for a generated `link_id`: a miss means the generated
    /// code and the manifest disagree (stale codegen), and panics.
    /// Python codegen splices this behind the generated `bound_producers()`
    /// module functions of `one_or_more` and `zero_or_more` slots; the scalar
    /// cardinalities use the singular
    /// [`bound_producer`](Self::bound_producer) /
    /// [`optional_bound_producer`](Self::optional_bound_producer).
    fn bound_producers(&self, link_id: &str) -> Vec<PyProducerRef> {
        self.inner
            .processor()
            .bound_producers(link_id)
            .iter()
            .map(|producer| PyProducerRef::from(producer.clone()))
            .collect()
    }

    /// The sole producer bound to the `cardinality: "one"` consumer slot
    /// at `link_id`. Node startup validated the slot's set size, so
    /// exactly one member exists; a miss or any other size means the
    /// generated code and the manifest disagree (stale codegen) and
    /// panics, exactly like [`bound_producers`](Self::bound_producers) on
    /// an unknown `link_id`. Python codegen splices this behind the
    /// generated `bound_producer()` module function of every `one` slot.
    fn bound_producer(&self, link_id: &str) -> PyProducerRef {
        PyProducerRef::from(self.inner.processor().sole_bound_producer(link_id).clone())
    }

    /// The producer bound to the `cardinality: "zero_or_one"` consumer slot
    /// at `link_id`, or `None` where the deployment wrote the slot vacant.
    /// Node startup validated the slot's set as holding at most one member,
    /// so a larger one means the generated code and the manifest disagree
    /// (stale codegen) and panics, exactly like
    /// [`bound_producers`](Self::bound_producers) on an unknown `link_id`.
    /// Python codegen splices this behind the generated `bound_producer()`
    /// module function of every `zero_or_one` slot.
    fn optional_bound_producer(&self, link_id: &str) -> Option<PyProducerRef> {
        self.inner
            .processor()
            .optional_bound_producer(link_id)
            .cloned()
            .map(PyProducerRef::from)
    }

    /// Checks that `target` is a member of the bound set of the slot at
    /// `link_id`, raising the membership error otherwise. Generated
    /// consumed service / action wrappers call this before anything
    /// reaches the wire; membership is per slot, so a producer bound to a
    /// different slot of the same consumer is rejected all the same.
    fn ensure_target_bound(&self, link_id: &str, target: &PyProducerRef) -> PyResult<()> {
        self.inner
            .processor()
            .ensure_target_bound(link_id, target.as_inner())
            .map_err(crate::messaging::to_py_err)
    }

    /// Handle onto the scalar pairing slot declared at `link_id` in
    /// `depends_on.pairings` (a `one` or `zero_or_one` slot):
    /// `peer(link_id).paired()` returns the current peer's identity (or
    /// `None` while unpaired) and `wait_paired()` awaits one. Raises
    /// `ValueError` if the manifest declares no such slot, and panics if it
    /// declares one with a multi-peer cardinality, which is read through
    /// `peer_set`.
    fn peer(&self, link_id: &str) -> PyResult<crate::messaging::PyPeerSlot> {
        self.inner
            .peer(link_id)
            .map(|slot| crate::messaging::PyPeerSlot { inner: slot })
            .map_err(crate::messaging::to_py_err)
    }

    /// Handle onto the multi-peer pairing slot declared at `link_id` in
    /// `depends_on.pairings` (a `one_or_more` or `zero_or_more` slot):
    /// `peer_set(link_id).peers()` returns every peer it currently holds, in
    /// establishment order. Raises `ValueError` if the manifest declares no
    /// such slot, and panics if it declares one with a scalar cardinality,
    /// which is read through `peer`.
    fn peer_set(&self, link_id: &str) -> PyResult<crate::messaging::PyPeerSlotSet> {
        self.inner
            .peer_set(link_id)
            .map(|set| crate::messaging::PyPeerSlotSet { inner: set })
            .map_err(crate::messaging::to_py_err)
    }

    /// The copy this instance belongs to, as the launch composed it; `None`
    /// for an instance run outside a copy.
    fn copy(&self) -> Option<&str> {
        self.inner.copy()
    }

    /// Announces the socket the node bound for the endpoint the manifest
    /// declares as `label` under `execution.endpoints`: `scheme` is the URI
    /// scheme token it serves (`http`, `https`), `host` and `port` the
    /// address the listener reports back, `path` `""` or a path starting
    /// with `/`. Called inside `setup`, after the bind; the daemon turns the
    /// binding into the URLs an operator opens.
    ///
    /// Raises `ValueError` for a label the manifest does not declare, for a
    /// second announcement of one label, once setup has returned, for a host
    /// that is not an IP literal, and for a scheme or path that breaks the
    /// rules above.
    #[pyo3(signature = (label, scheme, host, port, path = String::new()))]
    fn announce_endpoint(
        &self,
        label: &str,
        scheme: String,
        host: &str,
        port: u16,
        path: String,
    ) -> PyResult<()> {
        let ip: std::net::IpAddr = host.parse().map_err(|_| {
            // The same variant the Rust binding refuses a malformed scheme or
            // path with, so both languages word an invalid binding alike.
            crate::messaging::to_py_err(peppylib::PeppyError::InvalidEndpointBinding {
                label: label.to_string(),
                reason: format!(
                    "host `{host}` is not an IP literal (announce the address the listener \
                     bound, e.g. `0.0.0.0` or `127.0.0.1`)"
                ),
            })
        })?;
        self.inner
            .announce_endpoint(
                label,
                peppylib::runtime::EndpointBinding {
                    scheme,
                    address: std::net::SocketAddr::new(ip, port),
                    path,
                },
            )
            .map_err(crate::messaging::to_py_err)
    }

    /// Ends the announcement window and checks the set against the
    /// manifest: every declared label must have been announced.
    /// `NodeBuilder().run(...)` does this itself when `setup` returns; a
    /// caller that owns the runner's lifecycle directly (the
    /// `peppylib.testing` harness over
    /// [`new_standalone`](Self::new_standalone)) does it once its setup
    /// returns. Raises `ValueError` naming a declared label that was not
    /// announced.
    fn seal_endpoints(&self) -> PyResult<()> {
        self.inner
            .seal_endpoints()
            .map(drop)
            .map_err(crate::messaging::to_py_err)
    }

    /// The endpoints announced so far, in label order, each as
    /// `(label, scheme, host, port, path)`.
    fn announced_endpoints(&self) -> Vec<(String, String, String, u16, String)> {
        self.inner
            .announced_endpoints()
            .into_iter()
            .map(|endpoint| {
                (
                    endpoint.label,
                    endpoint.binding.scheme,
                    endpoint.binding.address.ip().to_string(),
                    endpoint.binding.address.port(),
                    endpoint.binding.path,
                )
            })
            .collect()
    }

    /// Declares the publisher for one topic of the scalar pairing slot at
    /// `link_id`: a `TopicPublisher` whose `publish` reaches the one paired
    /// peer and is a no-op while unpaired. Spliced by the generated
    /// `peppygen.paired_topics.<link_id>.<topic>.declare_publisher` call
    /// sites of scalar slots; `pairing_name` / `pairing_tag` / `topic` come
    /// from the pairing doc via codegen constants.
    fn declare_sole_peer_publisher<'py>(
        &self,
        py: Python<'py>,
        link_id: String,
        pairing_name: String,
        pairing_tag: String,
        topic: String,
        qos: crate::config::PyQoSProfile,
    ) -> PyResult<Bound<'py, PyAny>> {
        let node_runner = Arc::clone(&self.inner);
        crate::py_future::future_into_py(py, async move {
            let publisher = peppylib::runtime::declare_sole_peer_publisher(
                &node_runner,
                &link_id,
                &pairing_name,
                &pairing_tag,
                &topic,
                qos.into(),
            )
            .await
            .map_err(crate::messaging::to_py_err)?;
            Ok(crate::messaging::PyTopicPublisher { inner: publisher })
        })
    }

    /// Declares the publisher for one topic of the multi pairing slot at
    /// `link_id`. Spliced by the generated
    /// `peppygen.paired_topics.<link_id>.<topic>.declare_publisher` call
    /// sites of multi slots; `pairing_name` / `pairing_tag` / `topic` come
    /// from the pairing doc via codegen constants.
    fn declare_peer_publisher(
        &self,
        link_id: &str,
        pairing_name: &str,
        pairing_tag: &str,
        topic: &str,
        qos: crate::config::PyQoSProfile,
    ) -> PyResult<crate::messaging::PyPeerPublisher> {
        peppylib::runtime::declare_peer_publisher(
            &self.inner,
            link_id,
            pairing_name,
            pairing_tag,
            topic,
            qos.into(),
        )
        .map(|publisher| crate::messaging::PyPeerPublisher {
            inner: Arc::new(publisher),
        })
        .map_err(crate::messaging::to_py_err)
    }

    /// Subscribe to one peer-emitted topic of the pairing slot at `link_id`.
    /// Spliced by the generated `peppygen.paired_topics.<link_id>.<topic>.subscribe`
    /// call sites; `pairing_name` / `pairing_tag` / `topic` come from the
    /// pairing doc via codegen constants. The subscription yields
    /// `(peer, message)` tuples, nothing while the slot is unpaired, and
    /// follows the slot's live pin.
    fn subscribe_peer<'py>(
        &self,
        py: Python<'py>,
        link_id: String,
        pairing_name: String,
        pairing_tag: String,
        topic: String,
        qos: crate::config::PyQoSProfile,
    ) -> PyResult<Bound<'py, PyAny>> {
        let node_runner = Arc::clone(&self.inner);
        crate::py_future::future_into_py(py, async move {
            let subscription = peppylib::runtime::subscribe_peer(
                &node_runner,
                &link_id,
                &pairing_name,
                &pairing_tag,
                &topic,
                qos.into(),
            )
            .await
            .map_err(crate::messaging::to_py_err)?;
            Ok(crate::messaging::PyPeerSubscription {
                inner: Arc::new(tokio::sync::Mutex::new(subscription)),
            })
        })
    }

    /// Handle onto the scalar observer slot declared at `link_id` in
    /// `depends_on.pairing_observers` (a `one` or `zero_or_one` slot), read
    /// through the accessor its cardinality declares:
    /// `observation_slot(link_id).sole_source()` for a `one` slot, which always
    /// observes a pairing, and `observation_slot(link_id).source()` for a
    /// `zero_or_one` slot, which answers `None` where the deployment wrote it
    /// vacant. Raises `ValueError` if the manifest declares no such observer
    /// slot, and panics if it declares one with a multi-member cardinality,
    /// which is read through `observation_slot_set`.
    fn observation_slot(&self, link_id: &str) -> PyResult<crate::messaging::PyObservationSlot> {
        self.inner
            .observation_slot(link_id)
            .map(|slot| crate::messaging::PyObservationSlot { inner: slot })
            .map_err(crate::messaging::to_py_err)
    }

    /// Handle onto the multi-member observer slot declared at `link_id` in
    /// `depends_on.pairing_observers` (a `one_or_more` or `zero_or_more` slot):
    /// `observation_slot_set(link_id).sources()` returns every pairing it
    /// currently observes, in plan order. Raises `ValueError` if the manifest
    /// declares no such observer slot, and panics if it declares one with a
    /// scalar cardinality, which is read through `observation_slot`.
    fn observation_slot_set(
        &self,
        link_id: &str,
    ) -> PyResult<crate::messaging::PyObservationSlotSet> {
        self.inner
            .observation_slot_set(link_id)
            .map(|slot| crate::messaging::PyObservationSlotSet { inner: slot })
            .map_err(crate::messaging::to_py_err)
    }

    /// Subscribe to one topic emitted by an observer slot's sources, for every
    /// cardinality. Spliced by the generated
    /// `peppygen.paired_topics.<link_id>.<topic>.subscribe` call sites of
    /// observer modules; `pairing_name` / `pairing_tag` / `topic` come from the
    /// pairing doc via codegen constants. Messages from every member of the slot
    /// fan into one stream, each yielded as a `(source, message)` tuple whose
    /// `ObservedSource` tag tells members apart even when they share one
    /// instance, and delivery follows each source instance's lifecycle.
    fn subscribe_observed<'py>(
        &self,
        py: Python<'py>,
        link_id: String,
        pairing_name: String,
        pairing_tag: String,
        topic: String,
        qos: crate::config::PyQoSProfile,
    ) -> PyResult<Bound<'py, PyAny>> {
        let node_runner = Arc::clone(&self.inner);
        crate::py_future::future_into_py(py, async move {
            let subscription = peppylib::runtime::subscribe_observed(
                &node_runner,
                &link_id,
                &pairing_name,
                &pairing_tag,
                &topic,
                qos.into(),
            )
            .await
            .map_err(crate::messaging::to_py_err)?;
            Ok(crate::messaging::PyObservedSubscription {
                inner: Arc::new(tokio::sync::Mutex::new(subscription)),
            })
        })
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

    /// Bind the node to `clock`, the standalone spelling of a launcher's
    /// `framework: { clock: "<domain>" }` and of the publisher a domain
    /// declaration names. Wall time is the default.
    fn with_clock(&self, clock: &crate::clock::PyClockBinding) -> Self {
        Self {
            inner: self.inner.clone().with_clock(clock.inner.clone()),
        }
    }

    /// Pre-pair the pairing slot at `link_id` to the peer at
    /// `(peer_core_node, peer_instance_id)` whose complementary slot is
    /// `peer_link_id`. Repeat calls with the same `link_id` accumulate in
    /// call order, mirroring how repeated `--link KEY@instance` flags fill a
    /// multi slot; a scalar slot takes at most one, and a second one fails
    /// startup. Standalone-mode stand-in for the daemon's pair delivery;
    /// ignored (with a warning) if the manifest declares no such slot.
    fn with_peer_pin(
        &self,
        link_id: String,
        peer_core_node: String,
        peer_instance_id: String,
        peer_link_id: String,
    ) -> Self {
        Self {
            inner: self.inner.clone().with_peer_pin(
                link_id,
                peer_core_node,
                peer_instance_id,
                peer_link_id,
            ),
        }
    }

    /// `with_peer_pin` for a peer that belongs to the copy named `copy`, as
    /// a `stack join` stamps it: the member the node reads for this pair
    /// carries that copy name.
    fn with_peer_pin_in_copy(
        &self,
        link_id: String,
        peer_core_node: String,
        peer_instance_id: String,
        peer_link_id: String,
        copy: String,
    ) -> Self {
        Self {
            inner: self.inner.clone().with_peer_pin_in_copy(
                link_id,
                peer_core_node,
                peer_instance_id,
                peer_link_id,
                copy,
            ),
        }
    }

    /// Append the producer at `(producer_core_node, producer_instance_id)` to
    /// the bound set of the consumer slot at `link_id`. Repeat calls with the
    /// same `link_id` accumulate in call order, mirroring how repeated
    /// `--bind KEY@instance` flags accumulate: the resulting set must satisfy
    /// the slot's declared cardinality at startup (exactly one for `one`, at
    /// most one for `zero_or_one`, at least one for `one_or_more`, any size for
    /// `zero_or_more`; a `zero_or_more` slot may be left unseeded for an empty
    /// set, and a `zero_or_one` slot that runs empty says so with
    /// `with_vacant_producer_slot`). Standalone-mode stand-in for the
    /// launcher's validated binding map; ignored (with a warning) if the
    /// manifest declares no such slot.
    fn with_bound_producer(
        &self,
        link_id: String,
        producer_core_node: String,
        producer_instance_id: String,
    ) -> Self {
        Self {
            inner: self.inner.clone().with_bound_producer(
                link_id,
                producer_core_node,
                producer_instance_id,
            ),
        }
    }

    /// Seed the consumer slot at `link_id` with an explicit empty producer set,
    /// the standalone spelling of a deployment's
    /// `links: { <link_id>: { vacant: "<why>" } }`. Only a `zero_or_one` slot
    /// accepts it: startup admits an empty set there and rejects one on a `one`
    /// / `one_or_more` slot, exactly as it does for a daemon boot config. The
    /// reason a deployment writes down has no standalone counterpart, because
    /// there is no deployment to write it.
    fn with_vacant_producer_slot(&self, link_id: String) -> Self {
        Self {
            inner: self.inner.clone().with_vacant_producer_slot(link_id),
        }
    }

    /// Append the pairing at `(source_core_node, source_instance_id)`'s
    /// `source_link_id` slot to the member set the observer slot at `link_id`
    /// observes. Repeat calls with the same `link_id` accumulate in call order,
    /// mirroring how a launcher's `links` array orders a slot's members: the
    /// resulting set must satisfy the slot's declared cardinality at startup. A
    /// `zero_or_one` or `zero_or_more` slot left unseeded observes nothing,
    /// which those cardinalities admit; the two floored ones have no such state
    /// and so must be seeded. Standalone-mode stand-in for the member set the
    /// daemon stamps at spawn; ignored (with a warning) if the manifest
    /// declares no such observer slot.
    fn with_observed_source(
        &self,
        link_id: String,
        source_core_node: String,
        source_instance_id: String,
        source_link_id: String,
    ) -> Self {
        Self {
            inner: self.inner.clone().with_observed_source(
                link_id,
                source_core_node,
                source_instance_id,
                source_link_id,
            ),
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
                                        &setup_error,
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
                                    if !result.is_none() {
                                        // Watch every returned task: holding
                                        // them below keeps the GC-time
                                        // unretrieved-exception report from
                                        // ever firing, so this is the only
                                        // path that observes their failure.
                                        async_setup
                                            .task_failure_attach
                                            .bind(py)
                                            .call1((&result,))?;
                                        // Store the return value to prevent GC
                                        // of returned tasks.
                                        if let Ok(mut guard) = setup_return.lock() {
                                            *guard = Some(result.unbind());
                                        }
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
