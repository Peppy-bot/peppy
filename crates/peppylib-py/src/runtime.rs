use peppylib::runtime::{NodeBuilder, NodeRunner, StandaloneConfig};
use pyo3::prelude::*;
use pyo3::types::{PyBool, PyDict, PyFloat, PyInt, PyList, PyString};
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
        let value = py_to_json_value(py, &params)?;
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
                            let py_params = json_value_to_py(py, &params).map_err(|e| {
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

/// Convert serde_json::Value to a Python object.
fn json_value_to_py(py: Python<'_>, value: &serde_json::Value) -> PyResult<Py<PyAny>> {
    match value {
        serde_json::Value::Null => Ok(py.None()),
        serde_json::Value::Bool(b) => Ok(b.into_pyobject(py)?.to_owned().into_any().unbind()),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Ok(i.into_pyobject(py)?.into_any().unbind())
            } else {
                Ok(n.as_f64()
                    .unwrap_or(0.0)
                    .into_pyobject(py)?
                    .into_any()
                    .unbind())
            }
        }
        serde_json::Value::String(s) => {
            Ok(PyString::new(py, s).into_pyobject(py)?.into_any().unbind())
        }
        serde_json::Value::Array(arr) => {
            let items: Vec<Py<PyAny>> = arr
                .iter()
                .map(|v| json_value_to_py(py, v))
                .collect::<PyResult<_>>()?;
            Ok(PyList::new(py, items)?
                .into_pyobject(py)?
                .into_any()
                .unbind())
        }
        serde_json::Value::Object(map) => {
            let dict = PyDict::new(py);
            for (k, v) in map {
                dict.set_item(k, json_value_to_py(py, v)?)?;
            }
            Ok(dict.into_pyobject(py)?.into_any().unbind())
        }
    }
}

/// Convert a Python object to serde_json::Value.
fn py_to_json_value(py: Python<'_>, obj: &Py<PyAny>) -> PyResult<serde_json::Value> {
    let bound = obj.bind(py);

    if bound.is_none() {
        return Ok(serde_json::Value::Null);
    }
    if let Ok(b) = bound.cast::<PyBool>() {
        return Ok(serde_json::Value::Bool(b.is_true()));
    }
    if let Ok(i) = bound.cast::<PyInt>() {
        let val: i64 = i.extract()?;
        return Ok(serde_json::Value::Number(val.into()));
    }
    if let Ok(f) = bound.cast::<PyFloat>() {
        let val: f64 = f.extract()?;
        return Ok(serde_json::Value::Number(
            serde_json::Number::from_f64(val).ok_or_else(|| {
                PyErr::new::<pyo3::exceptions::PyValueError, _>("float is not a valid JSON number")
            })?,
        ));
    }
    if let Ok(s) = bound.cast::<PyString>() {
        return Ok(serde_json::Value::String(s.to_string()));
    }
    if let Ok(list) = bound.cast::<PyList>() {
        let items: Vec<serde_json::Value> = list
            .iter()
            .map(|item| py_to_json_value(py, &item.unbind()))
            .collect::<PyResult<_>>()?;
        return Ok(serde_json::Value::Array(items));
    }
    if let Ok(dict) = bound.cast::<PyDict>() {
        let mut map = serde_json::Map::new();
        for (k, v) in dict.iter() {
            let key: String = k.extract()?;
            map.insert(key, py_to_json_value(py, &v.unbind())?);
        }
        return Ok(serde_json::Value::Object(map));
    }

    Err(PyErr::new::<pyo3::exceptions::PyTypeError, _>(format!(
        "cannot convert {} to JSON",
        bound.get_type().name()?
    )))
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
