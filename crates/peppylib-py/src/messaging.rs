use crate::config::PyQoSProfile;
use bytes::Bytes;
use peppylib::messaging::{MessengerHandle, TopicMessenger};
use pmi::{Subscription, TopicMessage};
use pyo3::prelude::*;
use pyo3::types::PyBytes;
use std::sync::Arc;
use tokio::sync::Mutex;

/// Python wrapper for MessengerHandle
#[pyclass(name = "MessengerHandle")]
pub struct PyMessengerHandle {
    inner: Arc<Mutex<MessengerHandle>>,
}

#[pymethods]
impl PyMessengerHandle {
    /// Connect to a messenger at the specified host and port.
    #[staticmethod]
    fn connect<'py>(py: Python<'py>, host: String, port: u16) -> PyResult<Bound<'py, PyAny>> {
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
#[pyclass(name = "TopicMessage")]
pub struct PyTopicMessage {
    key_expr: String,
    payload: Vec<u8>,
    instance_id: String,
    master_node: String,
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

/// Python wrapper for TopicMessenger
#[pyclass(name = "TopicMessenger")]
pub struct PyTopicMessenger;

#[pymethods]
impl PyTopicMessenger {
    /// Subscribe to a topic.
    #[staticmethod]
    #[pyo3(signature = (messenger, as_master_node, as_instance_id, to_node_name, to_topic, qos, to_master_node=None, to_instance_id=None))]
    #[allow(clippy::too_many_arguments)]
    fn subscribe<'py>(
        py: Python<'py>,
        messenger: &PyMessengerHandle,
        as_master_node: String,
        as_instance_id: String,
        to_node_name: String,
        to_topic: String,
        qos: PyQoSProfile,
        to_master_node: Option<String>,
        to_instance_id: Option<String>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let inner = Arc::clone(&messenger.inner);
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            let handle = inner.lock().await;
            let subscription = TopicMessenger::subscribe(
                &handle,
                &as_master_node,
                &as_instance_id,
                &to_node_name,
                &to_topic,
                to_master_node.as_deref(),
                to_instance_id.as_deref(),
                qos.into(),
            )
            .await
            .map_err(|e| PyErr::new::<pyo3::exceptions::PyRuntimeError, _>(e.to_string()))?;

            Ok(PySubscription {
                inner: Arc::new(Mutex::new(subscription)),
            })
        })
    }

    /// Emit (publish) a message to a topic.
    #[staticmethod]
    #[allow(clippy::too_many_arguments)]
    fn emit<'py>(
        py: Python<'py>,
        messenger: &PyMessengerHandle,
        as_master_node: String,
        as_instance_id: String,
        as_node_name: String,
        as_topic_name: String,
        qos: PyQoSProfile,
        payload: Vec<u8>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let inner = Arc::clone(&messenger.inner);
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            let handle = inner.lock().await;
            TopicMessenger::emit(
                &handle,
                &as_master_node,
                &as_instance_id,
                &as_node_name,
                &as_topic_name,
                qos.into(),
                Bytes::from(payload),
            )
            .await
            .map_err(|e| PyErr::new::<pyo3::exceptions::PyRuntimeError, _>(e.to_string()))?;

            Ok(())
        })
    }
}

/// Register the messaging submodule
pub fn register(parent_module: &Bound<'_, PyModule>) -> PyResult<()> {
    let messaging_module = PyModule::new(parent_module.py(), "messaging")?;
    messaging_module.add_class::<PyMessengerHandle>()?;
    messaging_module.add_class::<PyTopicMessage>()?;
    messaging_module.add_class::<PySubscription>()?;
    messaging_module.add_class::<PyTopicMessenger>()?;
    parent_module.add_submodule(&messaging_module)?;
    Ok(())
}
