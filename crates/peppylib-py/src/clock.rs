//! Python bindings for the `clock` wire types and the high-level
//! `synchronize` helper.
//!
//! Mirrors `core_node_api::encoding::clock::{ClockRequest, ClockResponse,
//! ClockSource, ClockTick}` and `peppylib::core_node::clock::{ClockSync,
//! synchronize}`.

use core_node_api::encoding::{ClockRequest, ClockResponse, ClockSource, ClockTick};
use peppylib::core_node::clock::{ClockSync, synchronize};
use pyo3::exceptions::{PyRuntimeError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::PyBytes;

use crate::messaging::{duration_from_secs_f64, to_py_err};
use crate::runtime::PyNodeRunner;

fn encode_err(what: &str, err: core_node_api::Error) -> PyErr {
    PyRuntimeError::new_err(format!("failed to encode {what}: {err}"))
}

fn decode_err(what: &str, err: core_node_api::Error) -> PyErr {
    PyValueError::new_err(format!("failed to decode {what}: {err}"))
}

/// Which clock source the core node served a tick or response from.
///
/// Today only `Wall` is emitted; `Sim` and `Replay` are reserved for a future
/// `use_sim_time`-equivalent and are not constructable on the wire.
#[pyclass(name = "ClockSource", eq, eq_int, from_py_object)]
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum PyClockSource {
    Wall,
}

impl From<PyClockSource> for ClockSource {
    fn from(py: PyClockSource) -> Self {
        match py {
            PyClockSource::Wall => ClockSource::Wall,
        }
    }
}

impl From<ClockSource> for PyClockSource {
    fn from(src: ClockSource) -> Self {
        match src {
            ClockSource::Wall => PyClockSource::Wall,
        }
    }
}

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
/// `server_send_time` (`t2`), and the `clock_source` the server stamped from.
/// `t3` is the client's local time on receive — never on the wire.
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
    fn new(
        client_send_time: u64,
        server_recv_time: u64,
        server_send_time: u64,
        clock_source: PyClockSource,
    ) -> Self {
        Self {
            inner: ClockResponse::new(
                client_send_time,
                server_recv_time,
                server_send_time,
                clock_source.into(),
            ),
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

    #[getter]
    fn clock_source(&self) -> PyClockSource {
        self.inner.clock_source.into()
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
    fn new(time: u64, clock_source: PyClockSource) -> Self {
        Self {
            inner: ClockTick::new(time, clock_source.into()),
        }
    }

    #[getter]
    fn time(&self) -> u64 {
        self.inner.time
    }

    #[getter]
    fn clock_source(&self) -> PyClockSource {
        self.inner.clock_source.into()
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
/// Mirrors [`peppylib::core_node::clock::ClockSync`]. `synchronize` does not
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

    #[getter]
    fn clock_source(&self) -> PyClockSource {
        self.inner.clock_source.into()
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
/// Python equivalent of `peppylib::core_node::clock::synchronize`.
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
    pyo3_async_runtimes::tokio::future_into_py(py, async move {
        let sync = synchronize(&runner, timeout).await.map_err(to_py_err)?;
        Ok(PyClockSync::from(sync))
    })
}

/// Add the clock wire-type wrappers and `synchronize` to the parent
/// `core_node` Python submodule.
pub(crate) fn register_into(module: &Bound<'_, PyModule>) -> PyResult<()> {
    module.add_class::<PyClockSource>()?;
    module.add_class::<PyClockRequest>()?;
    module.add_class::<PyClockResponse>()?;
    module.add_class::<PyClockTick>()?;
    module.add_class::<PyClockSync>()?;
    module.add_function(wrap_pyfunction!(synchronize_clock, module)?)?;
    Ok(())
}
