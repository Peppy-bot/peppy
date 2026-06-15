//! Configuration utilities and re-exports from the config crate.

use config::NodeArguments;
pub use config::node::QoSProfile;

/// Format a JSON schema validation error into a human-readable message.
fn format_validation_error(error: &jsonschema::ValidationError) -> String {
    let message = error.to_string();

    // Extract field name from "required property" errors
    // e.g., '"device" is a required property' -> 'device'
    if message.contains("is a required property")
        && let Some(field) = message.split('"').nth(1)
    {
        return field.to_string();
    }

    // For other errors, include the path if present
    let path = error.instance_path().to_string();
    if path.is_empty() {
        message
    } else {
        format!("{}: {}", path.trim_start_matches('/'), message)
    }
}

/// Deserialize validated node arguments into a custom parameter struct.
///
/// Converts [`NodeArguments`] (already validated against the manifest spec) into
/// a user-defined struct type. It validates the input against the JSON schema
/// derived from the type, collecting all validation errors (including multiple
/// missing fields) and reporting them at once. Called by
/// [`NodeBuilder::init`](crate::runtime::NodeBuilder::init) to parse a node's
/// typed parameters eagerly.
pub(crate) fn deserialize_parameters<T>(args: &NodeArguments) -> Result<T, crate::PeppyError>
where
    T: serde::de::DeserializeOwned + schemars::JsonSchema,
{
    let json_value = serde_json::to_value(args).map_err(|e| {
        crate::ParameterDeserializationError::single(format!(
            "failed to serialize parameters: {}",
            e
        ))
    })?;

    // Generate JSON schema from the target type
    let schema = schemars::schema_for!(T);
    let schema_value = serde_json::to_value(&schema).map_err(|e| {
        crate::ParameterDeserializationError::single(format!("failed to generate schema: {}", e))
    })?;

    // Validate input against the schema
    let validator = jsonschema::validator_for(&schema_value).map_err(|e| {
        crate::ParameterDeserializationError::single(format!("failed to create validator: {}", e))
    })?;

    let errors: Vec<_> = validator.iter_errors(&json_value).collect();
    if !errors.is_empty() {
        let error_messages: Vec<String> =
            errors.iter().map(|e| format_validation_error(e)).collect();
        return Err(crate::ParameterDeserializationError::multiple(error_messages).into());
    }

    // Schema validation passed, now deserialize
    Ok(serde_json::from_value(json_value).map_err(|e| {
        crate::ParameterDeserializationError::single(format!(
            "failed to deserialize parameters: {}",
            e
        ))
    })?)
}
