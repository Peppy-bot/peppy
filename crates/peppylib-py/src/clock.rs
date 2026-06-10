//! Python bindings for the `clock` wire types and the high-level
//! `synchronize` helper.
//!
//! Mirrors `core_node_api::encoding::clock::{ClockRequest, ClockResponse,
//! ClockTick}` and `peppylib::clock::{ClockSync, synchronize}`.

use std::sync::Arc;

use core_node_api::encoding::{ClockRequest, ClockResponse, ClockTick};
use peppylib::clock::{ClockSync, PeppyClock, for_node, subscribe, synchronize};
use peppylib::messaging::Subscription;
use pyo3::prelude::*;
use pyo3::types::PyBytes;
use tokio::sync::Mutex;

use crate::messaging::{decode_err, duration_from_secs_f64, encode_err, to_py_err};
use crate::runtime::PyNodeRunner;

/// Request side of the NTP-style 4-timestamp exchange.
///
/// Carries `client_send_time` (`t0`) — the client's local clock just before
/// the request goes on the wire. Encoders/decoders use the same capnp wire
/// schema as the Rust [`ClockRequest`].
#[pyclass(name = "ClockRequest", skip_from_py_object)]
#[derive(Clone)]
pub struct PyClockRequest {
    inner: ClockRequest,
}

impl From<ClockRequest> for PyClockRequest {
    fn from(inner: ClockRequest) -> Self {
        Self { inner }
    }
}

#[pymethods]
impl PyClockRequest {
    #[new]
    fn new(client_send_time: u64) -> Self {
        Self {
            inner: ClockRequest::new(client_send_time),
        }
    }

    #[getter]
    fn client_send_time(&self) -> u64 {
        self.inner.client_send_time
    }

    fn encode<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyBytes>> {
        let payload = self
            .inner
            .encode()
            .map_err(|e| encode_err("ClockRequest", e))?;
        Ok(PyBytes::new(py, payload.as_ref()))
    }

    #[staticmethod]
    fn decode(data: &[u8]) -> PyResult<Self> {
        ClockRequest::decode(data)
            .map(Self::from)
            .map_err(|e| decode_err("ClockRequest", e))
    }
}

/// Response side of the NTP-style 4-timestamp exchange.
///
/// Carries the echoed `client_send_time` (`t0`), `server_recv_time` (`t1`),
/// and `server_send_time` (`t2`). `t3` is the client's local time on receive —
/// never on the wire.
#[pyclass(name = "ClockResponse", skip_from_py_object)]
#[derive(Clone)]
pub struct PyClockResponse {
    inner: ClockResponse,
}

impl From<ClockResponse> for PyClockResponse {
    fn from(inner: ClockResponse) -> Self {
        Self { inner }
    }
}

#[pymethods]
impl PyClockResponse {
    #[new]
    fn new(client_send_time: u64, server_recv_time: u64, server_send_time: u64) -> Self {
        Self {
            inner: ClockResponse::new(client_send_time, server_recv_time, server_send_time),
        }
    }

    #[getter]
    fn client_send_time(&self) -> u64 {
        self.inner.client_send_time
    }

    #[getter]
    fn server_recv_time(&self) -> u64 {
        self.inner.server_recv_time
    }

    #[getter]
    fn server_send_time(&self) -> u64 {
        self.inner.server_send_time
    }

    fn encode<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyBytes>> {
        let payload = self
            .inner
            .encode()
            .map_err(|e| encode_err("ClockResponse", e))?;
        Ok(PyBytes::new(py, payload.as_ref()))
    }

    #[staticmethod]
    fn decode(data: &[u8]) -> PyResult<Self> {
        ClockResponse::decode(data)
            .map(Self::from)
            .map_err(|e| decode_err("ClockResponse", e))
    }
}

/// One-way snapshot tick published periodically on the `clock` topic.
///
/// Use [`PyClockResponse`] (the request/response service via
/// [`synchronize`]) when you need to bound staleness with an NTP-style
/// round-trip exchange.
#[pyclass(name = "ClockTick", skip_from_py_object)]
#[derive(Clone)]
pub struct PyClockTick {
    inner: ClockTick,
}

impl From<ClockTick> for PyClockTick {
    fn from(inner: ClockTick) -> Self {
        Self { inner }
    }
}

#[pymethods]
impl PyClockTick {
    #[new]
    fn new(time: u64) -> Self {
        Self {
            inner: ClockTick::new(time),
        }
    }

    #[getter]
    fn time(&self) -> u64 {
        self.inner.time
    }

    fn encode<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyBytes>> {
        let payload = self
            .inner
            .encode()
            .map_err(|e| encode_err("ClockTick", e))?;
        Ok(PyBytes::new(py, payload.as_ref()))
    }

    #[staticmethod]
    fn decode(data: &[u8]) -> PyResult<Self> {
        ClockTick::decode(data)
            .map(Self::from)
            .map_err(|e| decode_err("ClockTick", e))
    }
}

/// Result of an NTP-style clock-sync exchange.
///
/// Mirrors [`peppylib::clock::ClockSync`]. `synchronize` does not
/// adjust the local clock — it only measures.
#[pyclass(name = "ClockSync", skip_from_py_object)]
#[derive(Clone)]
pub struct PyClockSync {
    inner: ClockSync,
}

impl From<ClockSync> for PyClockSync {
    fn from(inner: ClockSync) -> Self {
        Self { inner }
    }
}

