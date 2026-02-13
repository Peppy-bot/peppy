use crate::messaging::PyMessengerHandle;
use peppylib::runtime::{NodeBuilder, NodeRunner, StandaloneConfig};
use pyo3::prelude::*;
use pythonize::{depythonize, pythonize};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

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
        PyMessengerHandle {
            inner: Arc::new(Mutex::new(self.inner.messenger().clone())),
        }
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
                        // Reacquire the GIL to prepare Python arguments
                        Python::try_attach(|py| {
                            let io_err = |e: PyErr| {
                                peppylib::PeppyError::Io(std::io::Error::other(e.to_string()))
                            };

                            let py_params = pythonize(py, &params)
                                .map_err(|e| {
                                    peppylib::PeppyError::Io(std::io::Error::other(format!(
                                        "failed to convert params to Python: {e}"
                                    )))
                                })?
                                .unbind();
                            let py_runner = Py::new(py, PyNodeRunner { inner: node_runner })
                                .map_err(|e| {
                                    peppylib::PeppyError::Io(std::io::Error::other(format!(
                                        "failed to create PyNodeRunner: {e}"
                                    )))
                                })?;

                            // Spawn setup in a Python threading.Thread so it
                            // can block freely (e.g. asyncio.run with
                            // long-running coroutines), mirroring tokio::spawn
                            // in Rust nodes. Using Python's threading module
                            // (rather than std::thread) ensures debuggers can
                            // attach to the thread and hit breakpoints.
                            let kwargs = pyo3::types::PyDict::new(py);
                            kwargs.set_item("target", &setup_fn).map_err(&io_err)?;
                            kwargs
                                .set_item("args", (py_params, py_runner))
                                .map_err(&io_err)?;
                            kwargs.set_item("daemon", true).map_err(&io_err)?;
                            let threading = py.import("threading").map_err(&io_err)?;
                            let thread = threading
                                .call_method("Thread", (), Some(&kwargs))
                                .map_err(&io_err)?;
                            thread.call_method0("start").map_err(&io_err)?;

                            Ok::<_, peppylib::PeppyError>(())
                        })
                        .ok_or_else(|| {
                            peppylib::PeppyError::Io(std::io::Error::other(
                                "failed to attach to Python GIL",
                            ))
                        })?
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
    runtime_module.add_class::<PyStandaloneConfig>()?;
    runtime_module.add_class::<PyNodeBuilder>()?;
    parent_module.add_submodule(&runtime_module)?;
    Ok(())
}
