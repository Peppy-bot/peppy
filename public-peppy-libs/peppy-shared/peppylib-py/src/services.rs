mod shutdown;

use parking_lot::Mutex;
use peppylib::PeppyResult;
use peppylib::runtime::TaskHandle;
use pyo3::prelude::*;

use crate::messaging::{PyMessengerHandle, to_py_err};

/// Python wrapper for a running service task (TaskHandle).
#[pyclass(name = "ServiceTask")]
pub struct PyServiceTask {
    inner: Mutex<Option<TaskHandle<PeppyResult<()>>>>,
}

impl PyServiceTask {
    pub(crate) fn new(handle: TaskHandle<PeppyResult<()>>) -> Self {
        Self {
            inner: Mutex::new(Some(handle)),
        }
    }
}

#[pymethods]
impl PyServiceTask {
    /// Returns true if the service task has finished.
    fn is_finished(&self) -> PyResult<bool> {
        Ok(self.inner.lock().as_ref().is_none_or(|h| h.is_finished()))
    }

    /// Abort the service task.
    fn abort(&self) -> PyResult<()> {
        if let Some(h) = self.inner.lock().take() {
            h.abort();
        }
        Ok(())
    }
}

/// Generates a thin PyO3 wrapper that exposes a `listen` static method
/// delegating to the given `peppylib::services::*` function.
macro_rules! service_listener {
    ($py_name:literal, $struct_name:ident, $listen_fn:path) => {
        #[pyclass(name = $py_name)]
        pub struct $struct_name;

        #[pymethods]
        impl $struct_name {
            /// Start listening for requests, returning a background `ServiceTask`.
            #[staticmethod]
            fn listen<'py>(
                py: Python<'py>,
                messenger: &PyMessengerHandle,
                core_node: String,
                instance_id: String,
                as_identity: $crate::messaging::PySenderTarget,
            ) -> PyResult<Bound<'py, PyAny>> {
                let handle = messenger.inner.clone();
                let as_identity = as_identity.into_inner();
                crate::py_future::future_into_py(py, async move {
                    let join_handle = $listen_fn(&handle, &core_node, &instance_id, as_identity)
                        .await
                        .map_err(to_py_err)?;
                    Ok(PyServiceTask::new(join_handle))
                })
            }
        }
    };
}

service_listener!(
    "NodeHealthService",
    PyNodeHealthService,
    peppylib::services::health::listen_for_node_health
);

service_listener!(
    "NodeReadyService",
    PyNodeReadyService,
    peppylib::services::ready::listen_for_node_ready
);

/// Register the services submodule.
pub(crate) fn register(parent_module: &Bound<'_, PyModule>) -> PyResult<()> {
    let services_module = PyModule::new(parent_module.py(), "services")?;
    services_module.add_class::<PyServiceTask>()?;
    services_module.add_class::<PyNodeHealthService>()?;
    services_module.add_class::<PyNodeReadyService>()?;
    shutdown::register(&services_module)?;
    parent_module.add_submodule(&services_module)?;
    Ok(())
}
