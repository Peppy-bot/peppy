use config::AnyType;
use config::launcher::Name;
use config::node::{DependsOn, NodeDependency};
use config::runtime::{NodeInstanceConfig, RuntimeConfig};
use core_node_api::encoding::{
    NodeInfoRequest, NodeInfoResponse, NodeRunFeedback, NodeRunGoal, NodeRunGoalResponse,
    NodeRunResult, StackListRequest,
};
use core_node_api::{InstanceState, NodeStage, SerializedNodeGraph};
use names_generator2::get_random;
use peppylib::MessengerHandle;
use rand::rng;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;
use tokio::time::{Instant, sleep};
use tracing::{debug, info, warn};

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

/// Validates `--link-id` CLI input. Rejects the reserved default segment
/// `_` (which the runtime materializes when no `--link-id` is supplied),
/// plus anything else `Segment::try_link_id` rejects (empty, contains `/`,
/// wildcard sentinels). Returns the validated list with first-seen order
/// preserved and duplicates removed.
pub fn validate_link_ids(input: &[String]) -> std::result::Result<Vec<String>, String> {
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut out = Vec::new();
    for raw in input {
        let trimmed = raw.trim();
        if trimmed == pmi::DEFAULT_LINK_ID {
            return Err(format!(
                "--link-id `{trimmed}` is reserved (use a different identifier)"
            ));
        }
        pmi::Segment::try_link_id(trimmed)
            .map_err(|_| format!("--link-id `{trimmed}` is not a valid wire segment"))?;
        if seen.insert(trimmed.to_string()) {
            out.push(trimmed.to_string());
        }
    }
    Ok(out)
}

/// One consumer-pin that nobody is publishing yet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MissingLinkId {
    pub link_id: String,
    /// Stack nodes that declared this `link_id` against the target —
    /// `(consumer_name, consumer_tag)` pairs.
    pub consumers: Vec<(String, String)>,
}

/// Pure helper that compares consumer-declared `link_id` pins against
/// the set of `link_id`s about to exist (this launch's `--link-id`s
/// plus any already-running instances of the same node). Returns one
/// `MissingLinkId` per unsatisfied pin, with consumers listed in
/// first-seen order and link_ids sorted alphabetically.
///
/// Inputs are intentionally plain so this function is trivially
/// testable without a daemon or messenger.
fn compute_missing_link_ids(
    consumer_pins: &[(String, String, String)], // (link_id, consumer_name, consumer_tag)
    available_link_ids: &BTreeSet<String>,
) -> Vec<MissingLinkId> {
    // Group pins by link_id (sorted) while preserving first-seen consumer order.
    let mut grouped: BTreeMap<String, Vec<(String, String)>> = BTreeMap::new();
    for (link_id, consumer_name, consumer_tag) in consumer_pins {
        if available_link_ids.contains(link_id) {
            continue;
        }
        let entry = grouped.entry(link_id.clone()).or_default();
        let pair = (consumer_name.clone(), consumer_tag.clone());
        if !entry.contains(&pair) {
            entry.push(pair);
        }
    }
    grouped
        .into_iter()
        .map(|(link_id, consumers)| MissingLinkId { link_id, consumers })
        .collect()
}

/// Bundle returned by [`gather_missing_consumer_link_ids`] — the pins
/// the new instance can't cover plus the link_ids already advertised
/// by Running instances of the same node, both needed to render the
/// warning body.
struct ConsumerLinkIdCheck {
    missing: Vec<MissingLinkId>,
    existing_link_ids: BTreeSet<String>,
}

