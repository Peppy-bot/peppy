use super::iface::PyIface;
use super::{PyMessengerHandle, to_py_err};
use crate::config::PyQoSProfile;
use peppylib::messaging::{Subscription, TopicMessenger};
use peppylib::types::{Message, Payload};
use pyo3::prelude::*;
use pyo3::types::PyBytes;
use std::sync::Arc;
use tokio::sync::Mutex;

/// Python wrapper for TopicMessage
#[pyclass(name = "TopicMessage", skip_from_py_object)]
#[derive(Clone)]
pub struct PyTopicMessage {
    pub(crate) payload: Vec<u8>,
    pub(crate) instance_id: String,
    pub(crate) core_node: String,
}

#[pymethods]
impl PyTopicMessage {
    #[getter]
    fn payload<'py>(&self, py: Python<'py>) -> Bound<'py, PyBytes> {
        PyBytes::new(py, &self.payload)
    }

    #[getter]
    fn instance_id(&self) -> &str {
        &self.instance_id
    }

    #[getter]
    fn core_node(&self) -> &str {
        &self.core_node
    }
}

impl From<Message> for PyTopicMessage {
    fn from(msg: Message) -> Self {
        Self {
            payload: msg.payload().to_vec(),
            instance_id: msg.instance_id().to_string(),
            core_node: msg.core_node().to_string(),
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
    /// Subscribe to a topic. Pass `peppylib.Iface.native()` for non-`conforms_to`
    /// topics, `Iface.wildcard()` to match any publisher iface, or
    /// `Iface.conformed(name, tag)` for a specific interface.
    #[staticmethod]
    #[pyo3(signature = (messenger, as_core_node, as_instance_id, from_node_name, iface, to_topic, from_core_node, from_instance_id, qos))]
    #[allow(clippy::too_many_arguments)]
    fn subscribe<'py>(
        py: Python<'py>,
        messenger: &PyMessengerHandle,
        as_core_node: String,
        as_instance_id: String,
        from_node_name: String,
        iface: PyIface,
        to_topic: String,
        from_core_node: Option<String>,
        from_instance_id: Option<String>,
        qos: PyQoSProfile,
    ) -> PyResult<Bound<'py, PyAny>> {
        let handle = messenger.inner.clone();
        let iface = iface.into_inner();
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            let subscription = TopicMessenger::subscribe(
                &handle,
                &as_core_node,
                &as_instance_id,
                &from_node_name,
                iface,
                &to_topic,
                from_core_node.as_deref(),
                from_instance_id.as_deref(),
                qos.into(),
            )
            .await
            .map_err(to_py_err)?;

            Ok(PySubscription {
                inner: Arc::new(Mutex::new(subscription)),
            })
        })
    }

    /// Consume a topic from any node (external/unlinked topics).
    #[staticmethod]
    #[pyo3(signature = (messenger, as_core_node, as_instance_id, to_topic, from_core_node, from_instance_id, qos))]
    #[allow(clippy::too_many_arguments)]
    fn consume_external<'py>(
        py: Python<'py>,
        messenger: &PyMessengerHandle,
        as_core_node: String,
        as_instance_id: String,
        to_topic: String,
        from_core_node: Option<String>,
        from_instance_id: Option<String>,
        qos: PyQoSProfile,
    ) -> PyResult<Bound<'py, PyAny>> {
        let handle = messenger.inner.clone();
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            let subscription = TopicMessenger::consume_external(
                &handle,
                &as_core_node,
                &as_instance_id,
                &to_topic,
                from_core_node.as_deref(),
                from_instance_id.as_deref(),
                qos.into(),
            )
            .await
            .map_err(to_py_err)?;

            Ok(PySubscription {
                inner: Arc::new(Mutex::new(subscription)),
            })
        })
    }

    /// Emit (publish) a message to a topic. Pass `peppylib.Iface.native()` for
    /// non-`conforms_to` topics, or `Iface.conformed(name, tag)` for a specific
    /// interface.
    #[staticmethod]
    #[pyo3(signature = (messenger, as_core_node, as_instance_id, as_node_name, iface, as_topic_name, qos, payload))]
    #[allow(clippy::too_many_arguments)]
    fn emit<'py>(
        py: Python<'py>,
        messenger: &PyMessengerHandle,
        as_core_node: String,
        as_instance_id: String,
        as_node_name: String,
        iface: PyIface,
        as_topic_name: String,
        qos: PyQoSProfile,
        payload: Vec<u8>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let handle = messenger.inner.clone();
        let iface = iface.into_inner();
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            TopicMessenger::emit(
                &handle,
                &as_core_node,
                &as_instance_id,
                &as_node_name,
                iface,
                &as_topic_name,
                qos.into(),
                Payload::from(payload),
            )
            .await
            .map_err(to_py_err)?;

            Ok(())
        })
    }
}
