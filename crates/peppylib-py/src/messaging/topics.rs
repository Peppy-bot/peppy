use super::iface::PySenderTarget;
use super::{PyMessengerHandle, future_into_py_unit, to_py_err};
use crate::config::PyQoSProfile;
use peppylib::messaging::{LoanedPayload, Subscription, TopicMessenger, TopicPublisher};
use peppylib::types::{Message, Payload};
use pyo3::exceptions::{PyBufferError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::PyBytes;
use std::os::raw::{c_int, c_void};
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
    /// reservation). `from_instance_id` pins a single producer instance;
    /// `None` wildcards.
    #[staticmethod]
    #[pyo3(signature = (messenger, as_core_node, as_instance_id, from_target, to_topic, from_core_node, from_instance_id, qos, is_from_any=false))]
    #[allow(clippy::too_many_arguments)]
    fn subscribe<'py>(
        py: Python<'py>,
        messenger: &PyMessengerHandle,
        as_core_node: String,
        as_instance_id: String,
        from_target: Option<PySenderTarget>,
        to_topic: String,
        from_core_node: Option<String>,
        from_instance_id: Option<String>,
        qos: PyQoSProfile,
        is_from_any: bool,
    ) -> PyResult<Bound<'py, PyAny>> {
        let handle = messenger.inner.clone();
        let from_target = from_target.map(|t| t.into_inner());
        crate::py_future::future_into_py(py, async move {
            let filter = match from_instance_id {
                Some(id) => peppylib::messaging::ConsumerFilter::Pin(id),
                None => peppylib::messaging::ConsumerFilter::Any,
            };
            let subscription = TopicMessenger::subscribe(
                &handle,
                &as_core_node,
                &as_instance_id,
                from_target,
                is_from_any,
                &to_topic,
                from_core_node.as_deref(),
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

    /// Emit (publish) a message to a topic. Pass `SenderTarget.node(name, tag)`
    /// or `SenderTarget.interface(name, tag)`.
    #[staticmethod]
    #[pyo3(signature = (messenger, as_core_node, as_instance_id, as_target, as_topic_name, qos, payload))]
    #[allow(clippy::too_many_arguments)]
    fn emit<'py>(
        py: Python<'py>,
        messenger: &PyMessengerHandle,
        as_core_node: String,
        as_instance_id: String,
        as_target: PySenderTarget,
        as_topic_name: String,
        qos: PyQoSProfile,
        payload: Vec<u8>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let handle = messenger.inner.clone();
        let as_target = as_target.into_inner();
        future_into_py_unit(py, async move {
            TopicMessenger::emit(
                &handle,
                &as_core_node,
                &as_instance_id,
                as_target,
                &as_topic_name,
                qos.into(),
                Payload::from(payload),
            )
            .await
            .map_err(to_py_err)?;

            Ok(())
        })
    }

    /// Pre-bind a topic publisher for publish loops; see
    /// `TopicPublisher`. Returns an awaitable resolving to the publisher.
    /// `link_id=None` falls back to the reserved default `_` segment.
    #[staticmethod]
    #[pyo3(signature = (messenger, as_core_node, as_instance_id, as_target, as_topic_name, qos, link_id=None))]
    #[allow(clippy::too_many_arguments)]
    fn declare_publisher<'py>(
        py: Python<'py>,
        messenger: &PyMessengerHandle,
        as_core_node: String,
        as_instance_id: String,
        as_target: PySenderTarget,
        as_topic_name: String,
        qos: PyQoSProfile,
        link_id: Option<String>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let handle = messenger.inner.clone();
        let as_target = as_target.into_inner();
        crate::py_future::future_into_py(py, async move {
            let publisher = TopicMessenger::declare_publisher(
                &handle,
                &as_core_node,
                &as_instance_id,
                as_target,
                link_id.as_deref(),
                &as_topic_name,
                qos.into(),
            )
            .await
            .map_err(to_py_err)?;
            Ok(PyTopicPublisher { inner: publisher })
        })
    }
}

/// A writable publish buffer borrowed from a `TopicPublisher` via `loan()`.
/// Supports the buffer protocol: `memoryview(loan)` is a writable view the
/// caller fills in place (e.g. `np.asarray(memoryview(loan))[:] = frame`).
/// With shared memory on and a length at or above the publish threshold the
/// bytes live directly in the transport's shared-memory segment, so
/// `publisher.publish_loaned(loan)` sends them with zero further copies.
///
/// The buffer's contents are unspecified until written (a shared-memory loan
/// may contain recycled bytes of this session's earlier publishes): fill
/// every byte you publish, or `truncate` to the filled prefix.
///
/// The loan is single-use: publishing consumes it. Release every memoryview
/// (e.g. `del mv` or `mv.release()`) before publishing — a publish while a
/// view is exported raises `BufferError`. The loan and its views may be used
/// and dropped from any thread (worker-thread fills via
/// `asyncio.to_thread` are fine).
#[pyclass(name = "LoanedPayload", skip_from_py_object)]
pub struct PyLoanedPayload {
    /// `None` once published (consumed).
    inner: Option<LoanedPayload>,
    /// Live buffer-protocol exports; the loan must not be consumed or moved
    /// out while any view points into it.
    exports: usize,
}

