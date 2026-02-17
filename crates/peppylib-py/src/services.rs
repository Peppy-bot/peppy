mod health;
mod ready;
mod shutdown;

use peppylib::PeppyResult;
use pyo3::prelude::*;
use std::sync::Mutex;
use tokio::task::JoinHandle;

/// Python wrapper for a running service task (JoinHandle).
#[pyclass(name = "ServiceTask")]
pub struct PyServiceTask {
    inner: Mutex<Option<JoinHandle<PeppyResult<()>>>>,
}

impl PyServiceTask {
    pub(crate) fn new(handle: JoinHandle<PeppyResult<()>>) -> Self {
        Self {
            inner: Mutex::new(Some(handle)),
        }
    }
}

#[pymethods]
impl PyServiceTask {
    /// Returns true if the service task has finished.
    fn is_finished(&self) -> PyResult<bool> {
        let guard = self.inner.lock().map_err(|_| {
            PyErr::new::<pyo3::exceptions::PyRuntimeError, _>("service task mutex poisoned")
        })?;
        Ok(guard.as_ref().is_none_or(|h| h.is_finished()))
    }

    /// Abort the service task.
    fn abort(&self) -> PyResult<()> {
        let mut guard = self.inner.lock().map_err(|_| {
            PyErr::new::<pyo3::exceptions::PyRuntimeError, _>("service task mutex poisoned")
        })?;
        if let Some(h) = guard.take() {
            h.abort();
        }
        Ok(())
    }
}

/// Register the services submodule.
pub fn register(parent_module: &Bound<'_, PyModule>) -> PyResult<()> {
    let services_module = PyModule::new(parent_module.py(), "services")?;
    services_module.add_class::<PyServiceTask>()?;
    health::register(&services_module)?;
    ready::register(&services_module)?;
    shutdown::register(&services_module)?;
    parent_module.add_submodule(&services_module)?;
    Ok(())
}
