use config::peppy_config::Name;
use config::runtime::{NodeInstance, RuntimeConfig};
use config::{AnyType, NodeArguments};
use daemon_node::encoding::{
    NodeStartFeedback, NodeStartGoal, NodeStartGoalResponse, NodeStartResult,
};
use names_generator2::get_random;
use peppylib::{ActionMessenger, MessengerHandle, PeppyError};
use rand::rng;
use std::sync::Arc;
use std::time::Duration;
use tracing::info;

use crate::context::AppContext;
use crate::error::{Error, Result};
use crate::terminal::ScrollingOutput;

use super::env::caller_env_overrides;

const CALLER_INSTANCE_ID: &str = "peppy-cli";
const GOAL_TIMEOUT: Duration = Duration::from_secs(30);
const SCROLLING_OUTPUT_LINES: usize = 10;

/// Converts a list of key=value string pairs into NodeArguments.
/// Dot-separated keys are converted into nested objects.
/// For example: "device.physical=/dev/video0" becomes {"device": {"physical": "/dev/video0"}}
///
/// Values are parsed with type inference:
/// - "true"/"false" -> Bool
/// - Integer strings -> Int
/// - Float strings -> Float
/// - Everything else -> String
pub fn args_to_node_arguments(args: &[(String, String)]) -> NodeArguments {
    let mut result: NodeArguments = std::collections::BTreeMap::new();

    for (key, value) in args {
        let parsed_value = parse_value(value);
        insert_nested_value(&mut result, key, parsed_value);
    }

    result
}

/// Inserts a value at a dot-separated path into a nested NodeArguments structure.
/// For example, path "device.physical" with value "foo" creates:
/// {"device": {"physical": "foo"}}
fn insert_nested_value(root: &mut NodeArguments, path: &str, value: AnyType) {
    let parts: Vec<&str> = path.split('.').collect();

    if parts.len() == 1 {
        // Simple key, insert directly
        root.insert(path.to_string(), value);
        return;
    }

    // For nested paths, we need to navigate/create the path
    insert_at_path(root, &parts, value);
}

/// Recursively inserts a value at the given path parts.
fn insert_at_path(current: &mut NodeArguments, parts: &[&str], value: AnyType) {
    if parts.is_empty() {
        return;
    }

    let key = parts[0].to_string();

    if parts.len() == 1 {
        // Last part - insert the actual value
        current.insert(key, value);
        return;
    }

    // Intermediate part - ensure an Object exists at this key
    let entry = current
        .entry(key)
        .or_insert_with(|| AnyType::Object(std::collections::BTreeMap::new()));

    // If the entry isn't an object, make it one
    if !matches!(entry, AnyType::Object(_)) {
        *entry = AnyType::Object(std::collections::BTreeMap::new());
    }

    // Navigate into the object and recurse
    if let AnyType::Object(obj) = entry {
        insert_at_path(obj, &parts[1..], value);
    }
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
    daemon_node_name: &str,
    node_name: &str,
    tag: &str,
    args: &[(String, String)],
    instance_id: Option<String>,
    timeout_secs: u64,
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

    let (messaging_host, messaging_port) = match messenger_handle.messaging_endpoint().await {
        Some((host, port)) => (host, port),
        None => (
            config::consts::DEFAULT_MESSAGING_HOST.to_string(),
            messenger_handle.messaging_port().await,
        ),
    };

    // Create the runtime config with the parsed arguments
    let runtime_config = RuntimeConfig::new(
        messaging_host.as_str(),
        messaging_port,
        NodeInstance {
            instance_id: Name::new(instance_id.clone())
                .map_err(|e| Error::PeppyConfig(e.into()))?,
            arguments,
        },
        node_name,
        daemon_node_name,
    )
    .map_err(Error::PeppyConfig)?;

    let runtime_config_json =
        serde_json::to_string(&runtime_config).map_err(|e| Error::Sync(e.to_string()))?;

    info!(
        "Calling node_start for {}:{} (instance_id={})...",
        node_name, tag, instance_id
    );

    let start_goal = NodeStartGoal::new(
        &runtime_config_json,
        node_name.to_string(),
        tag.to_string(),
        timeout_secs,
    )
    .with_env_vars(caller_env_overrides());
    let mut action_handle = start_goal
        .send_goal(
            messenger_handle,
            daemon_node_name,
            CALLER_INSTANCE_ID,
            Some(daemon_node_name),
            None,
            GOAL_TIMEOUT,
        )
        .await
        .map_err(|e| Error::ExecutionFailed(format!("Failed to send node_start goal: {}", e)))?;

    // Decode the goal response to get log_path
    let goal_response_payload = action_handle.goal_response().payload();
    let goal_response = NodeStartGoalResponse::decode(&goal_response_payload)
        .map_err(|e| Error::ExecutionFailed(format!("Failed to decode goal response: {}", e)))?;

    if !goal_response.accepted {
        return Err(Error::ExecutionFailed(format!(
            "Goal rejected: {}",
            goal_response
                .rejection_reason
                .unwrap_or_else(|| "unknown reason".to_string())
        )));
    }

    info!("Log file: {}", goal_response.log_path.display());

    let mut scrolling_output = ScrollingOutput::new(SCROLLING_OUTPUT_LINES);

    let result_timeout = Duration::from_secs(timeout_secs);
    let deadline = tokio::time::Instant::now() + result_timeout;
    let start_result = loop {
        // Drain feedback so the publisher doesn't block on a full channel.
        loop {
            let now = tokio::time::Instant::now();
            if now >= deadline {
                scrolling_output.clear();
                return Err(Error::ExecutionFailed(format!(
                    "Timeout waiting for node_start result after {} seconds. \
                     Use --timeout <seconds> to increase the timeout.",
                    timeout_secs
                )));
            }
            let remaining = deadline - now;
            let drain_timeout = Duration::from_millis(50).min(remaining);
            match tokio::time::timeout(drain_timeout, action_handle.on_next_feedback()).await {
                Ok(Ok(msg)) => {
                    let payload = msg.payload();
                    if let Ok(feedback) = NodeStartFeedback::decode(payload.as_ref()) {
                        scrolling_output.add_line(&feedback.line, feedback.is_stderr());
                    }
                }
                Ok(Err(_)) => break,
                Err(_) => break,
            }
        }

        let now = tokio::time::Instant::now();
        if now >= deadline {
            scrolling_output.clear();
            return Err(Error::ExecutionFailed(format!(
                "Timeout waiting for node_start result after {} seconds. \
                 Use --timeout <seconds> to increase the timeout.",
                timeout_secs
            )));
        }
        let remaining = deadline - now;
        let poll_timeout = Duration::from_millis(200).min(remaining);
        match ActionMessenger::request_result(messenger_handle, &action_handle, poll_timeout).await
        {
            Ok(msg) => {
                let payload = msg.payload();
                match NodeStartResult::decode(&payload) {
                    Ok(result) => break result,
                    Err(err) => {
                        let pending = std::str::from_utf8(payload.as_ref())
                            .map(|text| text.starts_with("result pending"))
                            .unwrap_or(false);
                        if !pending {
                            scrolling_output.clear();
                            return Err(Error::ExecutionFailed(format!(
                                "Failed to decode node_start result: {}",
                                err
                            )));
                        }
                    }
                }
            }
            Err(PeppyError::ActionResultTimeout { .. }) => {}
            Err(err) => {
                scrolling_output.clear();
                return Err(Error::ExecutionFailed(format!(
                    "Failed to get node_start result: {}",
                    err
                )));
            }
        }

        tokio::time::sleep(Duration::from_millis(50)).await;
    };

    scrolling_output.clear();

    if !start_result.success {
        return Err(Error::ExecutionFailed(
            start_result
                .error_message
                .unwrap_or_else(|| "node_start failed with no error message".to_string()),
        ));
    }

    if let Some(pid) = start_result.pid {
        info!("Started node instance '{}' (pid: {})", instance_id, pid);
    } else {
        info!("Started node instance '{}'", instance_id);
    }
    Ok(instance_id)
}

