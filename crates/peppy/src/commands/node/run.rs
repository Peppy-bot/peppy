use config::AnyType;
use config::launcher::Name;
use config::runtime::{NodeInstanceConfig, RuntimeConfig};
use core_node_api::encoding::{
    NodeInfoRequest, NodeInfoResponse, NodeRunFeedback, NodeRunGoal, NodeRunGoalResponse,
    NodeRunResult,
};
use names_generator2::get_random;
use peppylib::MessengerHandle;
use rand::rng;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::{Instant, sleep};
use tracing::info;

use crate::commands::{CALLER_INSTANCE_ID, GOAL_TIMEOUT};
use crate::context::AppContext;
use crate::error::{Error, Result};

use super::TimeoutConfig;
use super::env::caller_env_overrides;

use peppylib::core_node::transport::{poll_node_info, send_node_run};
/// Timeout for the quick `NodeInfoRequest` preflight in the `run -b` flow.
/// Matches `node info`'s request timeout — this is a metadata lookup,
/// not a long-running action, so it must fail fast if the daemon is down
/// rather than waiting out `timeouts.max_secs` (which can be 1 hour).
const NODE_INFO_PREFLIGHT_TIMEOUT: Duration = Duration::from_secs(30);

/// Interval between `NodeInfoRequest` polls while waiting for an in-flight
/// build to transition `Building -> Ready`. Builds typically take
/// seconds-to-minutes, so 500 ms keeps CLI latency low without flooding the
/// daemon.
const BUILD_WAIT_POLL_INTERVAL: Duration = Duration::from_millis(500);

/// What `run -b` should do with a node based on its current lifecycle stage.
#[derive(Debug, PartialEq, Eq)]
enum BuildDecision {
    /// Stage is `Ready` — artifact exists, skip the build and run directly.
    Skip,
    /// Stage is `Added` — trigger `build_node_async`.
    Build,
    /// Stage is `Building` — another build is in flight, poll until it
    /// finishes instead of trying to start a second build (the daemon
    /// rejects concurrent builds).
    Wait,
}

/// Pure helper: compute the remaining `max_secs` budget given how many
/// seconds have already elapsed. Split out from `remaining_timeouts` so the
/// budget-arithmetic + error path can be unit-tested without needing a
/// tokio runtime or time-mocking feature.
fn remaining_max_secs(original_max: u64, elapsed: u64, stage: &str) -> Result<u64> {
    if elapsed >= original_max {
        return Err(Error::ExecutionFailed(format!(
            "Timeout: max timeout of {original_max}s exceeded before {stage}. \
             Use --max-timeout <seconds> to increase."
        )));
    }
    Ok(original_max - elapsed)
}

/// Derive a new `TimeoutConfig` whose `max_secs` is what remains of the
/// original `max_secs` budget after the time elapsed since `start`. Returns
/// an `ExecutionFailed` error if no budget remains, so callers never kick
/// off a stage with a zero deadline.
///
/// `idle_secs` is preserved as-is: it's a per-call "no output" guard, not a
/// wall-clock budget, so it should not shrink across stages.
fn remaining_timeouts(
    timeouts: &TimeoutConfig,
    start: Instant,
    stage: &str,
) -> Result<TimeoutConfig> {
    let elapsed = start.elapsed().as_secs();
    let max_secs = remaining_max_secs(timeouts.max_secs, elapsed, stage)?;
    Ok(TimeoutConfig {
        idle_secs: timeouts.idle_secs,
        max_secs,
    })
}

/// Classify a node's lifecycle stage into a `run -b` action.
///
/// This is split out as a pure function so the stage-matching logic can be
/// unit-tested directly. Documented stage values come from
/// `core_node_api::encoding::NodeInfo::stage`: `Added | Building | Ready | Root`.
fn classify_stage(stage: &str, node_name: &str, tag: &str) -> Result<BuildDecision> {
    match stage {
        "Ready" => Ok(BuildDecision::Skip),
        "Added" => Ok(BuildDecision::Build),
        "Building" => Ok(BuildDecision::Wait),
        "Root" => Err(Error::ExecutionFailed(format!(
            "Node '{}:{}' is a root node and cannot be built or run via `node run`",
            node_name, tag
        ))),
        other => Err(Error::ExecutionFailed(format!(
            "Node '{}:{}' is in unexpected stage '{}'; cannot safely proceed with `run -b`",
            node_name, tag, other
        ))),
    }
}

