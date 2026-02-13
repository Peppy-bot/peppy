use crate::messaging::PyMessengerHandle;
use peppylib::runtime::{NodeBuilder, NodeRunner, StandaloneConfig};
use pyo3::exceptions::PyRuntimeError;
use pyo3::prelude::*;
use pyo3::types::{PyCFunction, PyDict, PyString};
use pythonize::{depythonize, pythonize};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tokio_util::sync::CancellationToken;

type SharedPyError = Arc<Mutex<Option<PyErr>>>;

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
    let py_runner = Py::new(py, PyNodeRunner::new(Arc::clone(node_runner))).map_err(|e| {
        PyRuntimeError::new_err(format!("failed to create NodeRunner Python wrapper: {e}"))
    })?;
    setup_fn.call1(py, (py_params, py_runner))
}

fn is_awaitable(value: &Bound<'_, PyAny>) -> PyResult<bool> {
    value.hasattr("__await__")
}

fn run_python_awaitable(py: Python<'_>, awaitable: &Bound<'_, PyAny>) -> PyResult<()> {
    let awaitable = awaitable.clone().unbind();
    pyo3_async_runtimes::tokio::run(py, async move {
        let future = Python::try_attach(|py| {
            pyo3_async_runtimes::tokio::into_future(awaitable.bind(py).clone())
        })
        .ok_or_else(|| PyRuntimeError::new_err("failed to attach to Python GIL"))??;
        future.await?;
        Ok(())
    })?;
    Ok(())
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

fn format_python_traceback(py: Python<'_>, err: &PyErr) -> String {
    let formatted = (|| -> PyResult<String> {
        let traceback = py.import("traceback")?;
        let lines = traceback.call_method1("format_exception", (err.value(py),))?;
        let joined = PyString::new(py, "").call_method1("join", (lines,))?;
        joined.extract::<String>()
    })();

    formatted.unwrap_or_else(|_| err.to_string())
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
    inner: Arc<NodeRunner>,
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

    /// Get the daemon node this instance is bound to.
    fn bound_daemon_node(&self) -> &str {
        self.inner.processor().bound_daemon_node()
    }

    /// Get the instance ID this node is bound to.
    fn bound_instance_id(&self) -> &str {
        self.inner.processor().bound_instance_id()
    }

    /// Get the node name.
    fn node_name(&self) -> &str {
        self.inner.processor().node_name()
    }

    /// Spawn an async task in a dedicated Python thread.
    ///
    /// `async_fn` must be a zero-arg callable returning an awaitable
    /// (e.g. `lambda: my_coro(...)` or a bare `async def` with no parameters).
    ///
    /// Threads are daemonized by default so forgotten task handles do not block
    /// process exit. Set `daemon=False` if you need strict join semantics.
    ///
    /// If the task raises, the traceback is always printed. When
    /// `cancel_on_error` is `True` (the default), the node cancellation token
    /// is also triggered to shut down the runner. The exception is re-raised
    /// on the task thread, so debuggers and `threading.excepthook` can observe
    /// uncaught failures.
    #[pyo3(signature = (name, async_fn, *, cancel_on_error = true, daemon = true))]
    fn spawn_async(
        &self,
        py: Python<'_>,
        name: String,
        async_fn: Py<PyAny>,
        cancel_on_error: bool,
        daemon: bool,
    ) -> PyResult<PySpawnedAsyncTask> {
        let io_err = |e: PyErr| PyErr::new::<pyo3::exceptions::PyRuntimeError, _>(e.to_string());

        let py_cancel = Py::new(
            py,
            PyCancellationToken {
                inner: self.inner.cancellation_token().clone(),
            },
        )
        .map_err(&io_err)?;

        let state = PyDict::new(py);
        state.set_item("error", py.None()).map_err(&io_err)?;

        let task_name = name.clone();
        let task_fn = async_fn;
        let task_state = state.unbind();
        let task_state_for_target = task_state.clone_ref(py);
        let cancel_token = py_cancel.into_any();
        let target = PyCFunction::new_closure(
            py,
            Some(c"_peppy_spawn_async_target"),
            None,
            move |args, _kwargs| {
                let py = args.py();
                let run_result = (|| -> PyResult<()> {
                    if !task_fn.bind(py).is_callable() {
                        return Err(PyErr::new::<pyo3::exceptions::PyTypeError, _>(format!(
                            "{task_name}: expected callable returning awaitable, got {:?}",
                            task_fn.bind(py).get_type()
                        )));
                    }

                    let maybe_awaitable = task_fn.call0(py)?;
                    if !is_awaitable(maybe_awaitable.bind(py))? {
                        return Err(PyErr::new::<pyo3::exceptions::PyTypeError, _>(format!(
                            "{task_name}: callable must return an awaitable, got {:?}",
                            maybe_awaitable.bind(py).get_type()
                        )));
                    }

                    run_python_awaitable(py, maybe_awaitable.bind(py))?;
                    Ok(())
                })();

                if let Err(err) = run_result {
                    let traceback = format_python_traceback(py, &err);
                    let _ = task_state_for_target.bind(py).set_item("error", traceback);
                    if cancel_on_error {
                        let _ = cancel_token.bind(py).call_method0("cancel");
                    }
                    return Err(err);
                }

                Ok(())
            },
        )
        .map_err(&io_err)?;

        let kwargs = pyo3::types::PyDict::new(py);
        kwargs.set_item("target", target).map_err(&io_err)?;
        kwargs.set_item("name", name).map_err(&io_err)?;
        kwargs.set_item("daemon", daemon).map_err(&io_err)?;

        let threading = py.import("threading").map_err(&io_err)?;
        let thread = threading
            .call_method("Thread", (), Some(&kwargs))
            .map_err(&io_err)?;
        thread.call_method0("start").map_err(&io_err)?;

        Ok(PySpawnedAsyncTask {
            thread: thread.unbind(),
            state: task_state,
        })
    }
}

/// Python handle for a task created by `NodeRunner.spawn_async`.
#[pyclass(name = "SpawnedAsyncTask")]
pub struct PySpawnedAsyncTask {
    thread: Py<PyAny>,
    state: Py<PyDict>,
}

#[pymethods]
impl PySpawnedAsyncTask {
    /// Expose the underlying `threading.Thread` object.
    #[getter]
    fn thread(&self, py: Python<'_>) -> Py<PyAny> {
        self.thread.clone_ref(py)
    }

    /// Returns whether the underlying thread is still running.
    fn is_alive(&self, py: Python<'_>) -> PyResult<bool> {
        self.thread.bind(py).call_method0("is_alive")?.extract()
    }

    /// Join the thread and return True if it finished.
    #[pyo3(signature = (timeout_secs = None))]
    fn join(&self, py: Python<'_>, timeout_secs: Option<f64>) -> PyResult<bool> {
        if let Some(timeout_secs) = timeout_secs {
            let kwargs = PyDict::new(py);
            kwargs.set_item("timeout", timeout_secs)?;
            self.thread
                .bind(py)
                .call_method("join", (), Some(&kwargs))?;
        } else {
            self.thread.bind(py).call_method0("join")?;
        }

        Ok(!self.is_alive(py)?)
    }

    /// Return the captured traceback string if the task failed.
    fn exception(&self, py: Python<'_>) -> PyResult<Option<String>> {
        let Some(err_obj) = self.state.bind(py).get_item("error")? else {
            return Ok(None);
        };
        if err_obj.is_none() {
            return Ok(None);
        }
        Ok(Some(err_obj.extract::<String>()?))
    }

    /// Raise RuntimeError with the captured traceback if the task failed.
    fn raise_if_failed(&self, py: Python<'_>) -> PyResult<()> {
        if let Some(traceback) = self.exception(py)? {
            return Err(PyErr::new::<PyRuntimeError, _>(traceback));
        }
        Ok(())
    }
}

/// Python wrapper for StandaloneConfig.
#[pyclass(name = "StandaloneConfig", from_py_object)]
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

    /// Set runtime parameters from a Python dict.
    fn with_parameters(&self, py: Python<'_>, params: Py<PyAny>) -> PyResult<Self> {
        let value: serde_json::Value = depythonize(params.bind(py))?;
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
    /// The callback receives (params: dict, node_runner: NodeRunner).
    /// `run()` expects a synchronous callback. Use `run_async()` if your setup
    /// callback returns an awaitable.
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

            let run_result = builder.run(
                move |params: serde_json::Value, node_runner: Arc<NodeRunner>| {
                    let setup_error = Arc::clone(&setup_error_for_run);
                    async move {
                        match Python::try_attach(|py| -> PyResult<()> {
                            let setup_result =
                                call_setup_function(py, &setup_fn, &params, &node_runner)?;
                            let setup_bound = setup_result.bind(py);

                            if is_awaitable(setup_bound)? {
                                // Close the coroutine object to avoid un-awaited coroutine warnings.
                                let _ = setup_bound.call_method0("close");
                                return Err(PyRuntimeError::new_err(
                                    "NodeBuilder.run setup callback must be synchronous; \
                                     use NodeBuilder.run_async for async setup callbacks",
                                ));
                            }

                            Ok(())
                        }) {
                            Some(Ok(())) => Ok(()),
                            Some(Err(err)) => {
                                store_python_error(&setup_error, err);
                                Err(peppy_io_err("setup callback raised an exception"))
                            }
                            None => Err(peppy_io_err("failed to attach to Python GIL")),
                        }
                    }
                },
            );

            if let Some(err) = take_python_error(&setup_error) {
                return Err(err);
            }

            run_result.map_err(|e| PyErr::new::<pyo3::exceptions::PyRuntimeError, _>(e.to_string()))
        })
    }

    /// Run the node with an async setup callback.
    ///
    /// The callback receives (params: dict, node_runner: NodeRunner) and must
    /// return an awaitable.
    ///
    /// This method blocks until the node exits (shutdown or Ctrl+C).
    /// Must be called from a thread (not from the async event loop).
    fn run_async(&self, py: Python<'_>, setup_fn: Py<PyAny>) -> PyResult<()> {
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

            let run_result = builder.run(
                move |params: serde_json::Value, node_runner: Arc<NodeRunner>| {
                    let setup_error = Arc::clone(&setup_error_for_run);
                    async move {
                        match Python::try_attach(|py| -> PyResult<()> {
                            let setup_result =
                                call_setup_function(py, &setup_fn, &params, &node_runner)?;
                            let setup_bound = setup_result.bind(py);

                            if !is_awaitable(setup_bound)? {
                                return Err(PyRuntimeError::new_err(
                                    "NodeBuilder.run_async setup callback must return an awaitable",
                                ));
                            }

                            run_python_awaitable(py, setup_bound)?;
                            Ok(())
                        }) {
                            Some(Ok(())) => Ok(()),
                            Some(Err(err)) => {
                                store_python_error(&setup_error, err);
                                Err(peppy_io_err("setup callback raised an exception"))
                            }
                            None => Err(peppy_io_err("failed to attach to Python GIL")),
                        }
                    }
                },
            );

            if let Some(err) = take_python_error(&setup_error) {
                return Err(err);
            }

            run_result.map_err(|e| PyErr::new::<pyo3::exceptions::PyRuntimeError, _>(e.to_string()))
        })
    }
}

/// Register the runtime submodule
pub fn register(parent_module: &Bound<'_, PyModule>) -> PyResult<()> {
    let runtime_module = PyModule::new(parent_module.py(), "runtime")?;
    runtime_module.add_class::<PyCancellationToken>()?;
    runtime_module.add_class::<PyNodeRunner>()?;
    runtime_module.add_class::<PySpawnedAsyncTask>()?;
    runtime_module.add_class::<PyStandaloneConfig>()?;
    runtime_module.add_class::<PyNodeBuilder>()?;
    parent_module.add_submodule(&runtime_module)?;
    Ok(())
}
