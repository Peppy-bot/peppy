use config::peppy_config::{DeploymentInstance, Name};
use config::runtime::RuntimeConfig;
use config::{AnyType, NodeArguments};
use master_node::encoding::NodeStartRequest;
use names_generator2::get_random;
use peppylib::MessengerHandle;
use rand::rng;
use std::sync::Arc;
use std::time::Duration;
use tracing::info;

use crate::context::{AppContext, DaemonState};
use crate::error::{Error, Result};

const CALLER_INSTANCE_ID: &str = "peppy-cli";
// Allow extra time for node startup (build verification, zenoh connection, health check)
const START_REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

/// Converts a list of key=value string pairs into NodeArguments.
/// Values are parsed with type inference:
/// - "true"/"false" -> Bool
/// - Integer strings -> Int
/// - Float strings -> Float
/// - Everything else -> String
pub fn args_to_node_arguments(args: &[(String, String)]) -> NodeArguments {
    args.iter()
        .map(|(key, value)| {
            let any_value = parse_value(value);
            (key.clone(), any_value)
        })
        .collect()
}

/// Parses a string value into an AnyType with type inference
fn parse_value(value: &str) -> AnyType {
    // Try bool
    if value.eq_ignore_ascii_case("true") {
        return AnyType::Bool(true);
    }
    if value.eq_ignore_ascii_case("false") {
        return AnyType::Bool(false);
    }

    // Try integer (i64)
    if let Ok(int_val) = value.parse::<i64>() {
        return AnyType::Int(int_val);
    }

    // Try float (f64)
    if let Ok(float_val) = value.parse::<f64>() {
        return AnyType::Float(float_val);
    }

    // Default to string
    AnyType::String(value.to_string())
}

/// Shared logic for starting a node instance.
/// Used by both `run_node` and `add_node` (when --run is set).
pub async fn start_instance_async(
    messenger_handle: &MessengerHandle,
    master_node_name: &str,
    node_name: &str,
    tag: &str,
    args: &[(String, String)],
    instance_id: Option<String>,
) -> Result<String> {
    // Generate or use provided instance_id
    let instance_id = instance_id.unwrap_or_else(|| get_random(rng()));

    // Convert CLI arguments to NodeArguments
    let arguments = args_to_node_arguments(args);

    info!(
        "Starting node {}:{} with instance_id '{}' and {} argument(s)...",
        node_name,
        tag,
        instance_id,
        arguments.len()
    );

    let (messaging_host, messaging_port) = messenger_handle
        .messaging_endpoint()
        .await
        .unwrap_or_else(|| {
            (
                config::consts::DEFAULT_ZENOH_HOST.to_string(),
                config::consts::DEFAULT_ZENOH_PORT,
            )
        });

    // Create the runtime config with the parsed arguments
    let runtime_config = RuntimeConfig::new(
        messaging_host.as_str(),
        messaging_port,
        DeploymentInstance {
            instance_id: Name::new(instance_id.clone())
                .map_err(|e| Error::PeppyConfig(e.into()))?,
            arguments,
        },
        node_name,
        master_node_name,
    )
    .map_err(Error::PeppyConfig)?;

    let runtime_config_json =
        serde_json::to_string(&runtime_config).map_err(|e| Error::Sync(e.to_string()))?;

    info!(
        "Calling node_start for {}:{} (instance_id={})...",
        node_name, tag, instance_id
    );

    let start_request =
        NodeStartRequest::new(&runtime_config_json, node_name.to_string(), tag.to_string());
    let start_response = start_request
        .poll(
            messenger_handle,
            master_node_name,
            CALLER_INSTANCE_ID,
            master_node_name,
            START_REQUEST_TIMEOUT,
        )
        .await
        .map_err(|e| Error::ExecutionFailed(format!("Failed to call node_start service: {}", e)))?;

    if !start_response.success {
        return Err(Error::ExecutionFailed(
            start_response
                .error_message
                .unwrap_or_else(|| "node_start failed with no error message".to_string()),
        ));
    }

    info!("Started node instance '{}'", instance_id);
    Ok(instance_id)
}

pub fn run_node(
    ctx: &Arc<AppContext>,
    node_name: String,
    tag: String,
    args: Vec<(String, String)>,
    instance_id: Option<String>,
) -> Result<()> {
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(run_node_async(ctx, node_name, tag, args, instance_id))
}

async fn run_node_async(
    ctx: &Arc<AppContext>,
    node_name: String,
    tag: String,
    args: Vec<(String, String)>,
    instance_id: Option<String>,
) -> Result<()> {
    let daemon_state = DaemonState::read().map_err(|e| {
        Error::ExecutionFailed(format!(
            "Failed to read daemon state. Is the peppy daemon running? Error: {}",
            e
        ))
    })?;
    let master_node_name = daemon_state.master_node_name;

    ctx.connect().await?;
    let messenger_handle = ctx
        .messenger_handle()
        .ok_or_else(|| Error::ExecutionFailed("Failed to connect to daemon".to_string()))?;

    start_instance_async(
        messenger_handle,
        &master_node_name,
        &node_name,
        &tag,
        &args,
        instance_id,
    )
    .await?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_bool_values() {
        assert_eq!(parse_value("true"), AnyType::Bool(true));
        assert_eq!(parse_value("True"), AnyType::Bool(true));
        assert_eq!(parse_value("TRUE"), AnyType::Bool(true));
        assert_eq!(parse_value("false"), AnyType::Bool(false));
        assert_eq!(parse_value("False"), AnyType::Bool(false));
        assert_eq!(parse_value("FALSE"), AnyType::Bool(false));
    }

    #[test]
    fn parse_int_values() {
        assert_eq!(parse_value("42"), AnyType::Int(42));
        assert_eq!(parse_value("-42"), AnyType::Int(-42));
        assert_eq!(parse_value("0"), AnyType::Int(0));
    }

    #[test]
    fn parse_float_values() {
        assert_eq!(parse_value("1.25"), AnyType::Float(1.25));
        assert_eq!(parse_value("-2.5"), AnyType::Float(-2.5));
        assert_eq!(parse_value("0.0"), AnyType::Float(0.0));
    }

    #[test]
    fn parse_string_values() {
        assert_eq!(parse_value("hello"), AnyType::String("hello".to_string()));
        assert_eq!(
            parse_value("1280x720"),
            AnyType::String("1280x720".to_string())
        );
        assert_eq!(
            parse_value("foo=bar"),
            AnyType::String("foo=bar".to_string())
        );
    }

    #[test]
    fn args_to_node_arguments_converts_correctly() {
        let args = vec![
            ("resolution".to_string(), "1280x720".to_string()),
            ("frequency".to_string(), "30".to_string()),
            ("enabled".to_string(), "true".to_string()),
            ("gain".to_string(), "1.5".to_string()),
        ];

        let node_args = args_to_node_arguments(&args);

        assert_eq!(node_args.len(), 4);
        assert_eq!(
            node_args.get("resolution"),
            Some(&AnyType::String("1280x720".to_string()))
        );
        assert_eq!(node_args.get("frequency"), Some(&AnyType::Int(30)));
        assert_eq!(node_args.get("enabled"), Some(&AnyType::Bool(true)));
        assert_eq!(node_args.get("gain"), Some(&AnyType::Float(1.5)));
    }
}