/// Best-effort check that fires before the actual `node_run` goal. If
/// any stack consumer pins the target node with a `link_id` that no
/// instance (new or already-running) advertises, returns one
/// `MissingLinkId` per pin. Returns an empty `missing` vec on success
/// with no gaps. Returns an error only for unrecoverable transport
/// failures — the call site logs and swallows it so the run still
/// proceeds.
async fn gather_missing_consumer_link_ids(
    messenger: &MessengerHandle,
    core_node_name: &str,
    target_name: &str,
    target_tag: &str,
    new_link_ids: &[String],
) -> Result<ConsumerLinkIdCheck> {
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

    // Available link_ids = those advertised by Running instances of the
    // target node + the ones the operator just supplied for this launch.
    let mut existing_link_ids: BTreeSet<String> = BTreeSet::new();
    for node in &graph.nodes {
        if node.name == target_name && node.tag == target_tag {
            for inst in &node.instances {
                if inst.state == InstanceState::Running {
                    existing_link_ids.extend(inst.link_ids.iter().cloned());
                }
            }
        }
    }
    let mut available: BTreeSet<String> = new_link_ids.iter().cloned().collect();
    available.extend(existing_link_ids.iter().cloned());

    // For every other node in the stack, fetch its full config and scan
    // its `depends_on.nodes` for pins targeting (target_name, target_tag).
    // Skip Root entities — those are the daemon's own internals.
    let mut consumer_pins: Vec<(String, String, String)> = Vec::new();
    for node in &graph.nodes {
        if node.name == target_name && node.tag == target_tag {
            continue;
        }
        if matches!(node.stage, Some(NodeStage::Root)) {
            continue;
        }
        let info_response = poll_node_info(
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

        let info = match info_response {
            NodeInfoResponse::Found(info) => info,
            NodeInfoResponse::NotInStack => continue,
        };
        let Some(depends_on) = info.config.manifest.depends_on.as_ref() else {
            continue;
        };
        collect_target_pins(
            depends_on,
            target_name,
            target_tag,
            &node.name,
            &node.tag,
            &mut consumer_pins,
        );
    }

    Ok(ConsumerLinkIdCheck {
        missing: compute_missing_link_ids(&consumer_pins, &available),
        existing_link_ids,
    })
}

/// Append `(link_id, consumer_name, consumer_tag)` tuples to `pins` for
/// each entry in `depends_on.nodes` that pins `(target_name,
/// target_tag)` with a non-wildcard, non-default `link_id`. Interface
/// deps are intentionally not handled here; see plan §"Out of scope".
fn collect_target_pins(
    depends_on: &DependsOn,
    target_name: &str,
    target_tag: &str,
    consumer_name: &str,
    consumer_tag: &str,
    pins: &mut Vec<(String, String, String)>,
) {
    for dep in &depends_on.nodes {
        if pin_targets(dep, target_name, target_tag) {
            pins.push((
                dep.link_id.clone(),
                consumer_name.to_owned(),
                consumer_tag.to_owned(),
            ));
        }
    }
}

/// Returns `true` when the dep is a concrete pin against `(name, tag)`
/// — non-wildcard, non-default-sentinel.
fn pin_targets(dep: &NodeDependency, target_name: &str, target_tag: &str) -> bool {
    dep.name.as_str() == target_name
        && dep.tag == target_tag
        && !dep.from_any
        && dep.link_id != pmi::DEFAULT_LINK_ID
}

/// Renders the warning body shown to the operator. Splits out so the
/// content can be asserted on in unit tests without going through the
/// tracing layer.
fn format_missing_link_ids_warning(
    target_name: &str,
    target_tag: &str,
    missing: &[MissingLinkId],
    new_link_ids: &[String],
    existing_link_ids: &BTreeSet<String>,
) -> String {
    use std::fmt::Write;
    let mut out = format!(
        "{}:{} has stack consumers that expect link_ids no instance is publishing:\n",
        target_name, target_tag
    );
    for entry in missing {
        let consumers = entry
            .consumers
            .iter()
            .map(|(n, t)| format!("{n}:{t}"))
            .collect::<Vec<_>>()
            .join(", ");
        let _ = writeln!(
            out,
            "  - link_id `{}` — required by {}",
            entry.link_id, consumers
        );
    }
    if !new_link_ids.is_empty() {
        let _ = writeln!(
            out,
            "This instance will publish under: [{}]",
            new_link_ids.join(", ")
        );
    }
    if !existing_link_ids.is_empty() {
        let _ = writeln!(
            out,
            "Existing running instances publish under: [{}]",
            existing_link_ids
                .iter()
                .cloned()
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    out.push_str("Launch additional ");
    out.push_str(target_name);
    out.push(':');
    out.push_str(target_tag);
    out.push_str(" instances with the missing --link-id values to satisfy these consumers.");
    out
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
    link_ids: Vec<String>,
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
            arguments,
            link_ids,
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
    link_ids: Vec<String>,
    timeouts: TimeoutConfig,
    build: bool,
) -> Result<()> {
    crate::commands::block_on(run_node_async(
        ctx,
        node_name,
        tag,
        args,
        instance_id,
        link_ids,
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
    link_ids: Vec<String>,
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

    emit_missing_consumer_link_ids_warning(
        conn.messenger,
        &conn.core_node_name,
        &node_name,
        &tag,
        &link_ids,
    )
    .await;

    run_instance_async(
        conn.messenger,
        &conn.core_node_name,
        &node_name,
        &tag,
        &args,
        instance_id,
        link_ids,
        &remaining_timeouts(&timeouts, start, "run")?,
    )
    .await?;

    Ok(())
}

/// Runs the consumer-pin check and emits a single `warn!` if any pins
/// are missing. Swallows errors — the warning is informational, not
/// load-bearing.
async fn emit_missing_consumer_link_ids_warning(
    messenger: &MessengerHandle,
    core_node_name: &str,
    target_name: &str,
    target_tag: &str,
    new_link_ids: &[String],
) {
    match gather_missing_consumer_link_ids(
        messenger,
        core_node_name,
        target_name,
        target_tag,
        new_link_ids,
    )
    .await
    {
        Ok(check) if !check.missing.is_empty() => {
            let body = format_missing_link_ids_warning(
                target_name,
                target_tag,
                &check.missing,
                new_link_ids,
                &check.existing_link_ids,
            );
            warn!("{}", body);
        }
        Ok(_) => {}
        Err(e) => {
            debug!(
                "skipping consumer-link_id warning for {}:{}: {}",
                target_name, target_tag, e
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_link_ids_accepts_single_value() {
        let parsed = validate_link_ids(&["wrist_left_camera".to_string()]).expect("should accept");
        assert_eq!(parsed, vec!["wrist_left_camera".to_string()]);
    }

    #[test]
    fn validate_link_ids_accepts_multiple_values() {
        let parsed = validate_link_ids(&[
            "wrist_left_camera".to_string(),
            "wrist_right_camera".to_string(),
            "torso_camera".to_string(),
        ])
        .expect("should accept");
        assert_eq!(
            parsed,
            vec![
                "wrist_left_camera".to_string(),
                "wrist_right_camera".to_string(),
                "torso_camera".to_string(),
            ]
        );
    }

    #[test]
    fn validate_link_ids_deduplicates_preserving_first_seen_order() {
        let parsed = validate_link_ids(&[
            "wrist_left_camera".to_string(),
            "wrist_right_camera".to_string(),
            "wrist_left_camera".to_string(),
            "torso_camera".to_string(),
        ])
        .expect("should accept");
        assert_eq!(
            parsed,
            vec![
                "wrist_left_camera".to_string(),
                "wrist_right_camera".to_string(),
                "torso_camera".to_string(),
            ]
        );
    }

    #[test]
    fn validate_link_ids_rejects_empty_string() {
        let err = validate_link_ids(&["".to_string()]).expect_err("empty should error");
        assert!(err.contains("valid wire segment"), "msg: {err}");
    }

    #[test]
    fn validate_link_ids_rejects_whitespace_only() {
        let err = validate_link_ids(&["   ".to_string()]).expect_err("whitespace should error");
        assert!(err.contains("valid wire segment"), "msg: {err}");
    }

    #[test]
    fn validate_link_ids_rejects_underscore_sentinel() {
        let err = validate_link_ids(&["_".to_string()]).expect_err("`_` should error");
        assert!(err.contains("reserved"), "msg: {err}");
    }

    #[test]
    fn validate_link_ids_rejects_wildcard_sentinel() {
        let err = validate_link_ids(&["*".to_string()]).expect_err("`*` should error");
        assert!(err.contains("valid wire segment"), "msg: {err}");
    }

    #[test]
    fn validate_link_ids_rejects_segment_containing_slash() {
        let err = validate_link_ids(&["wrist/left".to_string()])
            .expect_err("`/` in segment should error");
        assert!(err.contains("valid wire segment"), "msg: {err}");
    }

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

    fn pin(link_id: &str, consumer_name: &str, consumer_tag: &str) -> (String, String, String) {
        (
            link_id.to_owned(),
            consumer_name.to_owned(),
            consumer_tag.to_owned(),
        )
    }

    fn available(items: &[&str]) -> BTreeSet<String> {
        items.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn compute_missing_link_ids_returns_empty_when_no_consumer_pins() {
        let missing = compute_missing_link_ids(&[], &available(&["main"]));
        assert!(missing.is_empty());
    }

    #[test]
    fn compute_missing_link_ids_returns_empty_when_pin_is_covered_by_new_link_id() {
        let pins = vec![pin("front_left", "perception", "v3")];
        let missing = compute_missing_link_ids(&pins, &available(&["front_left"]));
        assert!(
            missing.is_empty(),
            "front_left should be satisfied: {missing:?}"
        );
    }

    #[test]
    fn compute_missing_link_ids_reports_uncovered_pin() {
        let pins = vec![pin("front_right", "perception", "v3")];
        let missing = compute_missing_link_ids(&pins, &available(&["main"]));
        assert_eq!(
            missing,
            vec![MissingLinkId {
                link_id: "front_right".to_owned(),
                consumers: vec![("perception".to_owned(), "v3".to_owned())],
            }]
        );
    }

    #[test]
    fn compute_missing_link_ids_groups_consumers_under_same_link_id() {
        let pins = vec![
            pin("front_right", "perception", "v3"),
            pin("front_right", "recorder", "v1"),
        ];
        let missing = compute_missing_link_ids(&pins, &available(&[]));
        assert_eq!(
            missing.len(),
            1,
            "duplicate link_id should group: {missing:?}"
        );
        assert_eq!(missing[0].link_id, "front_right");
        assert_eq!(
            missing[0].consumers,
            vec![
                ("perception".to_owned(), "v3".to_owned()),
                ("recorder".to_owned(), "v1".to_owned()),
            ],
            "consumers should preserve first-seen order"
        );
    }

    #[test]
    fn compute_missing_link_ids_dedupes_identical_consumer_entries() {
        let pins = vec![
            pin("front_right", "perception", "v3"),
            pin("front_right", "perception", "v3"),
        ];
        let missing = compute_missing_link_ids(&pins, &available(&[]));
        assert_eq!(missing.len(), 1);
        assert_eq!(
            missing[0].consumers.len(),
            1,
            "identical consumer should dedupe"
        );
    }

    #[test]
    fn compute_missing_link_ids_orders_results_alphabetically_by_link_id() {
        let pins = vec![
            pin("zebra", "consumer_z", "v1"),
            pin("apple", "consumer_a", "v1"),
        ];
        let missing = compute_missing_link_ids(&pins, &available(&[]));
        let link_ids: Vec<&str> = missing.iter().map(|m| m.link_id.as_str()).collect();
        assert_eq!(link_ids, vec!["apple", "zebra"]);
    }

    #[test]
    fn pin_targets_accepts_concrete_pin_against_node() {
        let dep = NodeDependency {
            name: config::node::Name::new("my_node").unwrap(),
            tag: "v1".to_string(),
            link_id: "front_left".to_string(),
            from_any: false,
        };
        assert!(pin_targets(&dep, "my_node", "v1"));
    }

    #[test]
    fn pin_targets_rejects_from_any_dep() {
        let dep = NodeDependency {
            name: config::node::Name::new("my_node").unwrap(),
            tag: "v1".to_string(),
            link_id: "front_left".to_string(),
            from_any: true,
        };
        assert!(!pin_targets(&dep, "my_node", "v1"));
    }

    #[test]
    fn pin_targets_rejects_default_sentinel() {
        let dep = NodeDependency {
            name: config::node::Name::new("my_node").unwrap(),
            tag: "v1".to_string(),
            link_id: pmi::DEFAULT_LINK_ID.to_string(),
            from_any: false,
        };
        assert!(!pin_targets(&dep, "my_node", "v1"));
    }

    #[test]
    fn pin_targets_rejects_other_node() {
        let dep = NodeDependency {
            name: config::node::Name::new("other_node").unwrap(),
            tag: "v1".to_string(),
            link_id: "front_left".to_string(),
            from_any: false,
        };
        assert!(!pin_targets(&dep, "my_node", "v1"));
    }

    #[test]
    fn format_missing_link_ids_warning_renders_full_body() {
        let missing = vec![
            MissingLinkId {
                link_id: "front_left".to_owned(),
                consumers: vec![("perception".to_owned(), "v3".to_owned())],
            },
            MissingLinkId {
                link_id: "front_right".to_owned(),
                consumers: vec![
                    ("perception".to_owned(), "v3".to_owned()),
                    ("recorder".to_owned(), "v1".to_owned()),
                ],
            },
        ];
        let new_link_ids = vec!["main".to_owned()];
        let existing: BTreeSet<String> = ["aux".to_owned()].into_iter().collect();
        let body =
            format_missing_link_ids_warning("my_node", "v1", &missing, &new_link_ids, &existing);
        assert!(
            body.contains("my_node:v1"),
            "body should name target: {body}"
        );
        assert!(
            body.contains("front_left"),
            "body should list missing link_id: {body}"
        );
        assert!(
            body.contains("front_right"),
            "body should list missing link_id: {body}"
        );
        assert!(
            body.contains("perception:v3"),
            "body should name consumer: {body}"
        );
        assert!(
            body.contains("recorder:v1"),
            "body should name second consumer: {body}"
        );
        assert!(
            body.contains("This instance will publish under: [main]"),
            "body should report new link_ids: {body}"
        );
        assert!(
            body.contains("Existing running instances publish under: [aux]"),
            "body should report existing link_ids: {body}"
        );
        assert!(
            body.contains("--link-id"),
            "body should hint at the CLI flag: {body}"
        );
    }

    #[test]
    fn format_missing_link_ids_warning_omits_empty_published_lines() {
        let missing = vec![MissingLinkId {
            link_id: "front_left".to_owned(),
            consumers: vec![("perception".to_owned(), "v3".to_owned())],
        }];
        let body =
            format_missing_link_ids_warning("my_node", "v1", &missing, &[], &BTreeSet::new());
        assert!(
            !body.contains("This instance will publish under"),
            "empty new_link_ids should omit the line: {body}"
        );
        assert!(
            !body.contains("Existing running instances publish under"),
            "empty existing_link_ids should omit the line: {body}"
        );
    }
}
