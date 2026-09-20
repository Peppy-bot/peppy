use peppylib::messaging::{
    ContractIdentifier, NodeIdentifier, PairingIdentifier, ProducerRef, SenderTarget,
    SenderTargetError,
};
use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;

fn sender_target_error_to_py(err: SenderTargetError) -> PyErr {
    PyValueError::new_err(err.to_string())
}

/// Python wrapper for [`SenderTarget`]. Mirrors the Rust API: construct via
/// the `node(name, tag)` / `contract(name, tag)` static methods. Each
/// emission addresses either a node or a contract — never both. The wire
/// format embeds a `contract` / `node` discriminator so the two namespaces
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

    /// Build a contract-shaped target. Used for topics / services / actions
    /// backed by a `manifest.implements` slot. Raises `ValueError` if either
    /// segment fails validation.
    #[staticmethod]
    fn contract(name: &str, tag: &str) -> PyResult<Self> {
        ContractIdentifier::new(name, tag)
            .map(|inner| Self {
                inner: SenderTarget::Contract(inner),
            })
            .map_err(sender_target_error_to_py)
    }

    /// Build a pairing-shaped target. Used for topics exchanged over a
    /// `depends_on.pairings` slot. Raises `ValueError` if either segment
    /// fails validation.
    #[staticmethod]
    fn pairing(name: &str, tag: &str) -> PyResult<Self> {
        PairingIdentifier::new(name, tag)
            .map(|inner| Self {
                inner: SenderTarget::Pairing(inner),
            })
            .map_err(sender_target_error_to_py)
    }

    #[getter]
    fn is_node(&self) -> bool {
        self.inner.is_node()
    }

    #[getter]
    fn is_contract(&self) -> bool {
        self.inner.is_contract()
    }

    #[getter]
    fn is_pairing(&self) -> bool {
        self.inner.is_pairing()
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
            SenderTarget::Contract(_) => {
                format!(
                    "SenderTarget.contract({:?}, {:?})",
                    self.inner.name(),
                    self.inner.tag()
                )
            }
            SenderTarget::Pairing(_) => {
                format!(
                    "SenderTarget.pairing({:?}, {:?})",
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

/// Python wrapper for [`ProducerRef`] — the publisher's full
/// `(core_node, instance_id)` wire identity. Returned alongside every consumed
/// message, and accepted by every producer-targeting call site (topic
/// subscribe, service poll, action send_goal) so a consumer can pass the
/// identity it received straight back. `instance_id` alone is only unique
/// within one stack, so the pair is what distinguishes producers across the
/// whole mesh; consumers key per-slot state on it.
/// `frozen, eq, hash` make it usable directly as a `dict` key, mirroring the
/// Rust `HashMap<ProducerRef, _>` idiom. `from_py_object` lets it be extracted
/// as a call argument.
#[pyclass(name = "ProducerRef", frozen, eq, hash, from_py_object)]
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct PyProducerRef {
    pub(crate) inner: ProducerRef,
}

#[pymethods]
impl PyProducerRef {
    #[new]
    pub(crate) fn new(core_node: String, instance_id: String) -> Self {
        Self {
            inner: ProducerRef::new(core_node, instance_id),
        }
    }

    #[getter]
    fn core_node(&self) -> &str {
        &self.inner.core_node
    }

    #[getter]
    fn instance_id(&self) -> &str {
        &self.inner.instance_id
    }

    fn __repr__(&self) -> String {
        format!(
            "ProducerRef({:?}, {:?})",
            self.inner.core_node, self.inner.instance_id
        )
    }
}

impl PyProducerRef {
    pub(crate) fn into_inner(self) -> ProducerRef {
        self.inner
    }

    pub(crate) fn as_inner(&self) -> &ProducerRef {
        &self.inner
    }
}

impl From<ProducerRef> for PyProducerRef {
    fn from(inner: ProducerRef) -> Self {
        Self { inner }
    }
}
