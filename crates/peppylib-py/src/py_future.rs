//! Shutdown-safe replacement for `pyo3_async_runtimes::tokio::future_into_py`.
//!
//! `pyo3_async_runtimes` delivers every future result by attaching to the
//! interpreter from a thread of its global tokio runtime, using a plain
//! `Python::attach` with no shutdown guard. That runtime is never torn down,
//! so a delivery scheduled moments before `NodeBuilder.run()` returns races
//! interpreter finalization. A delivery thread that loses the race attaches
//! mid-finalization, and CPython 3.13 and older kill such a thread via
//! `pthread_exit`, which force-unwinds through Rust frames and crashes the
//! process (observed as generated nodes exiting with SIGSEGV at shutdown,
//! with no faulthandler output). The cancellation path is affected too:
//! upstream attaches even for futures whose Python side was already
//! cancelled, just to check `Future.cancelled()`.
//!
//! This module reimplements the conversion with the same semantics, but every
//! interpreter attach from runtime threads goes through a process-wide gate:
//!
//! - Deliveries acquire the gate in read mode and are skipped once it closes.
//! - The gate closes in write mode from a Python `atexit` handler registered
//!   at module import, which both waits for in-flight deliveries to finish
//!   and blocks later ones for good.
//!
//! CPython runs `atexit` callbacks at the start of finalization, while the
//! interpreter is still entirely intact and strictly before it sets the
//! finalizing flag that makes attaching fatal. Closing the gate there means
//! no runtime thread can attach once the dangerous regime begins, while
//! normal in-process use after a node stops (more runs, direct messenger
//! calls, test suites) is unaffected. Skipped deliveries are unobservable:
//! nothing can await their results once the interpreter is exiting.

use parking_lot::RwLock;
use pyo3::IntoPyObjectExt;
use pyo3::prelude::*;
use pyo3::types::{PyCFunction, PyDict};
use pyo3_async_runtimes::TaskLocals;
use pyo3_async_runtimes::err::RustPanic;
use std::future::Future;

/// Whether runtime threads may attach to the interpreter. Open until the
/// interpreter starts exiting; never reopens afterwards.
static ATTACH_GATE: RwLock<bool> = RwLock::new(true);

/// Register the `atexit` handler that closes the attach gate when the
/// interpreter starts exiting. Called once from module init; the GIL is
/// released while the write lock waits, so in-flight deliveries holding the
/// gate can finish their attach instead of deadlocking against this wait.
pub(crate) fn register_interpreter_exit_gate(py: Python<'_>) -> PyResult<()> {
    let close_gate = PyCFunction::new_closure(
        py,
        Some(c"_peppy_close_attach_gate"),
        Some(c"Block native result deliveries before interpreter finalization."),
        |args, _kwargs| {
            args.py().detach(|| {
                *ATTACH_GATE.write() = false;
            });
            Ok::<(), PyErr>(())
        },
    )?;
    py.import("atexit")?
        .call_method1("register", (close_gate,))?;
    Ok(())
}

/// Run `f` attached to the interpreter, unless the gate is closed or the
/// interpreter is already shutting down. Returns `None` when skipped. The
/// gate is held for the whole call, so the exit handler registered by
/// [`register_interpreter_exit_gate`] waits for it; `f` must therefore never
/// call back into this function, or it can deadlock against a pending gate
/// close.
pub(crate) fn try_attach_gated<F, R>(f: F) -> Option<R>
where
    F: for<'py> FnOnce(Python<'py>) -> R,
{
    let allowed = ATTACH_GATE.read();
    if !*allowed {
        return None;
    }
    Python::try_attach(f)
}

