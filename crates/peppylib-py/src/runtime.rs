use crate::messaging::PyMessengerHandle;
use peppylib::runtime::{NodeBuilder, NodeRunner, StandaloneConfig};
use pyo3::exceptions::PyRuntimeError;
use pyo3::prelude::*;
use pyo3::sync::PyOnceLock;
use pyo3::types::PyDict;
use pythonize::{depythonize, pythonize};
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

type PyAwaitableFuture = Pin<Box<dyn Future<Output = PyResult<Py<PyAny>>> + Send>>;

static SPAWN_TARGET_FACTORY: PyOnceLock<Py<PyAny>> = PyOnceLock::new();

fn peppy_io_err(message: impl Into<String>) -> peppylib::PeppyError {
    peppylib::PeppyError::Io(std::io::Error::other(message.into()))
}

fn py_err_to_peppy(context: &str, err: PyErr) -> peppylib::PeppyError {
    peppy_io_err(format!("{context}: {err}"))
}

fn spawn_target_factory<'py>(py: Python<'py>) -> PyResult<&'py Bound<'py, PyAny>> {
    SPAWN_TARGET_FACTORY
        .get_or_try_init(py, || {
            let helper = PyModule::from_code(
                py,
                c"
def make_target(task_name, task_fn, cancel_token, cancel_on_error, state):
    def target():
        try:
            import asyncio

            if not callable(task_fn):
                raise TypeError(
                    f\"{task_name}: expected callable returning awaitable, got {type(task_fn)!r}\"
                )
            maybe_awaitable = task_fn()
            if not hasattr(maybe_awaitable, '__await__'):
                raise TypeError(
                    f\"{task_name}: callable must return an awaitable, got {type(maybe_awaitable)!r}\"
                )

            asyncio.run(maybe_awaitable)
        except Exception:
            import sys, traceback

            state['error'] = traceback.format_exc()
            print(
                f\"peppy: async task '{task_name}' raised an exception\",
                file=sys.stderr,
            )
            print(state['error'], file=sys.stderr, end='')
            if cancel_on_error:
                cancel_token.cancel()

    return target
",
                c"_peppy_spawn_async_helper",
                c"_peppy_spawn_async_helper",
            )?;

            Ok(helper.getattr("make_target")?.unbind())
        })
        .map(|factory| factory.bind(py))
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
    /// If the task raises, the traceback is always printed. When
    /// `cancel_on_error` is `True` (the default), the node cancellation token
    /// is also triggered to shut down the runner.
    #[pyo3(signature = (name, async_fn, *, cancel_on_error = true, daemon = false))]
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

        let target = spawn_target_factory(py)
            .map_err(&io_err)?
            .call1((&name, async_fn, py_cancel, cancel_on_error, &state))
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
            state: state.unbind(),
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
    /// This method blocks until the node exits (shutdown or Ctrl+C).
    /// Must be called from a thread (not from the async event loop).
    fn run(&self, py: Python<'_>, setup_fn: Py<PyAny>) -> PyResult<()> {
        let standalone_config = self.standalone_config.clone();
        let config_path = self.config_path.clone();

        // Release the GIL while blocking so other Python threads can proceed
        py.detach(|| {
            let mut builder = NodeBuilder::<serde_json::Value>::new();

            if let Some(config) = standalone_config {
                builder = builder.standalone(config);
            }
            if let Some(path) = config_path {
                builder = builder.with_config_path(path);
            }

            builder
                .run(
                    move |params: serde_json::Value, node_runner: Arc<NodeRunner>| async move {
                        // Reacquire the GIL to prepare Python arguments and run setup.
                        let maybe_setup_future = Python::try_attach(|py| {
                            let py_params = pythonize(py, &params)
                                .map_err(|e| {
                                    peppy_io_err(format!("failed to convert params to Python: {e}"))
                                })?
                                .unbind();
                            let py_runner =
                                Py::new(py, PyNodeRunner::new(Arc::clone(&node_runner))).map_err(
                                    |e| peppy_io_err(format!("failed to create PyNodeRunner: {e}")),
                                )?;

                            let setup_result =
                                setup_fn.call1(py, (py_params, py_runner)).map_err(|e| {
                                    py_err_to_peppy("setup function raised an exception", e)
                                })?;

                            let is_awaitable =
                                setup_result.bind(py).hasattr("__await__").map_err(|e| {
                                    py_err_to_peppy(
                                        "failed to inspect setup return value for awaitability",
                                        e,
                                    )
                                })?;

                            if is_awaitable {
                                let setup_future = pyo3_async_runtimes::tokio::into_future(
                                    setup_result.into_bound(py),
                                )
                                .map_err(|e| {
                                    py_err_to_peppy(
                                        "failed to convert setup awaitable into Rust future",
                                        e,
                                    )
                                })?;
                                Ok::<_, peppylib::PeppyError>(Some(
                                    Box::pin(setup_future) as PyAwaitableFuture
                                ))
                            } else {
                                Ok::<_, peppylib::PeppyError>(None)
                            }
                        })
                        .ok_or_else(|| peppy_io_err("failed to attach to Python GIL"))??;

                        if let Some(setup_future) = maybe_setup_future {
                            setup_future.await.map_err(|e| {
                                py_err_to_peppy("setup awaitable raised an exception", e)
                            })?;
                        }

                        Ok(())
                    },
                )
                .map_err(|e| PyErr::new::<pyo3::exceptions::PyRuntimeError, _>(e.to_string()))
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
