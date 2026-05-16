use peppylib::messaging::{NATIVE_IFACE_SEGMENT_NAME, NATIVE_IFACE_SEGMENT_TAG};
use pyo3::PyResult;
use pyo3::exceptions::PyValueError;

const MISMATCH_MSG: &str = "iface_name and iface_tag must both be set or both be None";

/// Resolve optional `iface_name`/`iface_tag` Python arguments to wire
/// segments, substituting the native sentinels when both are `None`. Errors
/// with `ValueError` if exactly one of the two is set, which is never a valid
/// binding (a `conforms_to` interface requires both, native requires neither).
pub(crate) fn or_native<'a>(
    name: Option<&'a str>,
    tag: Option<&'a str>,
) -> PyResult<(&'a str, &'a str)> {
    match (name, tag) {
        (Some(n), Some(t)) => Ok((n, t)),
        (None, None) => Ok((NATIVE_IFACE_SEGMENT_NAME, NATIVE_IFACE_SEGMENT_TAG)),
        _ => Err(PyValueError::new_err(MISMATCH_MSG)),
    }
}

/// Owned counterpart of [`or_native`], for call sites that need to store the
/// segments beyond the current call (e.g. `ActionMessenger.send_goal` caches
/// them on the goal handle). Same strict mismatch validation.
pub(crate) fn or_native_owned(
    name: Option<String>,
    tag: Option<String>,
) -> PyResult<(String, String)> {
    match (name, tag) {
        (Some(n), Some(t)) => Ok((n, t)),
        (None, None) => Ok((
            NATIVE_IFACE_SEGMENT_NAME.to_string(),
            NATIVE_IFACE_SEGMENT_TAG.to_string(),
        )),
        _ => Err(PyValueError::new_err(MISMATCH_MSG)),
    }
}