#[pymethods]
impl PyLoanedPayload {
    /// Whether the buffer was born in shared memory (introspection for tests
    /// and benches; behavior is identical either way).
    #[getter]
    fn is_shm(&self) -> PyResult<bool> {
        Ok(self.borrow_inner()?.is_shm())
    }

    fn __len__(&self) -> PyResult<usize> {
        Ok(self.borrow_inner()?.len())
    }

    /// Shrink the loan to its filled prefix; only these bytes travel on
    /// publish. Requires all memoryviews to be released first.
    fn truncate(&mut self, new_len: usize) -> PyResult<()> {
        if self.exports > 0 {
            return Err(PyBufferError::new_err(
                "cannot truncate a loan while a memoryview is exported",
            ));
        }
        let inner = self
            .inner
            .as_mut()
            .ok_or_else(|| PyValueError::new_err("loan was already published"))?;
        inner
            .try_truncate(new_len)
            .map_err(|err| PyValueError::new_err(err.to_string()))
    }

    /// Buffer protocol: expose the loan as a writable, contiguous byte
    /// buffer. `PyBuffer_FillInfo` handles the flag negotiation.
    unsafe fn __getbuffer__(
        mut slf: PyRefMut<'_, Self>,
        view: *mut pyo3::ffi::Py_buffer,
        flags: c_int,
    ) -> PyResult<()> {
        let inner = slf
            .inner
            .as_mut()
            .ok_or_else(|| PyBufferError::new_err("loan was already published"))?;
        let ptr = inner.as_mut().as_mut_ptr() as *mut c_void;
        let len = inner.len() as pyo3::ffi::Py_ssize_t;
        // SAFETY: `ptr`/`len` describe the loan's live backing buffer, which
        // is heap- or SHM-allocated (stable address across moves of the
        // wrapper) and is kept alive until `exports` drops back to zero —
        // publish/truncate refuse while a view is out.
        let ret = unsafe {
            pyo3::ffi::PyBuffer_FillInfo(view, slf.as_ptr(), ptr, len, 0 /* writable */, flags)
        };
        if ret == -1 {
            return Err(PyErr::fetch(slf.py()));
        }
        slf.exports += 1;
        Ok(())
    }

    unsafe fn __releasebuffer__(mut slf: PyRefMut<'_, Self>, _view: *mut pyo3::ffi::Py_buffer) {
        slf.exports -= 1;
    }
}

impl PyLoanedPayload {
    fn borrow_inner(&self) -> PyResult<&LoanedPayload> {
        self.inner
            .as_ref()
            .ok_or_else(|| PyValueError::new_err("loan was already published"))
    }

    /// Consumes the loan for publishing; refuses while a view is exported.
    fn take_for_publish(&mut self) -> PyResult<LoanedPayload> {
        if self.exports > 0 {
            return Err(PyBufferError::new_err(
                "release every memoryview of the loan before publishing it",
            ));
        }
        self.inner
            .take()
            .ok_or_else(|| PyValueError::new_err("loan was already published"))
    }
}

/// Lock-free pre-bound topic publisher returned by
/// `TopicMessenger.declare_publisher`. `publish` skips the central messenger
/// lock; `loan`/`publish_loaned` give zero-copy publishing into shared
/// memory when the transport has it on.
#[pyclass(name = "TopicPublisher", skip_from_py_object)]
pub struct PyTopicPublisher {
    inner: TopicPublisher,
}

#[pymethods]
impl PyTopicPublisher {
    /// Publish an owned payload (`bytes`).
    fn publish<'py>(&self, py: Python<'py>, payload: Vec<u8>) -> PyResult<Bound<'py, PyAny>> {
        let publisher = self.inner.clone();
        future_into_py_unit(py, async move {
            publisher
                .publish(Payload::from(payload))
                .await
                .map_err(to_py_err)
        })
    }

    /// Borrow a writable publish buffer of `len` bytes (see `LoanedPayload`).
    fn loan(&self, len: usize) -> PyLoanedPayload {
        PyLoanedPayload {
            inner: Some(self.inner.loan(len)),
            exports: 0,
        }
    }

    /// Publish a filled loan, consuming it. Zero-copy when the loan is
    /// SHM-backed; the regular path otherwise. Raises `BufferError` if a
    /// memoryview of the loan is still exported, `ValueError` if the loan
    /// was already published.
    fn publish_loaned<'py>(
        &self,
        py: Python<'py>,
        loan: &mut PyLoanedPayload,
    ) -> PyResult<Bound<'py, PyAny>> {
        let loaned = loan.take_for_publish()?;
        let publisher = self.inner.clone();
        future_into_py_unit(py, async move {
            publisher.publish_loaned(loaned).await.map_err(to_py_err)
        })
    }
}
