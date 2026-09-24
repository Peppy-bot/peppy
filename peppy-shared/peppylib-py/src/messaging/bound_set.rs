//! The member of a bound producer set, as the generated `bound_members()`
//! accessors of `one_or_more` and `zero_or_more` slots answer it.

use super::PyProducerRef;
use peppylib::messaging::BoundMember;
use pyo3::prelude::*;

/// The copy an instance belongs to: the copy's name, and the id the copy's
/// fragment wrote for the instance, which the copy runs as
/// `<name>_<instance_id>`.
#[pyclass(name = "CopyTag", frozen, eq, hash, skip_from_py_object)]
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct PyCopyTag {
    pub(crate) inner: config::runtime::CopyTag,
}

#[pymethods]
impl PyCopyTag {
    #[new]
    fn new(name: &str, instance_id: &str) -> PyResult<Self> {
        let named = |value: &str| {
            config::runtime::Name::new(value)
                .map_err(|e| pyo3::exceptions::PyValueError::new_err(e.to_string()))
        };
        Ok(Self {
            inner: config::runtime::CopyTag {
                name: named(name)?,
                instance_id: named(instance_id)?,
            },
        })
    }

    /// The copy's name.
    #[getter]
    fn name(&self) -> &str {
        self.inner.name.as_str()
    }

    /// The id the copy's fragment wrote for the instance.
    #[getter]
    fn instance_id(&self) -> &str {
        self.inner.instance_id.as_str()
    }

    fn __repr__(&self) -> String {
        format!(
            "CopyTag({:?}, {:?})",
            self.inner.name.as_str(),
            self.inner.instance_id.as_str()
        )
    }
}

/// One member of a consumer slot's bound set: the producer and the copy its
/// instance belongs to (`None` for an instance the launcher deploys outside
/// any copy). A node holding members from several copies groups them by
/// `copy.name` and names each one by `copy.instance_id`.
#[pyclass(name = "BoundMember", frozen, eq, hash, skip_from_py_object)]
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct PyBoundMember {
    pub(crate) inner: BoundMember,
}

#[pymethods]
impl PyBoundMember {
    #[new]
    #[pyo3(signature = (producer, copy=None))]
    fn new(producer: &PyProducerRef, copy: Option<&PyCopyTag>) -> PyResult<Self> {
        Ok(Self {
            inner: BoundMember {
                producer: producer.as_inner().clone(),
                copy: copy.map(|copy| copy.inner.clone()),
            },
        })
    }

    /// The member's producer: its full wire address.
    #[getter]
    fn producer(&self) -> PyProducerRef {
        PyProducerRef::from(self.inner.producer.clone())
    }

    /// The copy the producer's instance belongs to, or `None`.
    #[getter]
    fn copy(&self) -> Option<PyCopyTag> {
        self.inner.copy.as_ref().map(|inner| PyCopyTag {
            inner: inner.clone(),
        })
    }

    fn __repr__(&self) -> String {
        format!(
            "BoundMember(producer={}, copy={})",
            PyProducerRef::from(self.inner.producer.clone()).__repr__(),
            self.copy()
                .map(|copy| copy.__repr__())
                .unwrap_or_else(|| "None".to_string())
        )
    }
}

impl From<BoundMember> for PyBoundMember {
    fn from(inner: BoundMember) -> Self {
        Self { inner }
    }
}
