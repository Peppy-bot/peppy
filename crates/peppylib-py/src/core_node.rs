//! Python bindings for the `core_node` high-level service wrappers.
//!
//! Mirrors `pub use core_node::{info::info, stack};` from
//! `crates/peppylib/src/lib.rs`: takes a `NodeRunner` directly and returns
//! fully-typed responses.

use std::time::Duration;

use core_node_api::SerializedNodeGraph;
use core_node_api::encoding::{ContainerInfo, InfoResponse, StackListResponse};
use pyo3::exceptions::{PyKeyError, PyRuntimeError};
use pyo3::prelude::*;
use pyo3::types::PyBytes;
use pythonize::pythonize;

use crate::messaging::{duration_from_secs_f64, encode_err, to_py_err};
use crate::runtime::PyNodeRunner;

fn optional_timeout(arg_name: &str, secs: Option<f64>) -> PyResult<Option<Duration>> {
    secs.map(|s| duration_from_secs_f64(arg_name, s))
        .transpose()
}

/// Python wrapper for `core_node_api::encoding::ContainerInfo`.
#[pyclass(name = "ContainerInfo", from_py_object)]
#[derive(Clone)]
pub struct PyContainerInfo {
    inner: ContainerInfo,
}

#[pymethods]
impl PyContainerInfo {
    #[new]
    fn new(apptainer_version: String, lima_version: String) -> Self {
        Self {
            inner: ContainerInfo {
                apptainer_version,
                lima_version,
            },
        }
    }

    #[getter]
    fn apptainer_version(&self) -> &str {
        &self.inner.apptainer_version
    }

    #[getter]
    fn lima_version(&self) -> &str {
        &self.inner.lima_version
    }
}

/// Python wrapper for `core_node_api::encoding::InfoResponse`.
///
/// Exposes the same fields as the Rust type plus `encode()` so tests can
/// fabricate capnp wire bytes for stub listeners.
#[pyclass(name = "InfoResponse", skip_from_py_object)]
#[derive(Clone)]
pub struct PyInfoResponse {
    inner: InfoResponse,
}

impl From<InfoResponse> for PyInfoResponse {
    fn from(inner: InfoResponse) -> Self {
        Self { inner }
    }
}

#[pymethods]
impl PyInfoResponse {
    #[new]
    #[allow(clippy::too_many_arguments)]
    fn new(
        uptime_secs: u64,
        core_node_name: String,
        core_node_instance_id: String,
        host_name: String,
        node_count: u32,
        git_version: String,
        container_info: PyContainerInfo,
        messaging_port: u16,
    ) -> Self {
        Self {
            inner: InfoResponse {
                uptime_secs,
                core_node_name,
                core_node_instance_id,
                host_name,
                node_count,
                git_version,
                container_info: container_info.inner,
                messaging_port,
            },
        }
    }

    #[getter]
    fn uptime_secs(&self) -> u64 {
        self.inner.uptime_secs
    }

    #[getter]
    fn core_node_name(&self) -> &str {
        &self.inner.core_node_name
    }

    #[getter]
    fn core_node_instance_id(&self) -> &str {
        &self.inner.core_node_instance_id
    }

    #[getter]
    fn host_name(&self) -> &str {
        &self.inner.host_name
    }

    #[getter]
    fn node_count(&self) -> u32 {
        self.inner.node_count
    }

    #[getter]
    fn git_version(&self) -> &str {
        &self.inner.git_version
    }

    #[getter]
    fn container_info(&self) -> PyContainerInfo {
        PyContainerInfo {
            inner: self.inner.container_info.clone(),
        }
    }

    #[getter]
    fn messaging_port(&self) -> u16 {
        self.inner.messaging_port
    }

    /// Capnp-encode this response into bytes suitable for replying from a stub
    /// service listener.
    fn encode<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyBytes>> {
        let payload = self
            .inner
            .encode()
            .map_err(|e| encode_err("InfoResponse", e))?;
        Ok(PyBytes::new(py, payload.as_ref()))
    }
}

/// Python wrapper for `core_node_api::encoding::StackListResponse` — used by
/// test stubs to produce capnp wire bytes for the `STACK_LIST` service.
#[pyclass(name = "StackListResponse", skip_from_py_object)]
#[derive(Clone)]
pub struct PyStackListResponse {
    inner: StackListResponse,
}

