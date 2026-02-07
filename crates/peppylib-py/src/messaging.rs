mod actions;
mod services;
mod topics;

use peppylib::PeppyError;
use peppylib::messaging::MessengerHandle;
use pmi::{MessengerBackend, Subscription, TopicMessage, ZenohAdapter, ZenohdInstance};
use pyo3::prelude::*;
use pyo3::types::PyBytes;
use std::sync::Arc;
use tokio::sync::Mutex;

/// Convert a `peppylib::error::Error` into an appropriate Python exception.
///
/// Maps timeout and unreachable variants to their natural Python counterparts
/// so that callers can catch `TimeoutError` or `ConnectionError` by type.
pub(crate) fn to_py_err(err: PeppyError) -> PyErr {
    match &err {
        PeppyError::ServiceTimeout { .. } | PeppyError::ActionResultTimeout { .. } => {
            PyErr::new::<pyo3::exceptions::PyTimeoutError, _>(err.to_string())
        }
        PeppyError::ServiceUnreachable { .. } | PeppyError::ActionResultUnreachable { .. } => {
            PyErr::new::<pyo3::exceptions::PyConnectionError, _>(err.to_string())
        }
        _ => PyErr::new::<pyo3::exceptions::PyRuntimeError, _>(err.to_string()),
    }
}

use topics::PyTopicMessenger;

/// Python wrapper for ZenohdInstance - an ephemeral zenohd router for testing.
///
/// The router is automatically stopped when this instance is garbage collected.
#[pyclass(name = "ZenohdInstance")]
pub struct PyZenohdInstance {
    inner: Arc<Mutex<Option<ZenohdInstance>>>,
    host: String,
    port: u16,
}

#[pymethods]
impl PyZenohdInstance {
    /// Start an ephemeral zenohd router on the specified host.
    ///
    /// If port is None, an available port will be automatically selected.
    /// If port is Some, that specific port will be used.
    ///
    /// Returns a ZenohdInstance that automatically stops the router when dropped.
    #[staticmethod]
    #[pyo3(signature = (host, port=None))]
    fn start_ephemeral<'py>(
        py: Python<'py>,
        host: String,
        port: Option<u16>,
    ) -> PyResult<Bound<'py, PyAny>> {
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            let instance = ZenohAdapter::start_router_ephemeral(&host, port)
                .await
                .map_err(|e| PyErr::new::<pyo3::exceptions::PyRuntimeError, _>(e.to_string()))?;

            let host = instance.host.clone();
            let port = instance.port;

            Ok(PyZenohdInstance {
                inner: Arc::new(Mutex::new(Some(instance))),
                host,
                port,
            })
        })
    }

    /// The host address the router is listening on.
    #[getter]
    fn host(&self) -> &str {
        &self.host
    }

    /// The port the router is listening on.
    #[getter]
    fn port(&self) -> u16 {
        self.port
    }

    /// Stop the router explicitly.
    ///
    /// This is called automatically when the instance is garbage collected,
    /// but can be called manually for deterministic cleanup.
    fn stop<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let inner = Arc::clone(&self.inner);
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            let mut guard = inner.lock().await;
            if let Some(mut instance) = guard.take() {
                instance.messenger().stop_router().await.map_err(|e| {
                    PyErr::new::<pyo3::exceptions::PyRuntimeError, _>(e.to_string())
                })?;
            }
            Ok(())
        })
    }

    /// Async context manager entry - returns self.
    fn __aenter__(slf: Py<Self>, py: Python<'_>) -> PyResult<Bound<'_, PyAny>> {
        // Return a coroutine that immediately resolves to self
        pyo3_async_runtimes::tokio::future_into_py(py, async move { Ok(slf) })
    }

    /// Async context manager exit - stops the router.
    #[pyo3(signature = (_exc_type=None, _exc_val=None, _exc_tb=None))]
    fn __aexit__<'py>(
        &self,
        py: Python<'py>,
        _exc_type: Option<Py<PyAny>>,
        _exc_val: Option<Py<PyAny>>,
        _exc_tb: Option<Py<PyAny>>,
    ) -> PyResult<Bound<'py, PyAny>> {
        self.stop(py)
    }
}

/// Python wrapper for MessengerHandle
#[pyclass(name = "MessengerHandle")]
pub struct PyMessengerHandle {
    pub(crate) inner: Arc<Mutex<MessengerHandle>>,
}

#[pymethods]
impl PyMessengerHandle {
    /// Connect to a messenger at the specified host and port.
    #[staticmethod]
    fn from_host_port<'py>(
        py: Python<'py>,
        host: String,
        port: u16,
    ) -> PyResult<Bound<'py, PyAny>> {
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

/// Python wrapper for TopicMessage
#[pyclass(name = "TopicMessage", skip_from_py_object)]
#[derive(Clone)]
pub struct PyTopicMessage {
    pub(crate) key_expr: String,
    pub(crate) payload: Vec<u8>,
    pub(crate) instance_id: String,
    pub(crate) master_node: String,
}

#[pymethods]
impl PyTopicMessage {
    #[getter]
    fn key_expr(&self) -> &str {
        &self.key_expr
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
}

impl From<TopicMessage> for PyTopicMessage {
    fn from(msg: TopicMessage) -> Self {
        Self {
            key_expr: msg.key_expr().to_string(),
            payload: msg.payload().to_bytes().to_vec(),
            instance_id: msg.instance_id().to_string(),
            master_node: msg.master_node().to_string(),
        }
    }
}

/// Python wrapper for Subscription
#[pyclass(name = "Subscription")]
pub struct PySubscription {
    inner: Arc<Mutex<Subscription>>,
}

#[pymethods]
impl PySubscription {
    /// Wait for and receive the next message.
    fn on_next_message<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let inner = Arc::clone(&self.inner);
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            let mut subscription = inner.lock().await;
            match subscription.on_next_message().await {
                Some(message) => Ok(Some(PyTopicMessage::from(message))),
                None => Ok(None),
            }
        })
    }
}

/// Register the messaging submodule
pub fn register(parent_module: &Bound<'_, PyModule>) -> PyResult<()> {
    let messaging_module = PyModule::new(parent_module.py(), "messaging")?;
    messaging_module.add_class::<PyZenohdInstance>()?;
    messaging_module.add_class::<PyMessengerHandle>()?;
    messaging_module.add_class::<PyTopicMessage>()?;
    messaging_module.add_class::<PySubscription>()?;
    messaging_module.add_class::<PyTopicMessenger>()?;
    services::register(&messaging_module)?;
    actions::register(&messaging_module)?;
    parent_module.add_submodule(&messaging_module)?;
    Ok(())
}