pub fn run_node(
    ctx: &Arc<AppContext>,
    node_name: String,
    tag: String,
    args: Vec<(String, String)>,
    instance_id: Option<String>,
    timeout_secs: u64,
) -> Result<()> {
    crate::commands::block_on(run_node_async(
        ctx,
        node_name,
        tag,
        args,
        instance_id,
        timeout_secs,
    ))
}

async fn run_node_async(
    ctx: &Arc<AppContext>,
    node_name: String,
    tag: String,
    args: Vec<(String, String)>,
    instance_id: Option<String>,
    timeout_secs: u64,
) -> Result<()> {
    let daemon_state = ctx.read_daemon_state().map_err(|e| {
        Error::ExecutionFailed(format!(
            "Failed to read daemon state. Is the peppy daemon running? Error: {}",
            e
        ))
    })?;
    let daemon_node_name = daemon_state.daemon_node_name;

    ctx.connect().await?;
    let messenger_handle = ctx
        .messenger_handle()
        .ok_or_else(|| Error::ExecutionFailed("Failed to connect to daemon".to_string()))?;

    start_instance_async(
        messenger_handle,
        &daemon_node_name,
        &node_name,
        &tag,
        &args,
        instance_id,
        timeout_secs,
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

    #[test]
    fn args_to_node_arguments_handles_nested_keys() {
        let args = vec![
            ("device.physical".to_string(), "/dev/video0".to_string()),
            ("device.sim".to_string(), "mock:camera".to_string()),
            ("video.frame_rate".to_string(), "30".to_string()),
            ("video.resolution.width".to_string(), "1280".to_string()),
            ("video.resolution.height".to_string(), "720".to_string()),
        ];

        let node_args = args_to_node_arguments(&args);

        // Should have 2 top-level keys: device and video
        assert_eq!(node_args.len(), 2);

        // Check device object
        let device = node_args.get("device").expect("device should exist");
        match device {
            AnyType::Object(device_obj) => {
                assert_eq!(device_obj.len(), 2);
                assert_eq!(
                    device_obj.get("physical"),
                    Some(&AnyType::String("/dev/video0".to_string()))
                );
                assert_eq!(
                    device_obj.get("sim"),
                    Some(&AnyType::String("mock:camera".to_string()))
                );
            }
            _ => panic!("device should be an object"),
        }

        // Check video object with nested resolution
        let video = node_args.get("video").expect("video should exist");
        match video {
            AnyType::Object(video_obj) => {
                assert_eq!(video_obj.len(), 2);
                assert_eq!(video_obj.get("frame_rate"), Some(&AnyType::Int(30)));

                let resolution = video_obj
                    .get("resolution")
                    .expect("resolution should exist");
                match resolution {
                    AnyType::Object(res_obj) => {
                        assert_eq!(res_obj.len(), 2);
                        assert_eq!(res_obj.get("width"), Some(&AnyType::Int(1280)));
                        assert_eq!(res_obj.get("height"), Some(&AnyType::Int(720)));
                    }
                    _ => panic!("resolution should be an object"),
                }
            }
            _ => panic!("video should be an object"),
        }
    }
}