/// Polls `NodeInfoRequest` until the node's stage transitions out of
/// `Building`. Returns `Ok(())` when the stage becomes `Ready`; returns an
/// error on timeout, on unexpected stage transitions (e.g. a failed build
/// falling back to `Added`), or if the node disappears from the stack.
async fn wait_for_build_to_finish(
    messenger: &MessengerHandle,
    core_node_name: &str,
    node_name: &str,
    tag: &str,
    timeouts: &TimeoutConfig,
) -> Result<()> {
    info!(
        "Node {}:{} is already building, waiting for the in-flight build to finish...",
        node_name, tag
    );

    let deadline = Instant::now() + Duration::from_secs(timeouts.max_secs);

    loop {
        let response = poll_node_info(
            &NodeInfoRequest::new(node_name.to_string(), tag.to_string()),
            messenger,
            core_node_name,
            CALLER_INSTANCE_ID,
            core_node_name,
            NODE_INFO_PREFLIGHT_TIMEOUT,
        )
        .await
        .map_err(|e| {
            Error::ExecutionFailed(format!(
                "Failed to poll node info while waiting for build: {}",
                e
            ))
        })?;

        match response {
            NodeInfoResponse::NotInStack => {
                return Err(Error::ExecutionFailed(format!(
                    "Node '{}:{}' disappeared from the stack while waiting for build to finish",
                    node_name, tag
                )));
            }
            NodeInfoResponse::Found(info) => match info.stage.as_str() {
                "Ready" => return Ok(()),
                "Building" => { /* still in flight — keep polling */ }
                other => {
                    return Err(Error::ExecutionFailed(format!(
                        "Node '{}:{}' transitioned to unexpected stage '{}' while waiting for build to finish",
                        node_name, tag, other
                    )));
                }
            },
        }

        if Instant::now() >= deadline {
            return Err(Error::ExecutionFailed(format!(
                "Timed out waiting for node '{}:{}' build to finish",
                node_name, tag
            )));
        }

        sleep(BUILD_WAIT_POLL_INTERVAL).await;
    }
}

/// Converts a list of key=value string pairs into node arguments.
/// Dot-separated keys are converted into nested objects.
/// For example: "device.physical=/dev/video0" becomes {"device": {"physical": "/dev/video0"}}
///
/// Values are parsed with type inference:
/// - "true"/"false" -> Bool
/// - Integer strings -> Int
/// - Float strings -> Float
/// - Everything else -> String
pub fn args_to_node_arguments(args: &[(String, String)]) -> BTreeMap<String, AnyType> {
    let mut result: BTreeMap<String, AnyType> = BTreeMap::new();

    for (key, value) in args {
        let parsed_value = parse_value(value);
        insert_nested_value(&mut result, key, parsed_value);
    }

    result
}

