mod actions;
mod iface;
mod services;
mod topics;

pub(crate) use iface::{PyProducerRef, PySenderTarget};

use peppylib::PeppyError;
use peppylib::messaging::MessengerHandle;
use pmi::{MessengerBackend, ZenohAdapter, ZenohdInstance};
use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
pub(crate) use topics::{PySubscription, PyTopicMessage, PyTopicMessenger, PyTopicPublisher};

/// Convert a `peppylib::error::Error` into an appropriate Python exception.
///
/// Maps timeout and unreachable variants to their natural Python counterparts
/// so that callers can catch `TimeoutError` or `ConnectionError` by type.
/// `ActionFeedbackProducerGone` joins the `ConnectionError` family (the peer
/// vanished), which keeps it type-distinguishable from the clean
/// end-of-stream close (`ActionFeedbackChannelClosed` → `RuntimeError`).
pub(crate) fn to_py_err(err: PeppyError) -> PyErr {
    match &err {
        PeppyError::ServiceTimeout { .. } | PeppyError::ActionResultTimeout { .. } => {
            PyErr::new::<pyo3::exceptions::PyTimeoutError, _>(err.to_string())
        }
        PeppyError::ServiceUnreachable { .. }
        | PeppyError::ActionResultUnreachable { .. }
        | PeppyError::ActionFeedbackProducerGone { .. } => {
            PyErr::new::<pyo3::exceptions::PyConnectionError, _>(err.to_string())
        }
        _ => PyErr::new::<pyo3::exceptions::PyRuntimeError, _>(err.to_string()),
    }
}

pub(crate) fn duration_from_secs_f64(arg_name: &str, secs: f64) -> PyResult<Duration> {
    Duration::try_from_secs_f64(secs).map_err(|_| {
        PyErr::new::<PyValueError, _>(format!(
            "{arg_name} must be a finite, non-negative number of seconds, got {secs}"
        ))
    })
}

/// Wrap a `core_node_api::Error` from an `encode()` call as a Python
/// `RuntimeError` — encode failures indicate an internal/wire-shape bug, not
/// caller misuse.
pub(crate) fn encode_err(what: &str, err: core_node_api::Error) -> PyErr {
    PyErr::new::<pyo3::exceptions::PyRuntimeError, _>(format!("failed to encode {what}: {err}"))
}

/// Wrap a `core_node_api::Error` from a `decode()` call as a Python
/// `ValueError` — decode failures usually mean the bytes the caller passed in
/// don't match the expected wire schema.
pub(crate) fn decode_err(what: &str, err: core_node_api::Error) -> PyErr {
    PyValueError::new_err(format!("failed to decode {what}: {err}"))
}

/// Drive a void async binding and resolve it to Python `None`.
///
/// `future_into_py` converts the future's `Ok` value via `IntoPyObject`, and
/// under PyO3 0.28 a bare `()` converts to an empty tuple rather than `None`,
/// while `Option::<()>::None` converts to `None`. Routing void async methods
/// through this helper gives them the Pythonic `None` return. The
/// `Output = PyResult<()>` bound also means the compiler rejects any non-void
/// future passed here by mistake.
pub(crate) fn future_into_py_unit<'py, F>(py: Python<'py>, fut: F) -> PyResult<Bound<'py, PyAny>>
where
    F: Future<Output = PyResult<()>> + Send + 'static,
{
    crate::py_future::future_into_py(py, async move {
        fut.await?;
        Ok(None::<()>)
    })
}

/// Python wrapper for ZenohdInstance - an ephemeral zenohd router for testing.
///
/// Use as an async context manager (`async with`) or call `stop()` explicitly
/// to ensure the router is cleanly shut down.
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
        crate::py_future::future_into_py(py, async move {
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
        future_into_py_unit(py, async move {
            let mut guard = inner.lock().await;
            if let Some(mut instance) = guard.take() {
                instance.take_messenger().stop_router().await.map_err(|e| {
                    PyErr::new::<pyo3::exceptions::PyRuntimeError, _>(e.to_string())
                })?;
            }
            Ok(())
        })
    }

    /// Async context manager entry - returns self.
    fn __aenter__(slf: Py<Self>, py: Python<'_>) -> PyResult<Bound<'_, PyAny>> {
        // Return a coroutine that immediately resolves to self
        crate::py_future::future_into_py(py, async move { Ok(slf) })
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

/// Python wrapper for MessengerHandle.
///
/// `MessengerHandle` already manages its own internal `Arc<Mutex<Messenger>>`,
/// so no additional outer lock is needed here. Cloning is a cheap `Arc` bump.
#[pyclass(name = "MessengerHandle", skip_from_py_object)]
#[derive(Clone)]
pub struct PyMessengerHandle {
    pub(crate) inner: MessengerHandle,
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
        crate::py_future::future_into_py(py, async move {
            let handle = MessengerHandle::from_host_port(&host, port)
                .await
                .map_err(|e| PyErr::new::<pyo3::exceptions::PyRuntimeError, _>(e.to_string()))?;
            Ok(PyMessengerHandle { inner: handle })
        })
    }

    /// Get the messaging port.
    fn messaging_port<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let handle = self.inner.clone();
        crate::py_future::future_into_py(py, async move { Ok(handle.messaging_port().await) })
    }

    /// Get the messaging endpoint as (host, port) tuple, or None if unavailable.
    fn messaging_endpoint<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let handle = self.inner.clone();
        crate::py_future::future_into_py(py, async move { Ok(handle.messaging_endpoint().await) })
    }
}

/// Register the messaging submodule
pub(crate) fn register(parent_module: &Bound<'_, PyModule>) -> PyResult<()> {
    let messaging_module = PyModule::new(parent_module.py(), "messaging")?;
    messaging_module.add_class::<PyZenohdInstance>()?;
    messaging_module.add_class::<PyMessengerHandle>()?;
    messaging_module.add_class::<PyTopicMessage>()?;
    messaging_module.add_class::<PySubscription>()?;
    messaging_module.add_class::<PyTopicMessenger>()?;
    messaging_module.add_class::<PyTopicPublisher>()?;
    messaging_module.add_class::<PySenderTarget>()?;
    messaging_module.add_class::<PyProducerRef>()?;
    services::register(&messaging_module)?;
    actions::register(&messaging_module)?;
    parent_module.add_submodule(&messaging_module)?;
    Ok(())
}
