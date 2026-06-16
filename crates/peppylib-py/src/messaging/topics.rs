use super::iface::{PyProducerRef, PySenderTarget};
use super::{PyMessengerHandle, future_into_py_unit, to_py_err};
use crate::config::PyQoSProfile;
use peppylib::messaging::{Subscription, TopicMessenger, TopicPublisher};
use peppylib::types::{Message, Payload};
use pyo3::prelude::*;
use pyo3::types::PyBytes;
use std::sync::Arc;
use tokio::sync::Mutex;

/// Python wrapper for TopicMessage
#[pyclass(name = "TopicMessage", skip_from_py_object)]
#[derive(Clone)]
pub struct PyTopicMessage {
    // Held as a `Payload` (refcounted `Bytes`) rather than `Vec<u8>` so cloning a
    // `PyTopicMessage` is a refcount bump, and the wire bytes are copied into a
    // Python buffer once, in the getter, instead of also being copied eagerly at
    // construction.
    pub(crate) payload: Payload,
    pub(crate) instance_id: String,
    pub(crate) core_node: String,
}

#[pymethods]
impl PyTopicMessage {
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

    /// The publisher's full `(core_node, instance_id)` identity as a structured
    /// [`PyProducerRef`]. This is what generated consumed-topic callbacks return
    /// alongside the message; a `from_any` consumer keys per-producer state on it.
    #[getter]
    fn producer(&self) -> PyProducerRef {
        PyProducerRef::new(self.core_node.clone(), self.instance_id.clone())
    }
}

impl From<Message> for PyTopicMessage {
    fn from(msg: Message) -> Self {
        Self {
            payload: msg.payload(),
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
        crate::py_future::future_into_py(py, async move {
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
    /// Subscribe to a topic. Pass `SenderTarget.node(name, tag)` or
    /// `SenderTarget.interface(name, tag)` to match the publisher's target,
    /// or `None` to match any publisher. `is_from_any` marks the slot as
    /// `from_any: true` (gates the messenger's per-`(name, tag)`
    /// reservation). `from_producer` pins a single producer by its full
    /// [`ProducerRef`](peppylib::messaging::ProducerRef) identity; `None`
    /// wildcards. Generated code splices
    /// `node_runner.pinned_producer_for(link_id)` here.
    #[staticmethod]
    #[pyo3(signature = (messenger, as_core_node, as_instance_id, from_target, to_topic, from_producer, qos, is_from_any=false))]
    #[allow(clippy::too_many_arguments)]
    fn subscribe<'py>(
        py: Python<'py>,
        messenger: &PyMessengerHandle,
        as_core_node: String,
        as_instance_id: String,
        from_target: Option<PySenderTarget>,
        to_topic: String,
        from_producer: Option<PyProducerRef>,
        qos: PyQoSProfile,
        is_from_any: bool,
    ) -> PyResult<Bound<'py, PyAny>> {
        let handle = messenger.inner.clone();
        let from_target = from_target.map(|t| t.into_inner());
        crate::py_future::future_into_py(py, async move {
            let filter = match from_producer {
                Some(producer) => peppylib::messaging::ConsumerFilter::Pin(producer.into_inner()),
                None => peppylib::messaging::ConsumerFilter::Any,
            };
            let subscription = TopicMessenger::subscribe(
                &handle,
                &as_core_node,
                &as_instance_id,
                from_target,
                is_from_any,
                &to_topic,
                &filter,
                qos.into(),
            )
            .await
            .map_err(to_py_err)?;

            Ok(PySubscription {
                inner: Arc::new(Mutex::new(subscription)),
            })
        })
    }

    /// Declare a reusable publisher for a topic and return a [`PyTopicPublisher`].
    ///
    /// This is the only topic-publish path. The central messenger lock is taken
    /// ONCE here, at declaration; every subsequent `publisher.publish(...)` is
    /// lock-free. Declare once, then publish per message (a camera streaming
    /// frames, a sensor at rate).
    #[staticmethod]
    #[pyo3(signature = (messenger, as_core_node, as_instance_id, as_target, as_topic_name, qos))]
    fn declare_publisher<'py>(
        py: Python<'py>,
        messenger: &PyMessengerHandle,
        as_core_node: String,
        as_instance_id: String,
        as_target: PySenderTarget,
        as_topic_name: String,
        qos: PyQoSProfile,
    ) -> PyResult<Bound<'py, PyAny>> {
        let handle = messenger.inner.clone();
        let as_target = as_target.into_inner();
        crate::py_future::future_into_py(py, async move {
            let publisher = TopicMessenger::declare_publisher(
                &handle,
                &as_core_node,
                &as_instance_id,
                as_target,
                None,
                &as_topic_name,
                qos.into(),
            )
            .await
            .map_err(to_py_err)?;
            Ok(PyTopicPublisher { inner: publisher })
        })
    }
}

/// Python wrapper for a lock-free per-topic publisher, vended by
/// [`PyTopicMessenger::declare_publisher`]. Holds the topic binding so each
/// `publish` skips the central messenger lock; clone-cheap (an `Arc` bump).
#[pyclass(name = "TopicPublisher")]
pub struct PyTopicPublisher {
    inner: TopicPublisher,
}

#[pymethods]
impl PyTopicPublisher {
    /// Publish a payload on the declared topic. Lock-free on the hot path.
    fn publish<'py>(&self, py: Python<'py>, payload: Vec<u8>) -> PyResult<Bound<'py, PyAny>> {
        let publisher = self.inner.clone();
        future_into_py_unit(py, async move {
            publisher
                .publish(Payload::from(payload))
                .await
                .map_err(to_py_err)?;
            Ok(())
        })
    }
}
