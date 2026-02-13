use super::{PyMessengerHandle, to_py_err};
use crate::config::PyQoSProfile;
use bytes::Bytes;
use peppylib::messaging::TopicMessenger;
use pmi::{Subscription, TopicMessage};
use pyo3::prelude::*;
use pyo3::types::PyBytes;
use std::sync::Arc;
use tokio::sync::Mutex;

/// Python wrapper for TopicMessage
#[pyclass(name = "TopicMessage", skip_from_py_object)]
#[derive(Clone)]
pub struct PyTopicMessage {
    pub(crate) key_expr: String,
    pub(crate) payload: Vec<u8>,
    pub(crate) instance_id: String,
    pub(crate) daemon_node: String,
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
    fn daemon_node(&self) -> &str {
        &self.daemon_node
    }
}

impl From<TopicMessage> for PyTopicMessage {
    fn from(msg: TopicMessage) -> Self {
        Self {
            key_expr: msg.key_expr().to_string(),
            payload: msg.payload().to_bytes().to_vec(),
            instance_id: msg.instance_id().to_string(),
            daemon_node: msg.daemon_node().to_string(),
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
    #[pyo3(signature = (messenger, as_daemon_node, as_instance_id, to_node_name, to_topic, to_daemon_node, to_instance_id, qos))]
    #[allow(clippy::too_many_arguments)]
    fn subscribe<'py>(
        py: Python<'py>,
        messenger: &PyMessengerHandle,
        as_daemon_node: String,
        as_instance_id: String,
        to_node_name: String,
        to_topic: String,
        to_daemon_node: Option<String>,
        to_instance_id: Option<String>,
        qos: PyQoSProfile,
    ) -> PyResult<Bound<'py, PyAny>> {
        let handle = messenger.inner.clone();
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            let subscription = TopicMessenger::subscribe(
                &handle,
                &as_daemon_node,
                &as_instance_id,
                &to_node_name,
                &to_topic,
                to_daemon_node.as_deref(),
                to_instance_id.as_deref(),
                qos.into(),
            )
            .await
            .map_err(to_py_err)?;

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
        as_daemon_node: String,
        as_instance_id: String,
        as_node_name: String,
        as_topic_name: String,
        qos: PyQoSProfile,
        payload: Vec<u8>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let handle = messenger.inner.clone();
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            TopicMessenger::emit(
                &handle,
                &as_daemon_node,
                &as_instance_id,
                &as_node_name,
                &as_topic_name,
                qos.into(),
                Bytes::from(payload),
            )
            .await
            .map_err(to_py_err)?;

            Ok(())
        })
    }
}