/// Inserts a value at a dot-separated path into a nested BTreeMap structure.
/// For example, path "device.physical" with value "foo" creates:
/// {"device": {"physical": "foo"}}
fn insert_nested_value(
    root: &mut std::collections::BTreeMap<String, AnyType>,
    path: &str,
    value: AnyType,
) {
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
fn insert_at_path(
    current: &mut std::collections::BTreeMap<String, AnyType>,
    parts: &[&str],
    value: AnyType,
) {
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

/// Shared logic for running a node instance.
/// Used by both `run_node` and `add_node` (when --run is set).
pub async fn run_instance_async(
    messenger_handle: &MessengerHandle,
    core_node_name: &str,
    node_name: &str,
    tag: &str,
    args: &[(String, String)],
    instance_id: Option<String>,
    timeouts: &TimeoutConfig,
) -> Result<String> {
    // Generate or use provided instance_id
    let instance_id = instance_id.unwrap_or_else(|| get_random(rng()));

    // Convert CLI arguments to node arguments
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
        NodeInstanceConfig {
            instance_id: Name::new(instance_id.clone())
                .map_err(|e| Error::PeppyConfig(e.into()))?,
            arguments,
        },
        node_name,
        core_node_name,
    )
    .map_err(Error::PeppyConfig)?;

    let runtime_config_json =
        serde_json::to_string(&runtime_config).map_err(|e| Error::Sync(e.to_string()))?;

    info!(
        "Calling node_run for {}:{} (instance_id={})...",
        node_name, tag, instance_id
    );

    let start_goal = NodeRunGoal::new(
        &runtime_config_json,
        node_name.to_string(),
        tag.to_string(),
        timeouts.max_secs,
    )
    .with_env_vars(caller_env_overrides());
    let mut action_handle = send_node_run(
        &start_goal,
        messenger_handle,
        core_node_name,
        CALLER_INSTANCE_ID,
        Some(core_node_name),
        None,
        GOAL_TIMEOUT,
    )
    .await
    .map_err(|e| Error::ExecutionFailed(format!("Failed to send node_run goal: {}", e)))?;

    let start_result = crate::commands::action_poll::run_action_with_feedback::<
        NodeRunGoalResponse,
        NodeRunFeedback,
        NodeRunResult,
    >(messenger_handle, &mut action_handle, timeouts, "node_run")
    .await?;

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
    timeouts: TimeoutConfig,
    build: bool,
) -> Result<()> {
    crate::commands::block_on(run_node_async(
        ctx,
        node_name,
        tag,
        args,
        instance_id,
        timeouts,
        build,
    ))
}

async fn run_node_async(
    ctx: &Arc<AppContext>,
    node_name: String,
    tag: String,
    args: Vec<(String, String)>,
    instance_id: Option<String>,
    timeouts: TimeoutConfig,
    build: bool,
) -> Result<()> {
    let conn = ctx.connect_to_daemon().await?;

    // Single end-to-end budget: every subsequent blocking stage (build, wait,
    // run) derives its `max_secs` from what's left of this budget, so the sum
    // of their wall-clock deadlines cannot exceed the original
    // `timeouts.max_secs`. The preflight `NodeInfoRequest` below intentionally
    // uses its own fixed-30s timeout and is exempt (see
    // `NODE_INFO_PREFLIGHT_TIMEOUT` docs above).
    let start = Instant::now();

    if build {
        // Look up the node's current lifecycle stage so we only trigger a
        // build when the node is not yet built. The same `NodeInfoRequest`
        // is used by the `node add` preflight (see add.rs).
        let response = poll_node_info(
            &NodeInfoRequest::new(node_name.clone(), tag.clone()),
            conn.messenger,
            &conn.core_node_name,
            CALLER_INSTANCE_ID,
            &conn.core_node_name,
            NODE_INFO_PREFLIGHT_TIMEOUT,
        )
        .await
        .map_err(|e| {
            Error::ExecutionFailed(format!("Failed to check node info before run: {}", e))
        })?;

        let info = match response {
            NodeInfoResponse::NotInStack => {
                return Err(Error::ExecutionFailed(format!(
                    "Node '{}:{}' is not in the node stack",
                    node_name, tag
                )));
            }
            NodeInfoResponse::Found(info) => info,
        };

        match classify_stage(info.stage.as_str(), &node_name, &tag)? {
            BuildDecision::Skip => {
                info!(
                    "Node {}:{} has already been built, skipping build",
                    node_name, tag
                );
            }
            BuildDecision::Build => {
                super::builder::build_node_async(
                    conn.messenger,
                    &conn.core_node_name,
                    &node_name,
                    &tag,
                    &remaining_timeouts(&timeouts, start, "build")?,
                    false,
                )
                .await?;
            }
            BuildDecision::Wait => {
                wait_for_build_to_finish(
                    conn.messenger,
                    &conn.core_node_name,
                    &node_name,
                    &tag,
                    &remaining_timeouts(&timeouts, start, "wait-for-build")?,
                )
                .await?;
            }
        }
    }

    run_instance_async(
        conn.messenger,
        &conn.core_node_name,
        &node_name,
        &tag,
        &args,
        instance_id,
        &remaining_timeouts(&timeouts, start, "run")?,
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

    #[test]
    fn classify_stage_ready_skips() {
        assert_eq!(
            classify_stage("Ready", "n", "t").expect("Ready should classify"),
            BuildDecision::Skip
        );
    }

    #[test]
    fn classify_stage_added_builds() {
        assert_eq!(
            classify_stage("Added", "n", "t").expect("Added should classify"),
            BuildDecision::Build
        );
    }

    #[test]
    fn classify_stage_building_waits_does_not_rebuild() {
        // Regression: previously stage "Building" fell through to the
        // "build" branch, and the daemon rejected the second build goal
        // with "action already in progress" / "cannot build". The CLI now
        // waits for the in-flight build instead of trying to start a new
        // one.
        assert_eq!(
            classify_stage("Building", "n", "t").expect("Building should classify"),
            BuildDecision::Wait
        );
    }

    #[test]
    fn classify_stage_root_fails_fast() {
        let err =
            classify_stage("Root", "my_node", "v1").expect_err("Root should fail to classify");
        let msg = format!("{err}");
        assert!(msg.contains("my_node"), "error should name node: {msg}");
        assert!(msg.contains("v1"), "error should name tag: {msg}");
        assert!(
            msg.contains("root"),
            "error should mention root stage: {msg}"
        );
    }

    #[test]
    fn remaining_max_secs_subtracts_elapsed() {
        let remaining = remaining_max_secs(100, 25, "build")
            .expect("remaining budget should still be positive");
        assert_eq!(remaining, 75);
    }

    #[test]
    fn remaining_max_secs_errors_when_budget_exhausted() {
        // Equal to budget — already exhausted (we refuse zero-deadline calls).
        let err_exact =
            remaining_max_secs(30, 30, "run").expect_err("exhausted budget should error");
        let msg = format!("{err_exact}");
        assert!(msg.contains("30s"), "error should cite original max: {msg}");
        assert!(msg.contains("run"), "error should cite stage label: {msg}");
        assert!(
            msg.contains("--max-timeout"),
            "error should hint at the CLI flag: {msg}"
        );

        // Past budget — same error path.
        assert!(remaining_max_secs(30, 45, "run").is_err());
    }

    #[test]
    fn classify_stage_unknown_fails_fast() {
        let err = classify_stage("Mystery", "my_node", "v1")
            .expect_err("unknown stage should fail to classify");
        let msg = format!("{err}");
        assert!(
            msg.contains("Mystery"),
            "error should quote unknown stage: {msg}"
        );
        assert!(msg.contains("my_node"), "error should name node: {msg}");
    }
}
