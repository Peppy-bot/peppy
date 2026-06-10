use parking_lot::Mutex;
use peppylib::services::shutdown::listen_for_shutdown;
use pyo3::prelude::*;

use super::PyServiceTask;
use crate::messaging::{PyMessengerHandle, to_py_err};

/// Python wrapper for a shutdown signal receiver.
///
/// When a shutdown request is received by the service, this receiver completes.
#[pyclass(name = "ShutdownReceiver")]
pub struct PyShutdownReceiver {
    inner: Mutex<Option<tokio::sync::oneshot::Receiver<()>>>,
}

#[pymethods]
impl PyShutdownReceiver {
    /// Wait for the shutdown signal.
    ///
    /// Returns `True` when a shutdown request is received, or `False` if the
    /// sender was dropped without sending.
    fn wait<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let rx = self.inner.lock().take().ok_or_else(|| {
            PyErr::new::<pyo3::exceptions::PyRuntimeError, _>("shutdown receiver already consumed")
        })?;
        crate::py_future::future_into_py(py, async move {
            match rx.await {
                Ok(()) => Ok(true),
                Err(_) => Ok(false),
            }
        })
    }
}

/// Python wrapper for the shutdown service.
#[pyclass(name = "ShutdownService")]
pub struct PyShutdownService;

#[pymethods]
impl PyShutdownService {
    /// Start listening for shutdown requests.
    ///
    /// Returns a tuple of (`ServiceTask`, `ShutdownReceiver`).
    #[staticmethod]
    fn listen<'py>(
        py: Python<'py>,
        messenger: &PyMessengerHandle,
        core_node: String,
        instance_id: String,
        as_identity: crate::messaging::PySenderTarget,
    ) -> PyResult<Bound<'py, PyAny>> {
        let handle = messenger.inner.clone();
        let as_identity = as_identity.into_inner();
        crate::py_future::future_into_py(py, async move {
            let (join_handle, shutdown_rx) =
                listen_for_shutdown(&handle, &core_node, &instance_id, as_identity)
                    .await
                    .map_err(to_py_err)?;

            let task = PyServiceTask::new(join_handle);
            let receiver = PyShutdownReceiver {
                inner: Mutex::new(Some(shutdown_rx)),
            };
            Ok((task, receiver))
        })
    }
}

pub(crate) fn register(parent_module: &Bound<'_, PyModule>) -> PyResult<()> {
    parent_module.add_class::<PyShutdownService>()?;
    parent_module.add_class::<PyShutdownReceiver>()?;
    Ok(())
}
