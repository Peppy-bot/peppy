use crate::messaging::PyMessengerHandle;
use peppylib::runtime::CancellationToken;
use peppylib::runtime::{NodeBuilder, NodeRunner, Processor, StandaloneConfig};
use pyo3::exceptions::PyRuntimeError;
use pyo3::prelude::*;
use pyo3::types::{PyCFunction, PyDict};
use pythonize::{depythonize, pythonize};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

type SharedPyError = Arc<Mutex<Option<PyErr>>>;
type SharedEventLoop = Arc<Mutex<Option<Py<PyAny>>>>;
type AsyncSetupResult = (std::sync::mpsc::Receiver<()>, Py<PyAny>, SharedEventLoop);

fn peppy_io_err(message: impl Into<String>) -> peppylib::PeppyError {
    peppylib::PeppyError::Io(std::io::Error::other(message.into()))
}

fn call_setup_function(
    py: Python<'_>,
    setup_fn: &Py<PyAny>,
    params: &serde_json::Value,
    node_runner: &Arc<NodeRunner>,
) -> PyResult<Py<PyAny>> {
    let py_params = pythonize(py, params)
        .map_err(|e| PyRuntimeError::new_err(format!("failed to convert params to Python: {e}")))?
        .unbind();
    let py_params = hydrate_parameters(py, py_params)?;
    let py_runner = Py::new(py, PyNodeRunner::new(Arc::clone(node_runner))).map_err(|e| {
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
) -> PyResult<AsyncSetupResult> {
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

    // 5. Submit the setup coroutine and register a done callback.
    //    A Rust channel signals completion so the caller can release the GIL
    //    before blocking — the event loop thread needs it to run the coroutine.
    let future =
        asyncio.call_method1("run_coroutine_threadsafe", (setup_awaitable, &event_loop))?;
    let (tx, rx) = std::sync::mpsc::sync_channel::<()>(1);
    let done_cb = PyCFunction::new_closure(
        py,
        Some(c"_peppy_setup_done"),
        None,
        move |_args, _kwargs| {
            let _ = tx.send(());
            Ok::<(), PyErr>(())
        },
    )?;
    future.call_method1("add_done_callback", (done_cb,))?;
    let future_ref = future.unbind();

    // 6. Schedule shutdown monitor: stop the event loop when the node shuts down.
    //    The event loop reference is shared so the main thread can also stop it
    //    after builder.run() returns, preventing a race where the process exits
    //    before the shutdown monitor thread has finished.
    let shared_loop: SharedEventLoop = Arc::new(Mutex::new(Some(event_loop.unbind())));
    let loop_for_shutdown = Arc::clone(&shared_loop);
    let cancel_for_shutdown = node_runner.cancellation_token().clone();
    let rt_handle = tokio::runtime::Handle::current();
    std::thread::Builder::new()
        .name("peppy-asyncio-shutdown".to_string())
        .spawn(move || {
            rt_handle.block_on(cancel_for_shutdown.cancelled());
            let loop_ref = loop_for_shutdown.lock().ok().and_then(|mut g| g.take());
            if let Some(event_loop) = loop_ref {
                let _ = Python::try_attach(|py| -> PyResult<()> {
                    let loop_ = event_loop.bind(py);
                    let stop = loop_.getattr("stop")?;
                    loop_.call_method1("call_soon_threadsafe", (stop,))?;
                    Ok(())
                });
            }
        })
        .map_err(|e| PyRuntimeError::new_err(format!("failed to start shutdown monitor: {e}")))?;

    Ok((rx, future_ref, shared_loop))
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
}

/// Python wrapper for NodeRunner.
#[pyclass(name = "NodeRunner")]
pub struct PyNodeRunner {
    pub(crate) inner: Arc<NodeRunner>,
    /// Cached messenger handle — cloning `MessengerHandle` is a cheap `Arc`
    /// bump, but we avoid re-wrapping it on every `messenger()` call.
    cached_messenger: PyMessengerHandle,
}

impl PyNodeRunner {
    fn new(node_runner: Arc<NodeRunner>) -> Self {
        let cached_messenger = PyMessengerHandle {
            inner: node_runner.messenger().clone(),
        };
        Self {
            inner: node_runner,
            cached_messenger,
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
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
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

    /// Producer `instance_id` pinned for this consumer's `link_id`, if
    /// any. Returned by the launcher's `bindings: {}` map or the CLI's
    /// `--bind` flags. `None` means the consumer wildcards
    /// `from_instance_id` on the wire and accepts any producer of the
    /// right `(name, tag)`.
    fn binding_for(&self, link_id: &str) -> Option<&str> {
        self.inner.processor().binding_for(link_id)
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
            let event_loop_handle: Arc<Mutex<Option<SharedEventLoop>>> = Arc::new(Mutex::new(None));
            let event_loop_for_run = Arc::clone(&event_loop_handle);

            let run_result = builder.run(
                move |params: serde_json::Value, node_runner: Arc<NodeRunner>| {
                    let setup_error = Arc::clone(&setup_error_for_run);
                    let setup_return = setup_return_for_run;
                    let event_loop_slot = event_loop_for_run;
                    async move {
                        // Phase 1: call setup and start async event loop (holds GIL)
                        let async_handle =
                            Python::try_attach(|py| -> PyResult<Option<AsyncSetupResult>> {
                                let setup_result =
                                    call_setup_function(py, &setup_fn, &params, &node_runner)?;
                                let setup_bound = setup_result.bind(py);

                                if is_awaitable(setup_bound)? {
                                    Ok(Some(start_async_setup(py, setup_bound, &node_runner)?))
                                } else {
                                    Ok(None)
                                }
                            });

                        match async_handle {
                            Some(Ok(Some((rx, future_ref, shared_loop)))) => {
                                // Store event loop handle so the main thread
                                // can stop it after builder.run() returns.
                                if let Ok(mut guard) = event_loop_slot.lock() {
                                    *guard = Some(shared_loop);
                                }

                                // Phase 2: wait without GIL so event loop
                                // thread can run the setup coroutine
                                rx.recv()
                                    .map_err(|_| peppy_io_err("async setup channel closed"))?;

                                // Phase 3: check for exceptions and capture
                                // the return value (re-acquires GIL)
                                match Python::try_attach(|py| -> PyResult<()> {
                                    let result = future_ref.bind(py).call_method0("result")?;
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

            // Stop the event loop if the shutdown monitor thread hasn't
            // already. This prevents a race where the process exits before
            // the background thread finishes stopping the loop, which can
            // cause a SIGSEGV when the daemon event-loop thread is killed
            // mid-execution during interpreter finalization.
            if let Some(shared) = event_loop_handle.lock().ok().and_then(|mut g| g.take()) {
                let loop_ref = shared.lock().ok().and_then(|mut g| g.take());
                if let Some(event_loop) = loop_ref {
                    let _ = Python::try_attach(|py| -> PyResult<()> {
                        let loop_ = event_loop.bind(py);
                        let stop = loop_.getattr("stop")?;
                        loop_.call_method1("call_soon_threadsafe", (stop,))?;
                        Ok(())
                    });
                }
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
