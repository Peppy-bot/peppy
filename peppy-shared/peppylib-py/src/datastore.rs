//! Python bindings for the datastore wire types and the high-level
//! `datastore::store` / `datastore::get` / `datastore::list` /
//! `datastore::remove` helpers.
//!
//! Mirrors `core_node_api::encoding::{DatastoreStoreResponse,
//! DatastoreGetResponse, DatastoreListEntry, DatastoreListResponse,
//! DatastoreRemoveResponse}` and
//! `peppylib::datastore::{StoredValue, DatastoreEntry, store, get, list,
//! remove}`.

use core_node_api::encoding::{
    DatastoreGetResponse, DatastoreListEntry, DatastoreListResponse, DatastoreRemoveResponse,
    DatastoreStoreResponse,
};
use peppylib::datastore::{DatastoreEntry, StoredValue};
use pyo3::prelude::*;
use pyo3::types::PyBytes;

use crate::messaging::{duration_from_secs_f64, encode_err, future_into_py_unit, to_py_err};
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
    fn new(found: bool, value: Vec<u8>, encoding: String, last_modified_by: String) -> Self {
        Self {
            inner: DatastoreGetResponse {
                found,
                value,
                encoding,
                last_modified_by,
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

    #[getter]
    fn last_modified_by(&self) -> &str {
        &self.inner.last_modified_by
    }

    fn encode<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyBytes>> {
        let payload = self
            .inner
            .encode()
            .map_err(|e| encode_err("DatastoreGetResponse", e))?;
        Ok(PyBytes::new(py, payload.as_ref()))
    }
}

/// Python wrapper for `core_node_api::encoding::DatastoreListResponse` — used by
/// test stubs to produce capnp wire bytes for the `DATASTORE_LIST` service.
/// Construct it from `(key, encoding, last_modified_by)` triples.
#[pyclass(name = "DatastoreListResponse", skip_from_py_object)]
#[derive(Clone)]
pub struct PyDatastoreListResponse {
    inner: DatastoreListResponse,
}

#[pymethods]
impl PyDatastoreListResponse {
    #[new]
    fn new(entries: Vec<(String, String, String)>) -> Self {
        let entries = entries
            .into_iter()
            .map(|(key, encoding, last_modified_by)| DatastoreListEntry {
                key,
                encoding,
                last_modified_by,
            })
            .collect();
        Self {
            inner: DatastoreListResponse { entries },
        }
    }

    fn encode<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyBytes>> {
        let payload = self
            .inner
            .encode()
            .map_err(|e| encode_err("DatastoreListResponse", e))?;
        Ok(PyBytes::new(py, payload.as_ref()))
    }
}

/// Python wrapper for `core_node_api::encoding::DatastoreRemoveResponse` — used
/// by test stubs to produce capnp wire bytes for the `DATASTORE_REMOVE` service.
#[pyclass(name = "DatastoreRemoveResponse", skip_from_py_object)]
#[derive(Clone)]
pub struct PyDatastoreRemoveResponse {
    inner: DatastoreRemoveResponse,
}

#[pymethods]
impl PyDatastoreRemoveResponse {
    #[new]
    fn new(removed: bool) -> Self {
        Self {
            inner: DatastoreRemoveResponse { removed },
        }
    }

    #[getter]
    fn removed(&self) -> bool {
        self.inner.removed
    }

    fn encode<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyBytes>> {
        let payload = self
            .inner
            .encode()
            .map_err(|e| encode_err("DatastoreRemoveResponse", e))?;
        Ok(PyBytes::new(py, payload.as_ref()))
    }
}

/// Python wrapper for `peppylib::datastore::StoredValue` — the
/// value returned by [`datastore::get`]: the raw bytes plus their Zenoh-style
/// encoding tag. The `encoding` getter returns a plain `str` (the open set of
/// tags means an arbitrary value may come back); it compares equal to the
/// `peppylib.Encoding` members.
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
        self.inner.encoding.as_str()
    }

    #[getter]
    fn last_modified_by(&self) -> &str {
        &self.inner.last_modified_by
    }
}

/// Python wrapper for `peppylib::datastore::DatastoreEntry` — one
/// key's metadata returned by [`datastore::list`]: its key, the encoding tag of
/// its value, and the `instance_id` of the node that last wrote it. The value
/// bytes are not included; fetch them with [`datastore::get`].
#[pyclass(name = "DatastoreEntry", skip_from_py_object)]
#[derive(Clone)]
pub struct PyDatastoreEntry {
    inner: DatastoreEntry,
}

