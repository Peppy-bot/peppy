//! Small glue helpers shared by the node command handlers that don't fit
//! in one of the more specific submodules: panic payload decoding,
//! random id generation, and the encoding-error wrapper used when a
//! goal handler fails to produce a response payload.

/// Extract a human-readable message from a panic payload.
/// Used by spawned task handlers to convert panics into failure results.
pub(crate) fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "unknown panic payload".to_string()
    }
}

/// Maps an encoding `Result` to a `PeppyResult`, wrapping the error as an
/// `InternalEncodingError` so it can be returned directly from a goal handler.
/// Used in place of open-coding the same `map_err` at every rejection and
/// accepted-response encoding site in the add/build/start handlers.
pub(crate) fn encode_response_or_err(
    identifier: &'static str,
    result: core_node_api::Result<Vec<u8>>,
) -> peppylib::PeppyResult<peppylib::types::Payload> {
    result.map(peppylib::types::Payload::from).map_err(|e| {
        peppylib::PeppyError::InternalEncodingError {
            identifier: identifier.to_string(),
            reason: format!("Failed to encode response: {}", e),
        }
    })
}

pub(crate) fn generate_random_id() -> String {
    use rand::RngExt;
    let mut rng = rand::rng();
    let bytes: [u8; 6] = rng.random();
    bytes.iter().map(|b| format!("{:02x}", b)).collect()
}
