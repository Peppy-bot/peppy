use peppylib::ServiceMessenger;
use peppylib::messaging::{ServiceEndpoint, ServiceRequestContext, ServiceTarget};
use peppylib::types::Payload;
use pyo3::prelude::*;
use pyo3::types::PyBytes;
use std::sync::Arc;
use tokio::sync::Mutex;

use super::iface::{PyProducerRef, PySenderTarget};
use super::{PyMessengerHandle, PyTopicMessage, duration_from_secs_f64, to_py_err};

/// Python wrapper for a service request received by a listener.
#[pyclass(name = "ServiceRequestContext")]
pub struct PyServiceRequestContext {
    request_id: String,
    link_id: String,
    // `Payload` (refcounted `Bytes`) so handing the bytes to the `message()`
    // wrapper is a refcount bump and Python receives a single copy in the getter.
    payload: Payload,
    instance_id: String,
    core_node: String,
}

#[pymethods]
impl PyServiceRequestContext {
    #[getter]
    fn request_id(&self) -> &str {
        &self.request_id
    }

    #[getter]
    fn link_id(&self) -> &str {
        &self.link_id
    }

    #[getter]
    fn payload<'py>(&self, py: Python<'py>) -> Bound<'py, PyBytes> {
        PyBytes::new(py, self.payload.as_ref())
    }

    #[getter]
    fn instance_id(&self) -> &str {
        &self.instance_id
    }

    #[getter]
    fn core_node(&self) -> &str {
        &self.core_node
    }

    /// Returns the underlying message as a `TopicMessage`.
    #[getter]
    fn message(&self) -> PyTopicMessage {
        PyTopicMessage {
            payload: self.payload.clone(),
            instance_id: self.instance_id.clone(),
            core_node: self.core_node.clone(),
        }
    }
}