impl From<DatastoreEntry> for PyDatastoreEntry {
    fn from(inner: DatastoreEntry) -> Self {
        Self { inner }
    }
}

#[pymethods]
impl PyDatastoreEntry {
    #[getter]
    fn key(&self) -> &str {
        &self.inner.key
    }

    #[getter]
    fn encoding(&self) -> &str {
        self.inner.encoding.as_str()
    }

    #[getter]
    fn last_modified_by(&self) -> &str {
        &self.inner.last_modified_by
    }
}

/// Store `value` under `key` (tagged with `encoding`) on `node_runner`'s
/// bound core node. Overwrites any existing value for `key`.
///
/// Python equivalent of `peppylib::datastore::store`.
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
    // `future_into_py_unit` resolves to Python `None` — a store that succeeds
    // has nothing to return (see the helper for why a bare `Ok(())` would not).
    future_into_py_unit(py, async move {
        peppylib::datastore::store(&runner, key, value, encoding, timeout)
            .await
            .map_err(to_py_err)?;
        Ok(())
    })
}

/// Retrieve the value stored under `key` from `node_runner`'s bound core
/// node. Resolves to `None` when no value is stored for `key`.
///
/// Python equivalent of `peppylib::datastore::get`.
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
    crate::py_future::future_into_py(py, async move {
        let value = peppylib::datastore::get(&runner, key, timeout)
            .await
            .map_err(to_py_err)?;
        Ok(value.map(PyStoredValue::from))
    })
}

/// List the metadata of every key in `node_runner`'s bound core node datastore.
/// Resolves to a list of `DatastoreEntry` (key, encoding, last_modified_by) —
/// the value bytes are not included; fetch them with `datastore::get`.
///
/// Python equivalent of `peppylib::datastore::list`.
#[pyfunction]
#[pyo3(signature = (node_runner, response_timeout_secs=None))]
fn datastore_list<'py>(
    py: Python<'py>,
    node_runner: &PyNodeRunner,
    response_timeout_secs: Option<f64>,
) -> PyResult<Bound<'py, PyAny>> {
    let runner = node_runner.inner.clone();
    let timeout = response_timeout_secs
        .map(|s| duration_from_secs_f64("response_timeout_secs", s))
        .transpose()?;
    crate::py_future::future_into_py(py, async move {
        let entries = peppylib::datastore::list(&runner, timeout)
            .await
            .map_err(to_py_err)?;
        Ok(entries
            .into_iter()
            .map(PyDatastoreEntry::from)
            .collect::<Vec<_>>())
    })
}

/// Remove (unset) `key` from `node_runner`'s bound core node. Resolves to `True`
/// if the key existed and was removed, `False` if it was already absent.
///
/// Python equivalent of `peppylib::datastore::remove`.
#[pyfunction]
#[pyo3(signature = (node_runner, key, response_timeout_secs=None))]
fn datastore_remove<'py>(
    py: Python<'py>,
    node_runner: &PyNodeRunner,
    key: String,
    response_timeout_secs: Option<f64>,
) -> PyResult<Bound<'py, PyAny>> {
    let runner = node_runner.inner.clone();
    let timeout = response_timeout_secs
        .map(|s| duration_from_secs_f64("response_timeout_secs", s))
        .transpose()?;
    crate::py_future::future_into_py(py, async move {
        let removed = peppylib::datastore::remove(&runner, key, timeout)
            .await
            .map_err(to_py_err)?;
        Ok(removed)
    })
}

/// Add the datastore wire-type wrappers and the `datastore::store` /
/// `datastore::get` / `datastore::list` / `datastore::remove` helpers to the
/// parent `core_node` Python submodule.
pub(crate) fn register_into(module: &Bound<'_, PyModule>) -> PyResult<()> {
    module.add_class::<PyDatastoreStoreResponse>()?;
    module.add_class::<PyDatastoreGetResponse>()?;
    module.add_class::<PyDatastoreListResponse>()?;
    module.add_class::<PyDatastoreRemoveResponse>()?;
    module.add_class::<PyStoredValue>()?;
    module.add_class::<PyDatastoreEntry>()?;
    module.add_function(wrap_pyfunction!(datastore_store, module)?)?;
    module.add_function(wrap_pyfunction!(datastore_get, module)?)?;
    module.add_function(wrap_pyfunction!(datastore_list, module)?)?;
    module.add_function(wrap_pyfunction!(datastore_remove, module)?)?;
    Ok(())
}
