use config::AnyType;
use config::launcher::{BindingValidationItem, DeploymentInstance, Name, validate_bindings};
use config::node::ConformsToItem;
use config::runtime::{NodeInstanceConfig, RuntimeConfig, SlotBinding};
use core_node_api::NodeStage;
use core_node_api::SerializedNodeGraph;
use core_node_api::encoding::{
    NodeInfoRequest, NodeInfoResponse, NodeRunFeedback, NodeRunGoal, NodeRunGoalResponse,
    NodeRunResult, StackListRequest,
};
use names_generator2::get_random;
use peppylib::MessengerHandle;
use rand::rng;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::{Instant, sleep};
use tracing::{debug, info};

use crate::commands::{CALLER_INSTANCE_ID, GOAL_TIMEOUT};
use crate::context::AppContext;
use crate::error::{Error, Result};

use super::TimeoutConfig;
use super::env::caller_env_overrides;

use peppylib::core_node::transport::{poll_node_info, poll_stack_list, send_node_run};
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
/// Split out as a pure function so the stage-matching logic can be
/// unit-tested directly.
fn classify_stage(stage: NodeStage, node_name: &str, tag: &str) -> Result<BuildDecision> {
    match stage {
        NodeStage::Ready => Ok(BuildDecision::Skip),
        NodeStage::Added => Ok(BuildDecision::Build),
        NodeStage::Building => Ok(BuildDecision::Wait),
        NodeStage::Root => Err(Error::ExecutionFailed(format!(
            "Node '{}:{}' is a root node and cannot be built or run via `node run`",
            node_name, tag
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
            NodeInfoResponse::Found(info) => match info.stage {
                NodeStage::Ready => return Ok(()),
                NodeStage::Building => { /* still in flight — keep polling */ }
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

/// Collapse the clap-parsed `Vec<(KEY, VALUE)>` into a `BTreeMap`,
/// rejecting duplicate `KEY`s. Each `KEY` must be unique per invocation
/// (rule 6) — pinned `KEY`s match a declared link_id, free-form `KEY`s
/// label a `from_any` binding, and either way two bindings on the same
/// key would clobber.
fn binds_to_map(binds: &[(String, String)], instance_id: &str) -> Result<BTreeMap<String, String>> {
    let mut map = BTreeMap::new();
    for (key, value) in binds {
        if map.insert(key.clone(), value.clone()).is_some() {
            return Err(Error::ExecutionFailed(format!(
                "duplicate binding key `{key}` on instance `{instance_id}` (each --bind KEY must be distinct)"
            )));
        }
    }
    Ok(map)
}

/// Pre-flight bind validation. Snapshots the running stack via
/// `stack_list` + `node_info`, feeds it together with the consumer
/// being launched into the launcher's `validate_bindings`, and returns
/// the resolved per-slot `SlotBinding` map for the consumer instance.
/// Every rule violation is a hard error; there is no warning path.
///
/// Returns `Ok(None)` on transient transport failures so the call site
/// can swallow them and continue — an unreachable daemon should fail
/// the actual `node_run` invocation, not the pre-flight.
async fn validate_binds_against_stack(
    messenger: &MessengerHandle,
    core_node_name: &str,
    target_name: &str,
    target_tag: &str,
    target_instance_id: &str,
    binds: &BTreeMap<String, String>,
) -> Result<Option<BTreeMap<String, SlotBinding>>> {
    let stack_response = poll_stack_list(
        &StackListRequest::new(false),
        messenger,
        core_node_name,
        CALLER_INSTANCE_ID,
        core_node_name,
        NODE_INFO_PREFLIGHT_TIMEOUT,
    )
    .await
    .map_err(|e| Error::ExecutionFailed(format!("failed to list stack: {e}")))?;

    let graph: SerializedNodeGraph = serde_json::from_str(&stack_response.graph_json)
        .map_err(|e| Error::ExecutionFailed(format!("failed to parse stack graph JSON: {e}")))?;

    // Snapshot every running (name, tag) → its instances + each instance's
    // bindings. Skip Root (the daemon's own internals).
    struct StackNode {
        name: String,
        tag: String,
        instances: Vec<DeploymentInstance>,
        depends_on: Option<config::node::DependsOn>,
        /// Producer-side `interfaces.conforms_to`. Empty when the node
        /// declares no conformance. Threaded through into the binding
        /// validator so interface-dep slots can check this node's
        /// conformance claims.
        conforms_to: Vec<ConformsToItem>,
    }

    let stack_nodes: Vec<_> = graph
        .nodes
        .iter()
        .filter(|n| !matches!(n.stage, Some(NodeStage::Root)))
        .collect();

    let info_futures = stack_nodes.iter().map(|node| async move {
        let info = poll_node_info(
            &NodeInfoRequest::new(node.name.clone(), node.tag.clone()),
            messenger,
            core_node_name,
            CALLER_INSTANCE_ID,
            core_node_name,
            NODE_INFO_PREFLIGHT_TIMEOUT,
        )
        .await
        .map_err(|e| {
            Error::ExecutionFailed(format!(
                "failed to fetch info for stack node '{}:{}': {e}",
                node.name, node.tag
            ))
        })?;
        Ok::<_, Error>(info)
    });
    let infos = futures::future::try_join_all(info_futures).await?;

    let mut snapshot: Vec<StackNode> = Vec::with_capacity(stack_nodes.len());
    for (node, info_response) in stack_nodes.iter().zip(infos) {
        let info = match info_response {
            NodeInfoResponse::Found(info) => info,
            NodeInfoResponse::NotInStack => continue,
        };
        // Running instances of this node. Their raw bindings map is
        // empty here because none of the validator rules in the
        // binding-driven dispatch model consults running consumers'
        // bindings — rule 7 (stack-wide instance_id uniqueness) only
        // needs `instance_id`s, and rules 1–5 / 6 are about the
        // new invocation's bindings. The resolved `slot_bindings` is
        // still surfaced through `node_info` for diagnostics and
        // future cross-CLI checks; the validator's
        // `BindingValidationItem` shape doesn't carry them today.
        let instances: Vec<DeploymentInstance> = node
            .instances
            .iter()
            .filter(|inst| inst.state == core_node_api::InstanceState::Running)
            .filter_map(|inst| {
                Name::new(inst.instance_id.clone())
                    .ok()
                    .map(|id| DeploymentInstance {
                        instance_id: id,
                        bindings: BTreeMap::new(),
                        arguments: BTreeMap::new(),
                        env_vars: BTreeMap::new(),
                        framework: config::launcher::FrameworkOverrides::default(),
                    })
            })
            .collect();
        snapshot.push(StackNode {
            name: node.name.clone(),
            tag: node.tag.clone(),
            instances,
            depends_on: info.config.manifest.depends_on,
            conforms_to: info.config.interfaces.conforms_to.unwrap_or_default(),
        });
    }

    // Synthesize a `DeploymentInstance` for the consumer we're about to
    // launch and append it to its `(name, tag)` group (or create the
    // group if the target node isn't in the stack yet, e.g. when the
    // user is launching the only instance of a freshly-added node).
    let synthetic_instance = DeploymentInstance {
        instance_id: Name::new(target_instance_id.to_owned())
            .map_err(|e| Error::PeppyConfig(e.into()))?,
        bindings: binds.clone(),
        arguments: BTreeMap::new(),
        env_vars: BTreeMap::new(),
        framework: config::launcher::FrameworkOverrides::default(),
    };
    if let Some(group) = snapshot
        .iter_mut()
        .find(|e| e.name == target_name && e.tag == target_tag)
    {
        group.instances.push(synthetic_instance);
    } else {
        // Fetch the target's depends_on so the validator can resolve
        // dead-key / missing-binding rules even for nodes that have no
        // running instance yet.
        let info_response = poll_node_info(
            &NodeInfoRequest::new(target_name.to_owned(), target_tag.to_owned()),
            messenger,
            core_node_name,
            CALLER_INSTANCE_ID,
            core_node_name,
            NODE_INFO_PREFLIGHT_TIMEOUT,
        )
        .await
        .ok()
        .and_then(|r| match r {
            NodeInfoResponse::Found(info) => Some(info),
            NodeInfoResponse::NotInStack => None,
        });
        let (depends_on, conforms_to) = match info_response {
            Some(info) => (
                info.config.manifest.depends_on,
                info.config.interfaces.conforms_to.unwrap_or_default(),
            ),
            None => (None, Vec::new()),
        };
        snapshot.push(StackNode {
            name: target_name.to_owned(),
            tag: target_tag.to_owned(),
            instances: vec![synthetic_instance],
            depends_on,
            conforms_to,
        });
    }

    let items: Vec<BindingValidationItem<'_>> = snapshot
        .iter()
        .map(|s| BindingValidationItem {
            node_name: &s.name,
            node_tag: &s.tag,
            instances: &s.instances,
            depends_on: s.depends_on.as_ref(),
            conforms_to: &s.conforms_to,
        })
        .collect();

    let mut validated = validate_bindings(&items);
    if !validated.errors.is_empty() {
        let msg = validated
            .errors
            .iter()
            .map(|e| e.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        return Err(Error::ExecutionFailed(msg));
    }
    Ok(Some(
        validated
            .slot_bindings
            .remove(target_instance_id)
            .unwrap_or_default(),
    ))
}

/// Shared logic for running a node instance.
/// Used by both `run_node` and `add_node` (when --run is set).
#[allow(clippy::too_many_arguments)]
pub async fn run_instance_async(
    messenger_handle: &MessengerHandle,
    core_node_name: &str,
    node_name: &str,
    tag: &str,
    args: &[(String, String)],
    instance_id: Option<String>,
    slot_bindings: BTreeMap<String, SlotBinding>,
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

    let runtime_config = RuntimeConfig::new(
        messaging_host.as_str(),
        messaging_port,
        NodeInstanceConfig {
            arguments,
            slot_bindings,
            ..NodeInstanceConfig::new(
                Name::new(instance_id.clone()).map_err(|e| Error::PeppyConfig(e.into()))?,
            )
        },
        node_name,
        tag,
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

#[allow(clippy::too_many_arguments)]
pub fn run_node(
    ctx: &Arc<AppContext>,
    node_name: String,
    tag: String,
    args: Vec<(String, String)>,
    instance_id: Option<String>,
    binds: Vec<(String, String)>,
    timeouts: TimeoutConfig,
    build: bool,
) -> Result<()> {
    crate::commands::block_on(run_node_async(
        ctx,
        node_name,
        tag,
        args,
        instance_id,
        binds,
        timeouts,
        build,
    ))
}

#[allow(clippy::too_many_arguments)]
async fn run_node_async(
    ctx: &Arc<AppContext>,
    node_name: String,
    tag: String,
    args: Vec<(String, String)>,
    instance_id: Option<String>,
    binds: Vec<(String, String)>,
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

        match classify_stage(info.stage, &node_name, &tag)? {
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

    // Materialize the instance_id up-front so we can feed it into the
    // binding validator's synthetic `DeploymentInstance` and into
    // `run_instance_async` — they have to agree, otherwise a target-node
    // mismatch error would point at a different instance_id than the one
    // we actually spawn.
    let prelaunch_instance_id = instance_id.clone().unwrap_or_else(|| get_random(rng()));

    let binds_map = binds_to_map(&binds, &prelaunch_instance_id)?;

    let slot_bindings = match validate_binds_against_stack(
        conn.messenger,
        &conn.core_node_name,
        &node_name,
        &tag,
        &prelaunch_instance_id,
        &binds_map,
    )
    .await
    {
        Ok(Some(slot_bindings)) => slot_bindings,
        Ok(None) => BTreeMap::new(),
        Err(e @ Error::ExecutionFailed(_)) => return Err(e),
        Err(e) => {
            debug!("skipping bind validation for {}:{}: {}", node_name, tag, e);
            BTreeMap::new()
        }
    };

    run_instance_async(
        conn.messenger,
        &conn.core_node_name,
        &node_name,
        &tag,
        &args,
        Some(prelaunch_instance_id),
        slot_bindings,
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
            classify_stage(NodeStage::Ready, "n", "t").expect("Ready should classify"),
            BuildDecision::Skip
        );
    }

    #[test]
    fn classify_stage_added_builds() {
        assert_eq!(
            classify_stage(NodeStage::Added, "n", "t").expect("Added should classify"),
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
            classify_stage(NodeStage::Building, "n", "t").expect("Building should classify"),
            BuildDecision::Wait
        );
    }

    #[test]
    fn classify_stage_root_fails_fast() {
        let err = classify_stage(NodeStage::Root, "my_node", "v1")
            .expect_err("Root should fail to classify");
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
}
