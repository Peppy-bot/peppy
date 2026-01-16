//! Configuration utilities and re-exports from the config crate.

pub use config::NodeArguments;
pub use config::consts::{NODE_CONFIG_FILE, NODE_CONFIG_FINGERPRINT_FILE, RUNTIME_CONFIG_VAR_NAME};
pub use config::node::*;

/// Deserialize node arguments into a custom parameter struct.
///
/// This function converts a [`NodeArguments`] map into a user-defined struct type
/// using serde's deserialization.
///
/// # Example
///
/// ```ignore
/// use serde::Deserialize;
/// use peppylib::config::{NodeArguments, deserialize_parameters};
///
/// #[derive(Deserialize)]
/// struct MyParams {
///     timeout: u32,
///     name: String,
/// }
///
/// fn example(args: &NodeArguments) -> peppylib::PeppyResult<MyParams> {
///     deserialize_parameters(args)
/// }
/// ```
pub fn deserialize_parameters<T>(args: &NodeArguments) -> Result<T, crate::PeppyError>
where
    T: serde::de::DeserializeOwned,
{
    let json_value = serde_json::to_value(args).map_err(|e| {
        crate::PeppyError::ParameterDeserialization(format!(
            "failed to serialize parameters: {}",
            e
        ))
    })?;
    serde_json::from_value(json_value).map_err(|e| {
        crate::PeppyError::ParameterDeserialization(format!(
            "failed to deserialize parameters: {}",
            e
        ))
    })
}
