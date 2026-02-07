use bytes::Bytes;
use peppylib::ServiceMessenger;
use peppylib::messaging::{ServiceEndpoint, ServiceRequestContext};
use pyo3::prelude::*;
use pyo3::types::PyBytes;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;

use super::{PyMessengerHandle, PyTopicMessage, to_py_err};

/// Python wrapper for a service request received by a listener.
#[pyclass(name = "ServiceRequestContext")]
pub struct PyServiceRequestContext {
    request_id: String,
    payload: Vec<u8>,
    instance_id: String,
    master_node: String,
}

#[pymethods]
impl PyServiceRequestContext {
    #[getter]
    fn request_id(&self) -> &str {
        &self.request_id
    }

    #[getter]
    fn payload<'py>(&self, py: Python<'py>) -> Bound<'py, PyBytes> {
        PyBytes::new(py, &self.payload)
    }

    #[getter]
    fn instance_id(&self) -> &str {
        &self.instance_id
    }

    #[getter]
    fn master_node(&self) -> &str {
        &self.master_node
    }

    /// Returns the underlying message as a `TopicMessage`.
    #[getter]
    fn message(&self) -> PyTopicMessage {
        PyTopicMessage {
            key_expr: String::new(),
            payload: self.payload.clone(),
            instance_id: self.instance_id.clone(),
            master_node: self.master_node.clone(),
        }
    }
}

impl From<ServiceRequestContext> for PyServiceRequestContext {
    fn from(ctx: ServiceRequestContext) -> Self {
        let request_id = ctx.request_id().to_string();
        let message = ctx.message();
        Self {
            request_id,
            payload: message.payload().to_bytes().to_vec(),
            instance_id: message.instance_id().to_string(),
            master_node: message.master_node().to_string(),
        }
    }
}

/// Python wrapper for a service endpoint that listens for incoming requests.
#[pyclass(name = "ServiceEndpoint")]
pub struct PyServiceEndpoint {
    pub(crate) inner: Arc<Mutex<ServiceEndpoint>>,
}

#[pymethods]
impl PyServiceEndpoint {
    /// Handle the next incoming request using the provided handler callable.
    ///
    /// The handler receives a `ServiceRequestContext` and must return `bytes`.
    /// Both sync and async handlers are supported.
    ///
    /// Returns `True` after processing a request, or `False` if the listener was closed.
    fn handle_next_request<'py>(
        &self,
        py: Python<'py>,
        handler: Py<PyAny>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let inner = Arc::clone(&self.inner);
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            // Phase 1: receive next request (pure Rust, no GIL needed)
            let recv_result = {
                let mut endpoint = inner.lock().await;
                endpoint.recv_next_request().await.map_err(to_py_err)?
            };

            let Some((context, responder)) = recv_result else {
                return Ok(false);
            };

            // Phase 2: call Python handler (supports sync and async callables)
            let py_context = PyServiceRequestContext::from(context);
            let (maybe_future, sync_bytes) = Python::attach(|py| -> PyResult<_> {
                let result = handler.call1(py, (py_context,))?;
                let is_awaitable = result.bind(py).hasattr("__await__")?;
                if is_awaitable {
                    let future = pyo3_async_runtimes::tokio::into_future(result.into_bound(py))?;
                    Ok((Some(future), None))
                } else {
                    Ok((None, Some(result.extract::<Vec<u8>>(py)?)))
                }
            })?;

            let response_bytes = if let Some(future) = maybe_future {
                let py_result = future.await?;
                Python::attach(|py| py_result.extract::<Vec<u8>>(py))?
            } else {
                sync_bytes.unwrap()
            };

            // Phase 3: send response (pure Rust)
            responder
                .respond(Bytes::from(response_bytes))
                .await
                .map_err(to_py_err)?;

            Ok(true)
        })
    }
}

/// Python wrapper for ServiceMessenger (request-response pattern).
#[pyclass(name = "ServiceMessenger")]
pub struct PyServiceMessenger;

#[pymethods]
impl PyServiceMessenger {
    /// Start listening for service requests.
    ///
    /// Returns a `ServiceEndpoint` that can be used to handle incoming requests.
    #[staticmethod]
    fn listen<'py>(
        py: Python<'py>,
        messenger: &PyMessengerHandle,
        as_master_node: String,
        as_instance_id: String,
        as_node_name: String,
        as_service_name: String,
    ) -> PyResult<Bound<'py, PyAny>> {
        let inner = Arc::clone(&messenger.inner);
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            let handle = inner.lock().await;
            let endpoint = ServiceMessenger::listen(
                &handle,
                &as_master_node,
                &as_instance_id,
                &as_node_name,
                &as_service_name,
            )
            .await
            .map_err(to_py_err)?;
            Ok(PyServiceEndpoint {
                inner: Arc::new(Mutex::new(endpoint)),
            })
        })
    }

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
            .map_err(to_py_err)?;
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
            .map_err(to_py_err)?;
            Ok(PyTopicMessage::from(response))
        })
    }
}

/// Register the services submodule
pub fn register(parent_module: &Bound<'_, PyModule>) -> PyResult<()> {
    let services_module = PyModule::new(parent_module.py(), "services")?;
    services_module.add_class::<PyServiceMessenger>()?;
    services_module.add_class::<PyServiceEndpoint>()?;
    services_module.add_class::<PyServiceRequestContext>()?;
    parent_module.add_submodule(&services_module)?;
    Ok(())
}
