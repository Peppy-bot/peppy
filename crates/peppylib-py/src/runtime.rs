use peppylib::runtime::{NodeBuilder, NodeRunner, StandaloneConfig};
use pyo3::prelude::*;
use pythonize::{depythonize, pythonize};
use std::path::PathBuf;
use std::sync::Arc;
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

    /// Set runtime parameters from a Python dict (JSON-like value).
    fn with_parameters_json(&self, py: Python<'_>, params: Py<PyAny>) -> PyResult<Self> {
        let value: serde_json::Value = depythonize(&params.bind(py))?;
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
                        // Reacquire the GIL to call the Python setup function
                        Python::try_attach(|py| {
                            let py_params = pythonize(py, &params).map_err(|e| {
                                peppylib::PeppyError::Io(std::io::Error::other(format!(
                                    "failed to convert params to Python: {e}"
                                )))
                            })?;
                            let py_runner = Py::new(py, PyNodeRunner { inner: node_runner })
                                .map_err(|e| {
                                    peppylib::PeppyError::Io(std::io::Error::other(format!(
                                        "failed to create PyNodeRunner: {e}"
                                    )))
                                })?;

                            setup_fn.call1(py, (py_params, py_runner)).map_err(|e| {
                                peppylib::PeppyError::Io(std::io::Error::other(format!(
                                    "Python setup function error: {e}"
                                )))
                            })?;
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
