use peppylib::services::ready::listen_for_node_ready;
use pyo3::prelude::*;

use super::PyServiceTask;
use crate::messaging::{PyMessengerHandle, to_py_err};

/// Python wrapper for the node ready service.
#[pyclass(name = "NodeReadyService")]
pub struct PyNodeReadyService;

#[pymethods]
impl PyNodeReadyService {
    /// Start listening for node ready requests.
    ///
    /// Returns a `ServiceTask` that runs in the background.
    #[staticmethod]
    fn listen<'py>(
        py: Python<'py>,
        messenger: &PyMessengerHandle,
        core_node: String,
        instance_id: String,
        node_name: String,
    ) -> PyResult<Bound<'py, PyAny>> {
        let handle = messenger.inner.clone();
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            let join_handle =
                listen_for_node_ready(&handle, &core_node, &instance_id, &node_name)
                    .await
                    .map_err(to_py_err)?;
            Ok(PyServiceTask::new(join_handle))
        })
    }
}

pub fn register(parent_module: &Bound<'_, PyModule>) -> PyResult<()> {
    parent_module.add_class::<PyNodeReadyService>()?;
    Ok(())
}