impl From<ServiceRequestContext> for PyServiceRequestContext {
    fn from(ctx: ServiceRequestContext) -> Self {
        let request_id = ctx.request_id().to_string();
        let link_id = ctx.link_id().to_string();
        let message = ctx.message();
        Self {
            request_id,
            link_id,
            payload: message.payload(),
            instance_id: message.instance_id().to_string(),
            core_node: message.core_node().to_string(),
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
        crate::py_future::future_into_py(py, async move {
            // Phase 1: receive next request (pure Rust, no GIL needed)
            let recv_result = {
                let mut endpoint = inner.lock().await;
                endpoint.recv_next_request().await.map_err(to_py_err)?
            };

            let Some((context, responder)) = recv_result else {
                return Ok(false);
            };

            // Phase 2: call Python handler (supports sync and async callables).
            // Gated attach: during interpreter shutdown the handler can no
            // longer run, so drop the request and report the listener closed.
            let py_context = PyServiceRequestContext::from(context);
            let Some(handler_call) = crate::py_future::try_attach_gated(|py| -> PyResult<_> {
                let result = handler.call1(py, (py_context,))?;
                let is_awaitable = result.bind(py).hasattr("__await__")?;
                if is_awaitable {
                    let future = pyo3_async_runtimes::tokio::into_future(result.into_bound(py))?;
                    Ok((Some(future), None))
                } else {
                    Ok((None, Some(result.extract::<Vec<u8>>(py)?)))
                }
            }) else {
                return Ok(false);
            };
            // Phase 3: send response (pure Rust). Handler errors take the
            // structured `respond_error` path so the caller sees
            // `ServiceError { reason }` without the framework smuggling a
            // sentinel through the response payload.
            let send_result = match handler_call {
                Ok((maybe_future, sync_bytes)) => {
                    let response_bytes = if let Some(future) = maybe_future {
                        match future.await {
                            Ok(py_result) => match crate::py_future::try_attach_gated(|py| {
                                py_result.extract::<Vec<u8>>(py)
                            }) {
                                Some(extracted) => extracted.map_err(|err| err.to_string()),
                                None => Err("node is shutting down".to_string()),
                            },
                            Err(err) => Err(err.to_string()),
                        }
                    } else if let Some(sync_bytes) = sync_bytes {
                        Ok(sync_bytes)
                    } else {
                        Err("internal error: missing synchronous handler response bytes"
                            .to_string())
                    };

                    match response_bytes {
                        Ok(response_bytes) => {
                            responder.respond(Payload::from(response_bytes)).await
                        }
                        Err(reason) => responder.respond_error(reason).await,
                    }
                }
                Err(err) => responder.respond_error(err.to_string()).await,
            };
            send_result.map_err(to_py_err)?;

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
    #[pyo3(signature = (messenger, as_core_node, as_instance_id, as_identity, as_service_name))]
    fn listen<'py>(
        py: Python<'py>,
        messenger: &PyMessengerHandle,
        as_core_node: String,
        as_instance_id: String,
        as_identity: PySenderTarget,
        as_service_name: String,
    ) -> PyResult<Bound<'py, PyAny>> {
        let handle = messenger.inner.clone();
        let as_identity = as_identity.into_inner();
        crate::py_future::future_into_py(py, async move {
            let endpoint = ServiceMessenger::listen(
                &handle,
                &as_core_node,
                &as_instance_id,
                as_identity,
                &as_service_name,
            )
            .await
            .map_err(to_py_err)?;
            Ok(PyServiceEndpoint {
                inner: Arc::new(Mutex::new(endpoint)),
            })
        })
    }

    /// Check whether a service producer is reachable. `target` is the
    /// producer's full `(core_node, instance_id)` pair (`None` probes any
    /// matching producer).
    #[staticmethod]
    #[pyo3(signature = (messenger, bound_core_node, as_instance_id, to_target, to_service_name, target=None))]
    fn is_reachable<'py>(
        py: Python<'py>,
        messenger: &PyMessengerHandle,
        bound_core_node: String,
        as_instance_id: String,
        to_target: PySenderTarget,
        to_service_name: String,
        target: Option<PyProducerRef>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let handle = messenger.inner.clone();
        let to_target = to_target.into_inner();
        crate::py_future::future_into_py(py, async move {
            let target = target.map(PyProducerRef::into_inner);
            let reachable = ServiceMessenger::is_reachable(
                &handle,
                &bound_core_node,
                &as_instance_id,
                to_target,
                &to_service_name,
                target
                    .as_ref()
                    .map_or(ServiceTarget::Any, ServiceTarget::Producer),
            )
            .await
            .map_err(to_py_err)?;
            Ok(reachable)
        })
    }

    /// Send a request to a service and wait for a response. `target` is the
    /// producer's full `(core_node, instance_id)` pair — `Some` pins it (no
    /// discovery), `None` is a genuine wildcard (discover-then-pin).
    /// Generated code splices `node_runner.pinned_producer_for(link_id)`.
    #[staticmethod]
    #[pyo3(signature = (messenger, bound_core_node, as_instance_id, to_target, to_service_name, target=None, request_payload=vec![], response_timeout_secs=2.0))]
    #[allow(clippy::too_many_arguments)]
    fn poll<'py>(
        py: Python<'py>,
        messenger: &PyMessengerHandle,
        bound_core_node: String,
        as_instance_id: String,
        to_target: PySenderTarget,
        to_service_name: String,
        target: Option<PyProducerRef>,
        request_payload: Vec<u8>,
        response_timeout_secs: f64,
    ) -> PyResult<Bound<'py, PyAny>> {
        let response_timeout =
            duration_from_secs_f64("response_timeout_secs", response_timeout_secs)?;
        let handle = messenger.inner.clone();
        let to_target = to_target.into_inner();
        crate::py_future::future_into_py(py, async move {
            let target = target.map(PyProducerRef::into_inner);
            let response = ServiceMessenger::poll(
                &handle,
                &bound_core_node,
                &as_instance_id,
                to_target,
                &to_service_name,
                target
                    .as_ref()
                    .map_or(ServiceTarget::Any, ServiceTarget::Producer),
                Payload::from(request_payload),
                response_timeout,
            )
            .await
            .map_err(to_py_err)?;
            Ok(PyTopicMessage::from(response))
        })
    }
}

/// Register the services submodule
pub(crate) fn register(parent_module: &Bound<'_, PyModule>) -> PyResult<()> {
    let services_module = PyModule::new(parent_module.py(), "services")?;
    services_module.add_class::<PyServiceMessenger>()?;
    services_module.add_class::<PyServiceEndpoint>()?;
    services_module.add_class::<PyServiceRequestContext>()?;
    parent_module.add_submodule(&services_module)?;
    Ok(())
}
