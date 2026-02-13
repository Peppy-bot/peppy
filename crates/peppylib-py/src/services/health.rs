use peppylib::services::health::listen_for_node_health;
use pyo3::prelude::*;
use std::sync::Arc;

use super::PyServiceTask;
use crate::messaging::{PyMessengerHandle, to_py_err};

/// Python wrapper for the node health service.
#[pyclass(name = "NodeHealthService")]
pub struct PyNodeHealthService;

#[pymethods]
impl PyNodeHealthService {
    /// Start listening for node health requests.
    ///
    /// Returns a `ServiceTask` that runs in the background.
    #[staticmethod]
    fn listen<'py>(
        py: Python<'py>,
        messenger: &PyMessengerHandle,
        daemon_node: String,
        instance_id: String,
        node_name: String,
    ) -> PyResult<Bound<'py, PyAny>> {
        let inner = Arc::clone(&messenger.inner);
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            let handle = inner.lock().await;
            let join_handle =
                listen_for_node_health(&handle, &daemon_node, &instance_id, &node_name)
                    .await
                    .map_err(to_py_err)?;
            Ok(PyServiceTask::new(join_handle))
        })
    }
}

pub fn register(parent_module: &Bound<'_, PyModule>) -> PyResult<()> {
    parent_module.add_class::<PyNodeHealthService>()?;
    Ok(())
}