/// Convert a Rust future into an awaitable asyncio future, delivering its
/// result through the attach gate.
///
/// Mirrors `pyo3_async_runtimes::tokio::future_into_py`: the future runs on
/// the global tokio runtime under the caller's task locals, cancelling the
/// asyncio future cancels the Rust future, and a panic surfaces as a
/// `RustPanic` exception.
pub(crate) fn future_into_py<'py, F, T>(py: Python<'py>, fut: F) -> PyResult<Bound<'py, PyAny>>
where
    F: Future<Output = PyResult<T>> + Send + 'static,
    T: for<'p> IntoPyObject<'p> + Send + 'static,
{
    let locals = pyo3_async_runtimes::tokio::get_current_locals(py)?;
    let py_fut = locals
        .event_loop(py)
        .call_method0(pyo3::intern!(py, "create_future"))?;

    let (cancel_tx, mut cancel_rx) = tokio::sync::oneshot::channel::<()>();
    py_fut.call_method1(
        pyo3::intern!(py, "add_done_callback"),
        (CancelOnPyFutureDone {
            cancel_tx: Some(cancel_tx),
        },),
    )?;

    let py_fut_for_delivery: Py<PyAny> = py_fut.clone().unbind();
    let runtime = pyo3_async_runtimes::tokio::get_runtime();
    runtime.spawn(async move {
        // Inner spawn so a panic in `fut` is observed as a JoinError here
        // instead of taking down this delivery task.
        let task = pyo3_async_runtimes::tokio::get_runtime().spawn(
            pyo3_async_runtimes::tokio::scope(locals.clone(), async move {
                tokio::select! {
                    biased;
                    result = fut => Some(result),
                    // Only an explicit cancellation signal counts; a dropped
                    // sender means the asyncio future completed or was
                    // collected, and must not cancel the Rust future.
                    Ok(()) = &mut cancel_rx => None,
                }
            }),
        );

        let result = match task.await {
            // The asyncio future is already cancelled; there is nothing to
            // deliver, and skipping the attach entirely keeps cancellation
            // (the normal node-shutdown path) off the interpreter.
            Ok(None) => return,
            Ok(Some(result)) => result,
            Err(join_err) => {
                if !join_err.is_panic() {
                    // Aborted: the runtime is shutting down.
                    return;
                }
                Err(RustPanic::new_err(format!(
                    "rust future panicked: {}",
                    panic_message(join_err.into_panic().as_ref())
                )))
            }
        };

        // Deliver from the blocking pool: attaching can block on the GIL and
        // must not stall a runtime worker.
        pyo3_async_runtimes::tokio::get_runtime().spawn_blocking(move || {
            try_attach_gated(|py| deliver_to_python(py, &locals, &py_fut_for_delivery, result));
        });
    });

    Ok(py_fut)
}

/// Resolve the asyncio future with `result`, scheduling the completion on its
/// event loop. Failures are printed rather than raised: this runs on a
/// runtime thread with no caller to propagate to.
fn deliver_to_python<T>(
    py: Python<'_>,
    locals: &TaskLocals,
    py_fut: &Py<PyAny>,
    result: PyResult<T>,
) where
    T: for<'p> IntoPyObject<'p>,
{
    let future = py_fut.bind(py);
    match cancelled(future) {
        Ok(false) => {}
        Ok(true) => return,
        Err(err) => {
            err.print_and_set_sys_last_vars(py);
            return;
        }
    }

    let scheduled = (|| -> PyResult<()> {
        let (complete, value) = match result.and_then(|value| value.into_bound_py_any(py)) {
            Ok(value) => (future.getattr(pyo3::intern!(py, "set_result"))?, value),
            Err(err) => (
                future.getattr(pyo3::intern!(py, "set_exception"))?,
                err.into_bound_py_any(py)?,
            ),
        };
        let kwargs = PyDict::new(py);
        kwargs.set_item(pyo3::intern!(py, "context"), py.None())?;
        locals.event_loop(py).call_method(
            pyo3::intern!(py, "call_soon_threadsafe"),
            (CheckedCompletor, future, complete, value),
            Some(&kwargs),
        )?;
        Ok(())
    })();
    if let Err(err) = scheduled {
        err.print_and_set_sys_last_vars(py);
    }
}

fn cancelled(future: &Bound<'_, PyAny>) -> PyResult<bool> {
    future
        .getattr(pyo3::intern!(future.py(), "cancelled"))?
        .call0()?
        .is_truthy()
}

fn panic_message(payload: &(dyn std::any::Any + Send)) -> &str {
    if let Some(message) = payload.downcast_ref::<&str>() {
        message
    } else if let Some(message) = payload.downcast_ref::<String>() {
        message
    } else {
        "unknown error"
    }
}

/// Done callback on the asyncio future that forwards a cancellation to the
/// Rust future. Runs on the event loop thread, so it never races
/// interpreter finalization.
#[pyclass]
struct CancelOnPyFutureDone {
    cancel_tx: Option<tokio::sync::oneshot::Sender<()>>,
}

#[pymethods]
impl CancelOnPyFutureDone {
    fn __call__(&mut self, future: &Bound<'_, PyAny>) -> PyResult<()> {
        if cancelled(future)?
            && let Some(cancel_tx) = self.cancel_tx.take()
        {
            let _ = cancel_tx.send(());
        }
        Ok(())
    }
}

/// Loop-side completion step: re-checks cancellation right before resolving
/// the future, because a cancellation can land between the delivery thread's
/// check and the scheduled callback running on the loop.
#[pyclass]
struct CheckedCompletor;

#[pymethods]
impl CheckedCompletor {
    fn __call__(
        &self,
        future: &Bound<'_, PyAny>,
        complete: &Bound<'_, PyAny>,
        value: &Bound<'_, PyAny>,
    ) -> PyResult<()> {
        if cancelled(future)? {
            return Ok(());
        }
        complete.call1((value,))?;
        Ok(())
    }
}