#[pymethods]
impl PyClockSync {
    /// `local + offset_ns ≈ core_node`. Signed because the local clock can
    /// lead the core node's clock.
    #[getter]
    fn offset_ns(&self) -> i64 {
        self.inner.offset_ns
    }

    /// Round-trip network delay observed during the exchange.
    #[getter]
    fn round_trip_delay_ns(&self) -> u64 {
        self.inner.round_trip_delay_ns
    }

    /// Raw wire response, exposed for callers that want the individual t0/t1/t2.
    #[getter]
    fn raw(&self) -> PyClockResponse {
        PyClockResponse::from(self.inner.raw.clone())
    }
}

/// Perform an NTP-style clock-sync exchange with `node_runner`'s bound core
/// node.
///
/// Python equivalent of `peppylib::clock::synchronize`.
#[pyfunction]
#[pyo3(name = "synchronize", signature = (node_runner, response_timeout_secs=None))]
fn synchronize_clock<'py>(
    py: Python<'py>,
    node_runner: &PyNodeRunner,
    response_timeout_secs: Option<f64>,
) -> PyResult<Bound<'py, PyAny>> {
    let runner = node_runner.inner.clone();
    let timeout = response_timeout_secs
        .map(|s| duration_from_secs_f64("response_timeout_secs", s))
        .transpose()?;
    crate::py_future::future_into_py(py, async move {
        let sync = synchronize(&runner, timeout).await.map_err(to_py_err)?;
        Ok(PyClockSync::from(sync))
    })
}

/// Long-lived subscription to the periodic `/clock` topic.
///
/// Wraps the raw [`Subscription`] (not the Rust `ClockSubscription` adapter)
/// so the Python lock guards the same inner state as
/// [`crate::messaging::PySubscription`] — no double-mutex layering.
#[pyclass(name = "ClockSubscription")]
pub struct PyClockSubscription {
    inner: Arc<Mutex<Subscription>>,
}

#[pymethods]
impl PyClockSubscription {
    /// Wait for the next tick. Returns `None` if the subscription closes.
    fn on_next_tick<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let inner = Arc::clone(&self.inner);
        crate::py_future::future_into_py(py, async move {
            let mut guard = inner.lock().await;
            match guard.on_next_message().await {
                Some(message) => {
                    let tick = ClockTick::decode(message.payload().as_ref())
                        .map_err(|e| decode_err("ClockTick", e))?;
                    Ok(Some(PyClockTick::from(tick)))
                }
                None => Ok(None),
            }
        })
    }
}

/// Subscribe to the periodic `/clock` topic on `node_runner`'s bound core
/// node.
///
/// Python equivalent of `peppylib::clock::subscribe`.
#[pyfunction]
#[pyo3(name = "subscribe_clock")]
fn subscribe_clock_py<'py>(
    py: Python<'py>,
    node_runner: &PyNodeRunner,
) -> PyResult<Bound<'py, PyAny>> {
    let runner = node_runner.inner.clone();
    crate::py_future::future_into_py(py, async move {
        let sub = subscribe(&runner).await.map_err(to_py_err)?.into_inner();
        Ok(PyClockSubscription {
            inner: Arc::new(Mutex::new(sub)),
        })
    })
}

/// User-facing clock handle. Mirrors
/// [`peppylib::clock::PeppyClock`]: hides whether the node was
/// launched in wall or sim mode and exposes a sync `now_ns()` for hot paths.
///
/// Build via [`clock_for_node_py`]. In sim mode the constructor opens the
/// `clock` subscription up front so subsequent `now_ns()` reads from the
/// in-memory cache.
#[pyclass(name = "PeppyClock")]
pub struct PyPeppyClock {
    inner: PeppyClock,
}

#[pymethods]
impl PyPeppyClock {
    /// Read the current core-node-aligned time in nanoseconds since the
    /// Unix epoch. Raises `RuntimeError` in sim mode if no `ClockTick` has
    /// been observed yet.
    fn now_ns(&self) -> PyResult<u64> {
        self.inner.now_ns().map_err(to_py_err)
    }
}

/// Build a [`PyPeppyClock`] for `node_runner`. Reads the daemon-resolved
/// `framework.use_sim_time` flag and installs the matching backend.
#[pyfunction]
#[pyo3(name = "clock_for_node")]
fn clock_for_node_py<'py>(
    py: Python<'py>,
    node_runner: &PyNodeRunner,
) -> PyResult<Bound<'py, PyAny>> {
    let runner = node_runner.inner.clone();
    crate::py_future::future_into_py(py, async move {
        let clock = for_node(&runner).await.map_err(to_py_err)?;
        Ok(PyPeppyClock { inner: clock })
    })
}

/// Add the clock wire-type wrappers and `synchronize` to the parent
/// `core_node` Python submodule.
pub(crate) fn register_into(module: &Bound<'_, PyModule>) -> PyResult<()> {
    module.add_class::<PyClockRequest>()?;
    module.add_class::<PyClockResponse>()?;
    module.add_class::<PyClockTick>()?;
    module.add_class::<PyClockSync>()?;
    module.add_class::<PyClockSubscription>()?;
    module.add_class::<PyPeppyClock>()?;
    module.add_function(wrap_pyfunction!(synchronize_clock, module)?)?;
    module.add_function(wrap_pyfunction!(subscribe_clock_py, module)?)?;
    module.add_function(wrap_pyfunction!(clock_for_node_py, module)?)?;
    Ok(())
}