#[pymethods]
impl PyStackListResponse {
    #[new]
    #[pyo3(signature = (graph_json, dot_graph=None))]
    fn new(graph_json: String, dot_graph: Option<String>) -> Self {
        Self {
            inner: StackListResponse::new(dot_graph, graph_json),
        }
    }

    #[getter]
    fn graph_json(&self) -> &str {
        &self.inner.graph_json
    }

    #[getter]
    fn dot_graph(&self) -> Option<&str> {
        self.inner.dot_graph.as_deref()
    }

    fn encode<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyBytes>> {
        let payload = self
            .inner
            .encode()
            .map_err(|e| encode_err("StackListResponse", e))?;
        Ok(PyBytes::new(py, payload.as_ref()))
    }
}

/// Python wrapper for `peppylib::stack::StackList`.
///
/// `graph` is returned as a Python dict via `pythonize` — the nested
/// `SerializedNodeGraph` types all derive `Serialize`/`Deserialize`, so a
/// dict/list representation faithfully mirrors the Rust shape without hand-
/// wrapping six more classes.
#[pyclass(name = "StackList")]
pub struct PyStackList {
    graph: SerializedNodeGraph,
    dot_graph: Option<String>,
}

#[pymethods]
impl PyStackList {
    #[getter]
    fn graph<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        pythonize(py, &self.graph).map_err(|e| {
            PyRuntimeError::new_err(format!("failed to convert graph to Python dict: {e}"))
        })
    }

    #[getter]
    fn dot_graph(&self) -> Option<&str> {
        self.dot_graph.as_deref()
    }

    /// Externally visible instance ids for the node identified by
    /// `(node_name, node_tag)`. Raises `KeyError` when no node matches;
    /// returns an empty list when the node exists but every instance is
    /// still `starting`. Mirrors
    /// `SerializedNodeGraph::running_instance_ids_by_node` on the Rust side.
    fn running_instance_ids_by_node(
        &self,
        node_name: &str,
        node_tag: &str,
    ) -> PyResult<Vec<String>> {
        self.graph
            .running_instance_ids_by_node(node_name, node_tag)
            .map(|ids| ids.into_iter().map(str::to_owned).collect())
            .map_err(|err| PyKeyError::new_err(err.to_string()))
    }
}

/// Poll the `INFO` service for `node_runner`'s bound core node.
///
/// Python equivalent of `peppylib::info`.
#[pyfunction]
#[pyo3(signature = (node_runner, response_timeout_secs=None))]
fn info<'py>(
    py: Python<'py>,
    node_runner: &PyNodeRunner,
    response_timeout_secs: Option<f64>,
) -> PyResult<Bound<'py, PyAny>> {
    let runner = node_runner.inner.clone();
    let timeout = optional_timeout("response_timeout_secs", response_timeout_secs)?;
    crate::py_future::future_into_py(py, async move {
        let response = peppylib::info(&runner, timeout).await.map_err(to_py_err)?;
        Ok(PyInfoResponse::from(response))
    })
}

/// Poll the `STACK_LIST` service for `node_runner`'s bound core node.
///
/// Python equivalent of `peppylib::stack::list`.
#[pyfunction]
#[pyo3(signature = (node_runner, with_dot_graph, response_timeout_secs=None))]
fn stack_list<'py>(
    py: Python<'py>,
    node_runner: &PyNodeRunner,
    with_dot_graph: bool,
    response_timeout_secs: Option<f64>,
) -> PyResult<Bound<'py, PyAny>> {
    let runner = node_runner.inner.clone();
    let timeout = optional_timeout("response_timeout_secs", response_timeout_secs)?;
    crate::py_future::future_into_py(py, async move {
        let result = peppylib::stack::list(&runner, with_dot_graph, timeout)
            .await
            .map_err(to_py_err)?;
        Ok(PyStackList {
            graph: result.graph,
            dot_graph: result.dot_graph,
        })
    })
}

/// Register the `core_node` submodule.
pub(crate) fn register(parent_module: &Bound<'_, PyModule>) -> PyResult<()> {
    let module = PyModule::new(parent_module.py(), "core_node")?;
    module.add_class::<PyContainerInfo>()?;
    module.add_class::<PyInfoResponse>()?;
    module.add_class::<PyStackListResponse>()?;
    module.add_class::<PyStackList>()?;
    module.add_function(wrap_pyfunction!(info, &module)?)?;
    module.add_function(wrap_pyfunction!(stack_list, &module)?)?;
    crate::clock::register_into(&module)?;
    crate::datastore::register_into(&module)?;
    parent_module.add_submodule(&module)?;
    Ok(())
}
