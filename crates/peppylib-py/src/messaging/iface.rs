use peppylib::messaging::{InterfaceIdentifier, NodeIdentifier, SenderTarget, SenderTargetError};
use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;

fn sender_target_error_to_py(err: SenderTargetError) -> PyErr {
    PyValueError::new_err(err.to_string())
}

/// Python wrapper for [`SenderTarget`]. Mirrors the Rust API: construct via
/// the `node(name, tag)` / `interface(name, tag)` static methods. Each
/// emission addresses either a node or an interface — never both. The wire
/// format embeds an `interface` / `node` discriminator so the two namespaces
/// cannot collide.
///
/// Subscribers that should match any publisher pass `None` for `from_target`
/// in `subscribe()` rather than constructing a wildcard `SenderTarget`.
#[pyclass(name = "SenderTarget", frozen, eq, hash, from_py_object)]
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct PySenderTarget {
    pub(crate) inner: SenderTarget,
}

#[pymethods]
impl PySenderTarget {
    /// Build a node-shaped target. `name` is the node's `manifest.name`,
    /// `tag` is the node's `manifest.tag`. Raises `ValueError` if either
    /// segment fails validation (empty, contains `/`, or collides with a
    /// reserved sentinel).
    #[staticmethod]
    fn node(name: &str, tag: &str) -> PyResult<Self> {
        NodeIdentifier::new(name, tag)
            .map(|inner| Self {
                inner: SenderTarget::Node(inner),
            })
            .map_err(sender_target_error_to_py)
    }

    /// Build an interface-shaped target. Used for topics / services / actions
    /// pulled in via `interfaces.conforms_to`. Raises `ValueError` if either
    /// segment fails validation.
    #[staticmethod]
    fn interface(name: &str, tag: &str) -> PyResult<Self> {
        InterfaceIdentifier::new(name, tag)
            .map(|inner| Self {
                inner: SenderTarget::Interface(inner),
            })
            .map_err(sender_target_error_to_py)
    }

    #[getter]
    fn is_node(&self) -> bool {
        self.inner.is_node()
    }

    #[getter]
    fn is_interface(&self) -> bool {
        self.inner.is_interface()
    }

    #[getter]
    fn name(&self) -> &str {
        self.inner.name()
    }

    #[getter]
    fn tag(&self) -> &str {
        self.inner.tag()
    }

    fn __repr__(&self) -> String {
        match &self.inner {
            SenderTarget::Node(_) => {
                format!(
                    "SenderTarget.node({:?}, {:?})",
                    self.inner.name(),
                    self.inner.tag()
                )
            }
            SenderTarget::Interface(_) => {
                format!(
                    "SenderTarget.interface({:?}, {:?})",
                    self.inner.name(),
                    self.inner.tag()
                )
            }
        }
    }
}

impl PySenderTarget {
    pub(crate) fn into_inner(self) -> SenderTarget {
        self.inner
    }
}
