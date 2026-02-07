use super::{PyMessengerHandle, PySubscription, to_py_err};
use crate::config::PyQoSProfile;
use bytes::Bytes;
use peppylib::messaging::TopicMessenger;
use pyo3::prelude::*;
use std::sync::Arc;
use tokio::sync::Mutex;

/// Python wrapper for TopicMessenger
#[pyclass(name = "TopicMessenger")]
pub struct PyTopicMessenger;

#[pymethods]
impl PyTopicMessenger {
    /// Subscribe to a topic.
    #[staticmethod]
    #[pyo3(signature = (messenger, as_master_node, as_instance_id, to_node_name, to_topic, to_master_node, to_instance_id, qos))]
    #[allow(clippy::too_many_arguments)]
    fn subscribe<'py>(
        py: Python<'py>,
        messenger: &PyMessengerHandle,
        as_master_node: String,
        as_instance_id: String,
        to_node_name: String,
        to_topic: String,
        to_master_node: Option<String>,
        to_instance_id: Option<String>,
        qos: PyQoSProfile,
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
            .map_err(to_py_err)?;

            Ok(())
        })
    }
}
