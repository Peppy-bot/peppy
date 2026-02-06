use bytes::Bytes;
use peppylib::ServiceMessenger;
use pyo3::prelude::*;
use std::sync::Arc;
use std::time::Duration;

use crate::messaging::{PyMessengerHandle, PyTopicMessage};

/// Python wrapper for ServiceMessenger (request-response pattern).
#[pyclass(name = "ServiceMessenger")]
pub struct PyServiceMessenger;

#[pymethods]
impl PyServiceMessenger {
    /// Check if a service has active subscribers.
    #[staticmethod]
    #[pyo3(signature = (messenger, bound_master_node, as_instance_id, target_node_name, target_service_name, target_master_node=None, target_instance_id=None))]
    #[allow(clippy::too_many_arguments)]
    fn is_reachable<'py>(
        py: Python<'py>,
        messenger: &PyMessengerHandle,
        bound_master_node: String,
        as_instance_id: String,
        target_node_name: String,
        target_service_name: String,
        target_master_node: Option<String>,
        target_instance_id: Option<String>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let inner = Arc::clone(&messenger.inner);
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            let handle = inner.lock().await;
            let reachable = ServiceMessenger::is_reachable(
                &handle,
                &bound_master_node,
                &as_instance_id,
                &target_node_name,
                &target_service_name,
                target_master_node.as_deref(),
                target_instance_id.as_deref(),
            )
            .await
            .map_err(|e| PyErr::new::<pyo3::exceptions::PyRuntimeError, _>(e.to_string()))?;
            Ok(reachable)
        })
    }

    /// Send a request to a service and wait for a response.
    #[staticmethod]
    #[pyo3(signature = (messenger, bound_master_node, as_instance_id, target_node_name, target_service_name, target_master_node=None, target_instance_id=None, request_payload=vec![], response_timeout_secs=2.0))]
    #[allow(clippy::too_many_arguments)]
    fn poll<'py>(
        py: Python<'py>,
        messenger: &PyMessengerHandle,
        bound_master_node: String,
        as_instance_id: String,
        target_node_name: String,
        target_service_name: String,
        target_master_node: Option<String>,
        target_instance_id: Option<String>,
        request_payload: Vec<u8>,
        response_timeout_secs: f64,
    ) -> PyResult<Bound<'py, PyAny>> {
        let inner = Arc::clone(&messenger.inner);
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            let handle = inner.lock().await;
            let response = ServiceMessenger::poll(
                &handle,
                &bound_master_node,
                &as_instance_id,
                &target_node_name,
                &target_service_name,
                target_master_node.as_deref(),
                target_instance_id.as_deref(),
                Bytes::from(request_payload),
                Duration::from_secs_f64(response_timeout_secs),
            )
            .await
            .map_err(|e| PyErr::new::<pyo3::exceptions::PyRuntimeError, _>(e.to_string()))?;
            Ok(PyTopicMessage::from(response))
        })
    }
}

/// Register the services submodule
pub fn register(parent_module: &Bound<'_, PyModule>) -> PyResult<()> {
    let services_module = PyModule::new(parent_module.py(), "services")?;
    services_module.add_class::<PyServiceMessenger>()?;
    parent_module.add_submodule(&services_module)?;
    Ok(())
}
