//! Python bindings for the datastore wire types and the high-level
//! `datastore_store` / `datastore_get` helpers.
//!
//! Mirrors `core_node_api::encoding::{DatastoreStoreResponse,
//! DatastoreGetResponse}` and
//! `peppylib::core_node::datastore::{StoredValue, datastore_store, datastore_get}`.

use core_node_api::encoding::{DatastoreGetResponse, DatastoreStoreResponse};
use peppylib::core_node::datastore::StoredValue;
use pyo3::prelude::*;
use pyo3::types::PyBytes;

use crate::messaging::{duration_from_secs_f64, encode_err, to_py_err};
use crate::runtime::PyNodeRunner;

/// Python wrapper for `core_node_api::encoding::DatastoreStoreResponse` — an
/// empty ack. Exposes `encode()` so test stubs can produce wire bytes.
#[pyclass(name = "DatastoreStoreResponse", skip_from_py_object)]
#[derive(Clone)]
pub struct PyDatastoreStoreResponse;

#[pymethods]
impl PyDatastoreStoreResponse {
    #[new]
    fn new() -> Self {
        Self
    }

    fn encode<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyBytes>> {
        let payload = DatastoreStoreResponse::new()
            .encode()
            .map_err(|e| encode_err("DatastoreStoreResponse", e))?;
        Ok(PyBytes::new(py, payload.as_ref()))
    }
}

/// Python wrapper for `core_node_api::encoding::DatastoreGetResponse` — used
/// by test stubs to produce capnp wire bytes for the `DATASTORE_GET` service.
#[pyclass(name = "DatastoreGetResponse", skip_from_py_object)]
#[derive(Clone)]
pub struct PyDatastoreGetResponse {
    inner: DatastoreGetResponse,
}

#[pymethods]
impl PyDatastoreGetResponse {
    #[new]
    fn new(found: bool, value: Vec<u8>, encoding: String) -> Self {
        Self {
            inner: DatastoreGetResponse {
                found,
                value,
                encoding,
            },
        }
    }

    #[getter]
    fn found(&self) -> bool {
        self.inner.found
    }

    #[getter]
    fn value<'py>(&self, py: Python<'py>) -> Bound<'py, PyBytes> {
        PyBytes::new(py, &self.inner.value)
    }

    #[getter]
    fn encoding(&self) -> &str {
        &self.inner.encoding
    }

    fn encode<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyBytes>> {
        let payload = self
            .inner
            .encode()
            .map_err(|e| encode_err("DatastoreGetResponse", e))?;
        Ok(PyBytes::new(py, payload.as_ref()))
    }
}

/// Python wrapper for `peppylib::core_node::datastore::StoredValue` — the
/// value returned by [`datastore_get`]: the raw bytes plus their Zenoh-style
/// encoding tag.
#[pyclass(name = "StoredValue", skip_from_py_object)]
#[derive(Clone)]
pub struct PyStoredValue {
    inner: StoredValue,
}

impl From<StoredValue> for PyStoredValue {
    fn from(inner: StoredValue) -> Self {
        Self { inner }
    }
}

#[pymethods]
impl PyStoredValue {
    #[getter]
    fn value<'py>(&self, py: Python<'py>) -> Bound<'py, PyBytes> {
        PyBytes::new(py, &self.inner.value)
    }

    #[getter]
    fn encoding(&self) -> &str {
        &self.inner.encoding
    }
}

/// Store `value` under `key` (tagged with `encoding`) on `node_runner`'s
/// bound core node. Overwrites any existing value for `key`.
///
/// Python equivalent of `peppylib::datastore_store`.
#[pyfunction]
#[pyo3(signature = (node_runner, key, value, encoding, response_timeout_secs=None))]
fn datastore_store<'py>(
    py: Python<'py>,
    node_runner: &PyNodeRunner,
    key: String,
    value: Vec<u8>,
    encoding: String,
    response_timeout_secs: Option<f64>,
) -> PyResult<Bound<'py, PyAny>> {
    let runner = node_runner.inner.clone();
    let timeout = response_timeout_secs
        .map(|s| duration_from_secs_f64("response_timeout_secs", s))
        .transpose()?;
    pyo3_async_runtimes::tokio::future_into_py(py, async move {
        peppylib::datastore_store(&runner, key, value, encoding, timeout)
            .await
            .map_err(to_py_err)?;
        // Resolve to Python `None` (not the empty tuple that a bare `Ok(())`
        // produces under PyO3 0.28's `IntoPyObject`) — a store that succeeds
        // has nothing to return, and `None` is the Pythonic contract for that.
        Ok(Python::attach(|py| py.None()))
    })
}

/// Retrieve the value stored under `key` from `node_runner`'s bound core
/// node. Resolves to `None` when no value is stored for `key`.
///
/// Python equivalent of `peppylib::datastore_get`.
#[pyfunction]
#[pyo3(signature = (node_runner, key, response_timeout_secs=None))]
fn datastore_get<'py>(
    py: Python<'py>,
    node_runner: &PyNodeRunner,
    key: String,
    response_timeout_secs: Option<f64>,
) -> PyResult<Bound<'py, PyAny>> {
    let runner = node_runner.inner.clone();
    let timeout = response_timeout_secs
        .map(|s| duration_from_secs_f64("response_timeout_secs", s))
        .transpose()?;
    pyo3_async_runtimes::tokio::future_into_py(py, async move {
        let value = peppylib::datastore_get(&runner, key, timeout)
            .await
            .map_err(to_py_err)?;
        Ok(value.map(PyStoredValue::from))
    })
}

/// Add the datastore wire-type wrappers and `datastore_store` / `datastore_get`
/// to the parent `core_node` Python submodule.
pub(crate) fn register_into(module: &Bound<'_, PyModule>) -> PyResult<()> {
    module.add_class::<PyDatastoreStoreResponse>()?;
    module.add_class::<PyDatastoreGetResponse>()?;
    module.add_class::<PyStoredValue>()?;
    module.add_function(wrap_pyfunction!(datastore_store, module)?)?;
    module.add_function(wrap_pyfunction!(datastore_get, module)?)?;
    Ok(())
}
