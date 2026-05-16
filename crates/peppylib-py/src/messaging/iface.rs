use peppylib::messaging::{Iface, IfaceError};
use pyo3::PyResult;
use pyo3::exceptions::PyValueError;

fn iface_error_to_py(_err: IfaceError) -> pyo3::PyErr {
    PyValueError::new_err("iface_name and iface_tag must both be set or both be None")
}

/// Resolve optional `iface_name`/`iface_tag` Python arguments into an [`Iface`].
/// Errors with `ValueError` if exactly one of the two is set, which is never a
/// valid binding (a `conforms_to` interface requires both, native requires neither).
pub(crate) fn into_iface(name: Option<&str>, tag: Option<&str>) -> PyResult<Iface> {
    Iface::from_options(name, tag).map_err(iface_error_to_py)
}

/// Owned counterpart of [`into_iface`], for call sites that take owned strings
/// from Python.
pub(crate) fn into_iface_owned(name: Option<String>, tag: Option<String>) -> PyResult<Iface> {
    Iface::from_options(name.as_deref(), tag.as_deref()).map_err(iface_error_to_py)
}
