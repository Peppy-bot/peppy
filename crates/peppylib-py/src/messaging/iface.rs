use peppylib::messaging::{Iface, IfaceError};
use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;

fn iface_error_to_py(err: IfaceError) -> PyErr {
    PyValueError::new_err(err.to_string())
}

/// Python wrapper for [`Iface`]. Mirrors the Rust API: construct via the
/// `native()` / `wildcard()` / `conformed()` static methods.
///
/// The wire-level segment markers used by the zenoh transport (`_` for native,
/// `*` for wildcard) live inside `pmi::wire::zenoh_format` and never appear in
/// user or generator-emitted Python code. Callers that need a non-`conforms_to`
/// artifact use `Iface.native()`; subscribers that should match any publisher
/// iface use `Iface.wildcard()`.
#[pyclass(name = "Iface", frozen, eq, hash, from_py_object)]
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct PyIface {
    pub(crate) inner: Iface,
}

#[pymethods]
impl PyIface {
    /// Iface for an artifact that is not part of a `conforms_to` interface.
    #[staticmethod]
    fn native() -> Self {
        Self {
            inner: Iface::native(),
        }
    }

    /// Iface that matches any publisher's iface segments. Used on the consumer
    /// side when the deployment config doesn't pin the producer's iface.
    #[staticmethod]
    fn wildcard() -> Self {
        Self {
            inner: Iface::wildcard(),
        }
    }

    /// Iface pulled in via `interfaces.conforms_to`, identified by `name` /
    /// `tag`. Raises `ValueError` if either segment fails validation (empty,
    /// contains `/`, or collides with a reserved sentinel).
    #[staticmethod]
    fn conformed(name: &str, tag: &str) -> PyResult<Self> {
        Iface::new(name, tag)
            .map(|inner| Self { inner })
            .map_err(iface_error_to_py)
    }

    #[getter]
    fn is_native(&self) -> bool {
        self.inner.is_native()
    }

    fn __repr__(&self) -> String {
        match &self.inner {
            Iface::Native => "Iface.native()".to_string(),
            Iface::Wildcard => "Iface.wildcard()".to_string(),
            Iface::Conformed { .. } => {
                format!(
                    "Iface.conformed({:?}, {:?})",
                    self.inner.name(),
                    self.inner.tag()
                )
            }
        }
    }
}

impl PyIface {
    pub(crate) fn into_inner(self) -> Iface {
        self.inner
    }
}
