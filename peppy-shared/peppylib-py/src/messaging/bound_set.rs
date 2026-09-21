//! The member of a bound producer set, as the generated `bound_members()`
//! accessors of `one_or_more` and `zero_or_more` slots answer it.

use super::PyProducerRef;
use peppylib::messaging::BoundMember;
use pyo3::prelude::*;

/// One member of a consumer slot's bound set: the producer and the copy its
/// instance belongs to (`None` for an instance the launcher deploys outside
/// any copy). A node holding members from several copies groups them by
/// `copy`.
#[pyclass(name = "BoundMember", frozen, eq, hash, skip_from_py_object)]
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct PyBoundMember {
    pub(crate) inner: BoundMember,
}

#[pymethods]
impl PyBoundMember {
    #[new]
    #[pyo3(signature = (producer, copy=None))]
    fn new(producer: &PyProducerRef, copy: Option<&str>) -> PyResult<Self> {
        let copy = copy
            .map(config::runtime::Name::new)
            .transpose()
            .map_err(|e| pyo3::exceptions::PyValueError::new_err(e.to_string()))?;
        Ok(Self {
            inner: BoundMember {
                producer: producer.as_inner().clone(),
                copy,
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
    fn copy(&self) -> Option<&str> {
        self.inner.copy.as_ref().map(|copy| copy.as_str())
    }

    fn __repr__(&self) -> String {
        let copy = match &self.inner.copy {
            Some(copy) => format!("{:?}", copy.as_str()),
            None => "None".to_string(),
        };
        format!(
            "BoundMember(producer={}, copy={})",
            PyProducerRef::from(self.inner.producer.clone()).__repr__(),
            copy
        )
    }
}

impl From<BoundMember> for PyBoundMember {
    fn from(inner: BoundMember) -> Self {
        Self { inner }
    }
}
