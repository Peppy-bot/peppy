use peppylib::messaging::MessengerHandle;
use pyo3::prelude::*;
use std::sync::Arc;
use tokio::sync::Mutex;

/// Python wrapper for MessengerHandle
#[pyclass(name = "MessengerHandle")]
pub struct PyMessengerHandle {
    inner: Arc<Mutex<MessengerHandle>>,
}

#[pymethods]
impl PyMessengerHandle {
    /// Connect to a messenger at the specified host and port.
    #[staticmethod]
    fn connect<'py>(py: Python<'py>, host: String, port: u16) -> PyResult<Bound<'py, PyAny>> {
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            let handle = MessengerHandle::from_host_port(&host, port)
                .await
                .map_err(|e| PyErr::new::<pyo3::exceptions::PyRuntimeError, _>(e.to_string()))?;
            Ok(PyMessengerHandle {
                inner: Arc::new(Mutex::new(handle)),
            })
        })
    }

    /// Get the messaging port.
    fn messaging_port<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let inner = Arc::clone(&self.inner);
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            let handle = inner.lock().await;
            Ok(handle.messaging_port().await)
        })
    }

    /// Get the messaging endpoint as (host, port) tuple, or None if unavailable.
    fn messaging_endpoint<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let inner = Arc::clone(&self.inner);
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            let handle = inner.lock().await;
            Ok(handle.messaging_endpoint().await)
        })
    }
}

/// Register the messaging submodule
pub fn register(parent_module: &Bound<'_, PyModule>) -> PyResult<()> {
    let messaging_module = PyModule::new(parent_module.py(), "messaging")?;
    messaging_module.add_class::<PyMessengerHandle>()?;
    parent_module.add_submodule(&messaging_module)?;
    Ok(())
}
