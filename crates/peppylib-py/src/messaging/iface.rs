use peppylib::messaging::{NATIVE_IFACE_SEGMENT_NAME, NATIVE_IFACE_SEGMENT_TAG};

/// Resolve optional `iface_name`/`iface_tag` Python arguments to wire
/// segments, substituting the native sentinels when `None`.
pub(crate) fn or_native<'a>(name: Option<&'a str>, tag: Option<&'a str>) -> (&'a str, &'a str) {
    (
        name.unwrap_or(NATIVE_IFACE_SEGMENT_NAME),
        tag.unwrap_or(NATIVE_IFACE_SEGMENT_TAG),
    )
}

/// Owned counterpart of [`or_native`], for call sites that need to store the
/// segments beyond the current call (e.g. `ActionMessenger.send_goal` caches
/// them on the goal handle).
pub(crate) fn or_native_owned(name: Option<String>, tag: Option<String>) -> (String, String) {
    (
        name.unwrap_or_else(|| NATIVE_IFACE_SEGMENT_NAME.to_string()),
        tag.unwrap_or_else(|| NATIVE_IFACE_SEGMENT_TAG.to_string()),
    )
}
