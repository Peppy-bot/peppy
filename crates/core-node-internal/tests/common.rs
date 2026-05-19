#![allow(dead_code)]

use config::consts::{
    DEFAULT_MESSAGING_HOST, NODE_CONFIG_FILE, PEPPY_OUTPUT_DIR, PEPPYGEN_OUTPUT_PATH, PeppyDirs,
};
use config::node::{PeppygenLanguage, QoSProfile};
use core_node::names;
use core_node::nodes_repo_cache_path;
use core_node::{CoreNode, CoreNodeArguments};
use core_node_api::encoding::{
    ClockRequest, ClockResponse, ClockTick, NodeAddFeedback, NodeAddGoal, NodeAddGoalResponse,
    NodeAddResult, NodeBuildFeedback, NodeBuildGoal, NodeBuildGoalResponse, NodeBuildResult,
    NodeRunFeedback, NodeRunGoal, NodeRunGoalResponse, NodeRunResult, NodeSource, wall_now_ns,
};
use gix_url::Url as GitUrl;
use node_stack::NodeStack;
use peppylib::messaging::{MessengerHandle, SenderTarget, TopicMessenger};
use peppylib::runtime::{TaskHandle, spawn};
use peppylib::{ActionMessenger, PeppyError, ServiceMessenger};
use pmi::{Messenger, MessengerAdapter, MessengerBackend, MockAdapter};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::Duration;
use tempfile::TempDir;
use tokio::sync::Mutex;
use tokio::sync::mpsc::UnboundedSender;

/// Default tag used by tests when building a [`SenderTarget`]. Matches the
/// `manifest.tag` value the integration test fixtures emit.
pub const TEST_NODE_TAG: &str = "v1";

/// Builds a node-shaped [`SenderTarget`] with the standard test tag. Panics on
/// invalid names — tests use known-good values only.
pub fn test_node_target(name: &str) -> SenderTarget {
    SenderTarget::node(name, TEST_NODE_TAG).expect("test node target")
}

/// Builds a node-shaped [`SenderTarget`] tagged with [`names::CORE_NODE_TAG`].
/// Use this when the test caller is addressing one of the daemon's own services
/// (clock, info, ping, node_add, …) — the daemon's listeners pin their tag to
/// `CORE_NODE_TAG`, not the `v1` used for ordinary test nodes.
pub fn core_node_target(name: &str) -> SenderTarget {
    SenderTarget::node(name, names::CORE_NODE_TAG).expect("core node target")
}

/// Returns `Ok(())` if the payload is a "result pending" sentinel, or `Err` with a
/// decode-failure message otherwise.
fn check_pending_or_decode_error(
    payload: &[u8],
    err: impl std::fmt::Display,
) -> Result<(), String> {
    if peppylib::encoding::is_result_pending(payload) {
        Ok(())
    } else {
        Err(format!("Failed to decode result: {}", err))
    }
}

/// A wrapper around `TaskHandle` that aborts the task when dropped.
pub struct AbortOnDrop<T>(pub TaskHandle<T>);

impl<T> Drop for AbortOnDrop<T> {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Polls `ServiceMessenger::is_reachable` until the named service responds or
/// `deadline` expires. Replaces fixed sleeps used as broker-propagation
/// barriers in tests that spawn a `handle_requests` task and then need to
/// be sure callers can route to it.
pub async fn wait_until_service_reachable(
    messenger: &MessengerHandle,
    bound_core_node: &str,
    to_node_name: &str,
    to_service_name: &str,
    to_core_node: &str,
    to_instance_id: &str,
    timeout: Duration,
) {
    use peppylib::messaging::ServiceMessenger;
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if let Ok(true) = ServiceMessenger::is_reachable(
            messenger,
            bound_core_node,
            "ready_probe",
            test_node_target(to_node_name),
            to_service_name,
            Some(to_core_node),
            Some(to_instance_id),
        )
        .await
        {
            return;
        }
        if std::time::Instant::now() >= deadline {
            panic!(
                "service {to_node_name}/{to_service_name} on \
                 {to_core_node}/{to_instance_id} did not become \
                 reachable within {timeout:?}"
            );
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// Drives the NTP-style 4-timestamp exchange against the started core node and
/// asserts the wire contract: server echoes `t0` unchanged, and the causal
/// chain `t0 ≤ t1 ≤ t2 ≤ t3` holds. Shared between the mock-messenger and
/// real-zenoh round-trip tests.
pub async fn assert_clock_round_trip(started: &StartedCoreNode) {
    let t0 = wall_now_ns().expect("system clock should be available");
    let request_payload = ClockRequest::new(t0)
        .encode()
        .expect("encode should succeed");

    let response = ServiceMessenger::poll(
        &started.caller_handle,
        &started.core_node_name,
        CALLER_INSTANCE_ID,
        core_node_target(&started.core_node_name),
        names::CLOCK,
        Some(&started.core_node_name),
        None,
        request_payload,
        Duration::from_secs(5),
    )
    .await
    .expect("clock service poll should succeed");

    let t3 = wall_now_ns().expect("system clock should be available");
    let clock_response = ClockResponse::decode(&response.payload()).expect("decode should succeed");

    assert_eq!(
        clock_response.client_send_time, t0,
        "server should echo client_send_time unchanged"
    );
    // Causal chain t0 ≤ t1 ≤ t2 ≤ t3 catches both unit mismatches (ns vs ms)
    // and t1/t2 stamping-order regressions in one assert.
    assert!(
        t0 <= clock_response.server_recv_time
            && clock_response.server_recv_time <= clock_response.server_send_time
            && clock_response.server_send_time <= t3,
        "expected t0 ({}) ≤ t1 ({}) ≤ t2 ({}) ≤ t3 ({})",
        t0,
        clock_response.server_recv_time,
        clock_response.server_send_time,
        t3,
    );
}

/// Subscribes to the `clock` topic, collects three consecutive `ClockTick`s,
/// and asserts they are strictly monotonic. Shared between the mock-messenger
/// and real-zenoh publish tests.
pub async fn assert_clock_topic_emits_monotonic_ticks(
    started: &StartedCoreNode,
    caller_core_node: &str,
    caller_instance_id: &str,
    tick_timeout: Duration,
) {
    let mut subscription = TopicMessenger::subscribe(
        &started.caller_handle,
        caller_core_node,
        caller_instance_id,
        Some(core_node_target(&started.core_node_name)),
        names::CLOCK,
        Some(&started.core_node_name),
        None,
        QoSProfile::SensorData,
    )
    .await
    .expect("clock topic subscription should succeed");

    let mut times = Vec::with_capacity(3);
    for _ in 0..3 {
        let message = tokio::time::timeout(tick_timeout, subscription.on_next_message())
            .await
            .unwrap_or_else(|_| panic!("clock tick should arrive within {tick_timeout:?}"))
            .expect("subscription should not close");

        let tick = ClockTick::decode(message.payload().as_ref())
            .expect("clock tick decode should succeed");
        times.push(tick.time);
    }

    // Strict (not non-strict) so a publisher that re-emits the same payload
    // doesn't silently pass.
    assert!(
        times.windows(2).all(|w| w[0] < w[1]),
        "clock ticks should be strictly monotonic, got {times:?}",
    );
}

fn init_test_data_dir() -> (TempDir, PeppyDirs) {
    // Place test data under $HOME so paths are visible inside the Lima VM on macOS.
    // Lima 2.0+ only mounts ~ into the guest; system temp (/var/folders/...) is inaccessible.
    let home = std::env::var("HOME").expect("HOME must be set");
    let test_tmp_root = std::path::PathBuf::from(&home).join(".peppy/test-tmp");
    std::fs::create_dir_all(&test_tmp_root).expect("create ~/.peppy/test-tmp/");
    let dir = TempDir::new_in(&test_tmp_root).expect("test data dir");
    let peppy_dirs = PeppyDirs::new(dir.path());
    (dir, peppy_dirs)
}

pub const CALLER_INSTANCE_ID: &str = "caller_instance";
pub const TEST_GIT_HASH: &str = "test-hash";

/// Source for a node to be added. Used by `send_node_add_and_wait` to support
/// filesystem paths, git repositories, and HTTP URLs.
#[derive(Debug)]
pub enum NodeAddSource<'a> {
    /// Add from a local filesystem path.
    Path(&'a Path),
    /// Add from a git repository.
    Git {
        repo_url: GitUrl,
        repo_path: &'a str,
        repo_ref: Option<&'a str>,
    },
    /// Add from an HTTP URL (for .tzst archives).
    Http {
        url: url::Url,
        sha256: Option<String>,
    },
    /// Add a node by `(name, tag)` against the repo cache — the daemon
    /// resolves transitive deps from `~/.peppy/cache/nodes.json5`.
    RepoNode { name: &'a str, tag: &'a str },
}

impl<'a> From<&'a Path> for NodeAddSource<'a> {
    fn from(path: &'a Path) -> Self {
        NodeAddSource::Path(path)
    }
}

impl<'a> From<&'a PathBuf> for NodeAddSource<'a> {
    fn from(path: &'a PathBuf) -> Self {
        NodeAddSource::Path(path.as_path())
    }
}

pub struct NodeRunTestTimeouts {
    pub goal: Duration,
    pub result: Duration,
}

/// Combined response from send_node_run_and_wait containing both goal and result responses.
pub struct NodeRunTestResponse {
    pub goal_response: NodeRunGoalResponse,
    pub result: NodeRunResult,
}

/// Builds a `RuntimeConfig` from the given parts and returns its JSON5 serialization,
/// ready to be passed to a `node_run` request.
pub fn build_runtime_config_json5(
    host: &str,
    port: u16,
    core_node_name: &str,
    node_name: &str,
    node_tag: &str,
    instance_id: &str,
    arguments: std::collections::BTreeMap<String, config::AnyType>,
) -> String {
    let runtime_config = config::runtime::RuntimeConfig::new(
        host,
        port,
        config::runtime::NodeInstanceConfig {
            instance_id: config::launcher::Name::new(instance_id).expect("valid instance id"),
            arguments,
            framework: Default::default(),
            link_ids: Vec::new(),
        },
        node_name,
        node_tag,
        core_node_name,
    )
    .expect("runtime config should be valid");
    serde_json5::to_string(&runtime_config).expect("runtime config should serialize")
}

/// Convenience wrapper around `build_runtime_config_json5` using `127.0.0.1`,
/// the default messaging port, and no node arguments — the shape used by most tests.
pub fn default_runtime_config_json5(
    core_node_name: &str,
    node_name: &str,
    node_tag: &str,
    instance_id: &str,
) -> String {
    build_runtime_config_json5(
        "127.0.0.1",
        config::consts::DEFAULT_MESSAGING_PORT,
        core_node_name,
        node_name,
        node_tag,
        instance_id,
        Default::default(),
    )
}

/// Writes a node config file and the corresponding fingerprint file expected by `node_add`.
pub fn write_peppy_json5(dir: &Path, content: &str) {
    let config_path = dir.join(NODE_CONFIG_FILE);
    std::fs::write(&config_path, content).expect("failed to write peppy.json5");
    config::fingerprint::create_codegen_fingerprint(&config_path, Path::new(PEPPYGEN_OUTPUT_PATH));
}

pub fn create_tar_zst_from_dir(source_dir: &Path, archive_path: &Path, archive_root_name: &str) {
    let bundle_file = std::fs::File::create(archive_path).expect("failed to create bundle file");
    let encoder =
        zstd::stream::write::Encoder::new(bundle_file, 0).expect("failed to create zstd encoder");
    let mut tar_builder = tar::Builder::new(encoder);
    tar_builder
        .append_dir_all(archive_root_name, source_dir)
        .expect("failed to append source dir to tar");
    tar_builder.finish().expect("failed to finish tar");
    let encoder = tar_builder
        .into_inner()
        .expect("failed to finish tar encoder");
    encoder.finish().expect("failed to finalize zstd stream");
}

#[allow(clippy::too_many_arguments)]
async fn send_node_run_and_wait_internal(
    messenger: &MessengerHandle,
    core_node_name: &str,
    runtime_config_json5: &str,
    node_name: &str,
    tag: &str,
    timeouts: &NodeRunTestTimeouts,
    feedback_tx: Option<UnboundedSender<NodeRunFeedback>>,
    env_vars: Vec<(String, String)>,
) -> Result<NodeRunTestResponse, String> {
    let goal = NodeRunGoal::new(
        runtime_config_json5,
        node_name,
        tag,
        timeouts.result.as_secs(),
    )
    .with_env_vars(env_vars);
    let goal_payload = goal
        .encode()
        .map_err(|e| format!("Failed to encode goal: {}", e))?;

    let mut action_handle = ActionMessenger::send_goal(
        messenger,
        core_node_name,
        CALLER_INSTANCE_ID,
        core_node_target(core_node_name),
        names::NODE_RUN_ACTION,
        Some(core_node_name),
        None,
        goal_payload,
        QoSProfile::default(),
        timeouts.goal,
    )
    .await
    .map_err(|e| format!("Failed to send goal: {}", e))?;

    // Decode the goal response to get log_path
    let goal_response_payload = action_handle.goal_response().payload();
    let goal_response = NodeRunGoalResponse::decode(&goal_response_payload)
        .map_err(|e| format!("Failed to decode goal response: {}", e))?;

    let absolute_deadline = tokio::time::Instant::now() + timeouts.result;
    let mut last_activity = tokio::time::Instant::now();
    let feedback_tx = feedback_tx.as_ref();

    loop {
        // Drain feedback so the publisher doesn't block on a full channel.
        loop {
            let now = tokio::time::Instant::now();
            if now >= absolute_deadline {
                return Err("Timeout waiting for node_run result".to_string());
            }
            if now.duration_since(last_activity) >= timeouts.result {
                return Err("Timeout waiting for node_run result (idle)".to_string());
            }
            let drain_timeout = Duration::from_millis(50);
            match tokio::time::timeout(drain_timeout, action_handle.on_next_feedback()).await {
                Ok(Ok(msg)) => {
                    last_activity = tokio::time::Instant::now();
                    let payload = msg.payload();
                    if let Ok(feedback) = NodeRunFeedback::decode(payload.as_ref())
                        && let Some(tx) = feedback_tx
                    {
                        let _ = tx.send(feedback);
                    }
                }
                Ok(Err(_)) => break,
                Err(_) => break,
            }
        }

        let now = tokio::time::Instant::now();
        if now >= absolute_deadline {
            return Err("Timeout waiting for node_run result".to_string());
        }
        if now.duration_since(last_activity) >= timeouts.result {
            return Err("Timeout waiting for node_run result (idle)".to_string());
        }
        let poll_timeout = Duration::from_millis(200);

        match ActionMessenger::request_result(messenger, &action_handle, poll_timeout).await {
            Ok(msg) => {
                let payload = msg.payload();
                match NodeRunResult::decode(&payload) {
                    Ok(result) => {
                        // Drain any remaining feedback that may have arrived while polling for the
                        // result so callers can reliably assert on stdout/stderr markers.
                        loop {
                            let Ok(Some(msg)) = action_handle.try_next_feedback() else {
                                break;
                            };
                            let payload = msg.payload();
                            if let Ok(feedback) = NodeRunFeedback::decode(payload.as_ref())
                                && let Some(tx) = feedback_tx
                            {
                                let _ = tx.send(feedback);
                            }
                        }
                        return Ok(NodeRunTestResponse {
                            goal_response,
                            result,
                        });
                    }
                    Err(err) => {
                        check_pending_or_decode_error(payload.as_ref(), err)?;
                    }
                }
            }
            Err(PeppyError::ActionResultTimeout { .. }) => {}
            Err(err) => return Err(format!("Failed to get result: {}", err)),
        }

        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[allow(clippy::too_many_arguments)]
async fn send_node_add_and_wait_internal<'a>(
    messenger: &MessengerHandle,
    core_node_name: &str,
    source: impl Into<NodeAddSource<'a>>,
    goal_timeout: Duration,
    result_timeout: Duration,
    feedback_tx: Option<UnboundedSender<NodeAddFeedback>>,
    env_vars: Vec<(String, String)>,
    force: bool,
) -> Result<NodeAddResult, String> {
    let source = source.into();

    let goal = match &source {
        NodeAddSource::Path(path) => {
            // For directory sources, ensure the git hash file exists. Archive sources must
            // already contain the expected git hash within the bundle.
            if path.is_dir() {
                let peppy_dir = path.join(PEPPY_OUTPUT_DIR);
                std::fs::create_dir_all(&peppy_dir).map_err(|e| {
                    format!(
                        "Failed to create peppy output dir {}: {}",
                        peppy_dir.display(),
                        e
                    )
                })?;
                let git_hash_path = peppy_dir.join("git.hash");
                if !git_hash_path.exists() {
                    std::fs::write(&git_hash_path, TEST_GIT_HASH).map_err(|e| {
                        format!(
                            "Failed to write git hash file {}: {}",
                            git_hash_path.display(),
                            e
                        )
                    })?;
                }
            }
            NodeAddGoal::new(path, TEST_GIT_HASH, result_timeout.as_secs())
        }
        NodeAddSource::Git {
            repo_url,
            repo_path,
            repo_ref,
        } => NodeAddGoal::new_git(
            repo_url.clone(),
            *repo_path,
            repo_ref.map(str::to_owned),
            TEST_GIT_HASH,
            result_timeout.as_secs(),
        ),
        NodeAddSource::Http { url, sha256 } => NodeAddGoal::new_http(
            url.clone(),
            sha256.clone(),
            TEST_GIT_HASH,
            result_timeout.as_secs(),
        ),
        NodeAddSource::RepoNode { name, tag } => {
            let src = NodeSource::repo_node(*name, *tag)
                .map_err(|e| format!("invalid repo-node source in test: {e}"))?;
            NodeAddGoal::from_source(src, TEST_GIT_HASH, result_timeout.as_secs())
        }
    }
    .with_env_vars(env_vars)
    .with_force(force);

    let goal_payload = goal
        .encode()
        .map_err(|e| format!("Failed to encode goal: {}", e))?;

    let mut action_handle = ActionMessenger::send_goal(
        messenger,
        core_node_name,
        CALLER_INSTANCE_ID,
        core_node_target(core_node_name),
        names::NODE_ADD_ACTION,
        Some(core_node_name),
        None,
        goal_payload,
        QoSProfile::default(),
        goal_timeout,
    )
    .await
    .map_err(|e| format!("Failed to send goal: {}", e))?;

    // Check if the goal was rejected - if so, return a failure result immediately.
    // This matches the behavior of the CLI client which doesn't poll for results
    // when the goal is rejected.
    let goal_response_payload = action_handle.goal_response().payload();
    let goal_response = NodeAddGoalResponse::decode(&goal_response_payload)
        .map_err(|e| format!("Failed to decode goal response: {}", e))?;

    if !goal_response.accepted {
        return Ok(NodeAddResult::failure(
            PathBuf::new(),
            goal_response
                .rejection_reason
                .unwrap_or_else(|| "Goal rejected without reason".to_string()),
        ));
    }

    let absolute_deadline = tokio::time::Instant::now() + result_timeout;
    let mut last_activity = tokio::time::Instant::now();
    let feedback_tx = feedback_tx.as_ref();

    loop {
        // Drain feedback so the publisher doesn't block on a full channel.
        loop {
            let now = tokio::time::Instant::now();
            if now >= absolute_deadline {
                return Err("Timeout waiting for node_add result".to_string());
            }
            if now.duration_since(last_activity) >= result_timeout {
                return Err("Timeout waiting for node_add result (idle)".to_string());
            }
            let drain_timeout = Duration::from_millis(50);
            match tokio::time::timeout(drain_timeout, action_handle.on_next_feedback()).await {
                Ok(Ok(msg)) => {
                    last_activity = tokio::time::Instant::now();
                    let payload = msg.payload();
                    if let Ok(feedback) = NodeAddFeedback::decode(payload.as_ref())
                        && let Some(tx) = feedback_tx
                    {
                        let _ = tx.send(feedback);
                    }
                }
                Ok(Err(_)) => break,
                Err(_) => break,
            }
        }

        let now = tokio::time::Instant::now();
        if now >= absolute_deadline {
            return Err("Timeout waiting for node_add result".to_string());
        }
        if now.duration_since(last_activity) >= result_timeout {
            return Err("Timeout waiting for node_add result (idle)".to_string());
        }
        let poll_timeout = Duration::from_millis(200);

        match ActionMessenger::request_result(messenger, &action_handle, poll_timeout).await {
            Ok(msg) => {
                let payload = msg.payload();
                match NodeAddResult::decode(&payload) {
                    Ok(result) => {
                        // Drain any remaining feedback that may have arrived while polling for the
                        // result so callers can reliably assert on stdout/stderr markers.
                        loop {
                            let Ok(Some(msg)) = action_handle.try_next_feedback() else {
                                break;
                            };
                            let payload = msg.payload();
                            if let Ok(feedback) = NodeAddFeedback::decode(payload.as_ref())
                                && let Some(tx) = feedback_tx
                            {
                                let _ = tx.send(feedback);
                            }
                        }
                        return Ok(result);
                    }
                    Err(err) => {
                        check_pending_or_decode_error(payload.as_ref(), err)?;
                    }
                }
            }
            Err(PeppyError::ActionResultTimeout { .. }) => {}
            Err(err) => return Err(format!("Failed to get result: {}", err)),
        }

        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[allow(clippy::too_many_arguments)]
pub async fn send_node_build_and_wait(
    messenger: &MessengerHandle,
    core_node_name: &str,
    node_name: &str,
    node_tag: &str,
    goal_timeout: Duration,
    result_timeout: Duration,
    env_vars: Vec<(String, String)>,
    feedback_tx: Option<UnboundedSender<NodeBuildFeedback>>,
) -> Result<NodeBuildResult, String> {
    let goal =
        NodeBuildGoal::new(node_name, node_tag, result_timeout.as_secs()).with_env_vars(env_vars);
    let goal_payload = goal
        .encode()
        .map_err(|e| format!("Failed to encode build goal: {}", e))?;

    let mut action_handle = ActionMessenger::send_goal(
        messenger,
        core_node_name,
        CALLER_INSTANCE_ID,
        core_node_target(core_node_name),
        names::NODE_BUILD_ACTION,
        Some(core_node_name),
        None,
        goal_payload,
        QoSProfile::default(),
        goal_timeout,
    )
    .await
    .map_err(|e| format!("Failed to send build goal: {}", e))?;

    let goal_response_payload = action_handle.goal_response().payload();
    let goal_response = NodeBuildGoalResponse::decode(&goal_response_payload)
        .map_err(|e| format!("Failed to decode build goal response: {}", e))?;

    if !goal_response.accepted {
        return Ok(NodeBuildResult::failure(
            PathBuf::new(),
            goal_response
                .rejection_reason
                .unwrap_or_else(|| "Build goal rejected without reason".to_string()),
        ));
    }
    let feedback_tx = feedback_tx;
    let feedback_tx = feedback_tx.as_ref();

    let absolute_deadline = tokio::time::Instant::now() + result_timeout;
    let mut last_activity = tokio::time::Instant::now();

    loop {
        loop {
            let now = tokio::time::Instant::now();
            if now >= absolute_deadline {
                return Err("Timeout waiting for node_build result".to_string());
            }
            if now.duration_since(last_activity) >= result_timeout {
                return Err("Timeout waiting for node_build result (idle)".to_string());
            }
            let drain_timeout = Duration::from_millis(50);
            match tokio::time::timeout(drain_timeout, action_handle.on_next_feedback()).await {
                Ok(Ok(msg)) => {
                    last_activity = tokio::time::Instant::now();
                    if let Some(tx) = feedback_tx
                        && let Ok(feedback) = NodeBuildFeedback::decode(msg.payload().as_ref())
                    {
                        let _ = tx.send(feedback);
                    }
                }
                Ok(Err(_)) => break,
                Err(_) => break,
            }
        }

        let now = tokio::time::Instant::now();
        if now >= absolute_deadline {
            return Err("Timeout waiting for node_build result".to_string());
        }
        if now.duration_since(last_activity) >= result_timeout {
            return Err("Timeout waiting for node_build result (idle)".to_string());
        }
        let poll_timeout = Duration::from_millis(200);

        match ActionMessenger::request_result(messenger, &action_handle, poll_timeout).await {
            Ok(msg) => {
                let payload = msg.payload();
                match NodeBuildResult::decode(&payload) {
                    Ok(result) => {
                        // Drain any remaining feedback that may have arrived while polling for the
                        // result so callers can reliably assert on stdout/stderr markers.
                        loop {
                            let Ok(Some(msg)) = action_handle.try_next_feedback() else {
                                break;
                            };
                            let payload = msg.payload();
                            if let Ok(feedback) = NodeBuildFeedback::decode(payload.as_ref())
                                && let Some(tx) = feedback_tx
                            {
                                let _ = tx.send(feedback);
                            }
                        }
                        return Ok(result);
                    }
                    Err(err) => {
                        check_pending_or_decode_error(payload.as_ref(), err)?;
                    }
                }
            }
            Err(PeppyError::ActionResultTimeout { .. }) => {}
            Err(err) => return Err(format!("Failed to get build result: {}", err)),
        }

        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Helper function to send a node_add goal and wait for the result.
/// This wraps the action pattern for simpler test usage.
pub async fn send_node_add_and_wait<'a>(
    messenger: &MessengerHandle,
    core_node_name: &str,
    source: impl Into<NodeAddSource<'a>>,
    goal_timeout: Duration,
    result_timeout: Duration,
    feedback_tx: Option<UnboundedSender<NodeAddFeedback>>,
) -> Result<NodeAddResult, String> {
    send_node_add_and_wait_internal(
        messenger,
        core_node_name,
        source,
        goal_timeout,
        result_timeout,
        feedback_tx,
        Vec::new(),
        false,
    )
    .await
}

pub async fn send_node_add_and_wait_with_env<'a>(
    messenger: &MessengerHandle,
    core_node_name: &str,
    source: impl Into<NodeAddSource<'a>>,
    goal_timeout: Duration,
    result_timeout: Duration,
    feedback_tx: Option<UnboundedSender<NodeAddFeedback>>,
    env_vars: Vec<(String, String)>,
) -> Result<NodeAddResult, String> {
    send_node_add_and_wait_internal(
        messenger,
        core_node_name,
        source,
        goal_timeout,
        result_timeout,
        feedback_tx,
        env_vars,
        false,
    )
    .await
}

/// Builder for a `nodes.json5` cache fixture. Tests call
/// [`TestPackagesCache::fs_entry`] / `git_entry` / `http_entry` to declare
/// discovered nodes and then [`TestPackagesCache::write`] to serialize
/// the file under `peppy_dirs.cache_dir()/nodes.json5`.
#[derive(Default)]
pub struct TestPackagesCache {
    entries: Vec<serde_json::Value>,
}

impl TestPackagesCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// `absolute_path` is the directory containing `peppy.json5`. The
    /// cache stores the manifest file path (path-points-at-file
    /// convention), so we join `NODE_CONFIG_FILE` here.
    pub fn fs_entry(mut self, name: &str, tag: &str, absolute_path: impl AsRef<Path>) -> Self {
        let manifest_path = absolute_path.as_ref().join(NODE_CONFIG_FILE);
        let mut m = serde_json::Map::new();
        m.insert("node_name".into(), serde_json::Value::String(name.into()));
        m.insert("node_tag".into(), serde_json::Value::String(tag.into()));
        m.insert("source_type".into(), serde_json::Value::String("fs".into()));
        m.insert(
            "path".into(),
            serde_json::Value::String(manifest_path.to_string_lossy().into_owned()),
        );
        self.entries.push(serde_json::Value::Object(m));
        self
    }

    /// `path_in_repo` is the directory containing `peppy.json5` within
    /// the checked-out repo. We join `NODE_CONFIG_FILE` so the cache
    /// records the manifest file path.
    pub fn git_entry(
        mut self,
        name: &str,
        tag: &str,
        repo_url: &str,
        resolved_ref: &str,
        path_in_repo: &str,
    ) -> Self {
        let manifest_path = Path::new(path_in_repo).join(NODE_CONFIG_FILE);
        let mut m = serde_json::Map::new();
        m.insert("node_name".into(), serde_json::Value::String(name.into()));
        m.insert("node_tag".into(), serde_json::Value::String(tag.into()));
        m.insert(
            "source_type".into(),
            serde_json::Value::String("git".into()),
        );
        m.insert(
            "source_uri".into(),
            serde_json::Value::String(repo_url.into()),
        );
        m.insert(
            "resolved_ref".into(),
            serde_json::Value::String(resolved_ref.into()),
        );
        m.insert(
            "path".into(),
            serde_json::Value::String(manifest_path.to_string_lossy().into_owned()),
        );
        self.entries.push(serde_json::Value::Object(m));
        self
    }

    pub fn write(self, peppy_dirs: &config::consts::PeppyDirs) {
        let cache_dir = peppy_dirs.cache_dir();
        std::fs::create_dir_all(&cache_dir).expect("failed to create cache dir");
        let content =
            serde_json::to_string_pretty(&self.entries).expect("failed to serialize cache entries");
        std::fs::write(nodes_repo_cache_path(peppy_dirs), content)
            .expect("failed to write nodes.json5 fixture");
    }
}

/// Convenience helper — writes `peppy.json5` under `dir` but skips the
/// fingerprint generation (useful for packages-cache FS fixtures that
/// aren't going through the fingerprint verification path).
pub fn write_plain_peppy_json5(dir: &Path, content: &str) {
    std::fs::create_dir_all(dir).expect("failed to create dir");
    std::fs::write(dir.join(NODE_CONFIG_FILE), content).expect("failed to write peppy.json5");
}

/// Convenience helper for tests that staged a node via `send_node_add_and_wait`
/// and now need it built so `spawn_real_running_instance` can find a `Ready`
/// entity. Builds the node and asserts the build succeeded.
pub async fn build_staged_node(started: &StartedCoreNode, node_name: &str, node_tag: &str) {
    let result = send_node_build_and_wait(
        &started.caller_handle,
        &started.core_node_name,
        node_name,
        node_tag,
        Duration::from_secs(30),
        Duration::from_secs(120),
        Vec::new(),
        None,
    )
    .await
    .expect("node_build request should complete");
    assert!(
        result.success,
        "build_staged_node failed: {:?}",
        result.error_message
    );
}

/// Convenience helper for tests that need a node to be both added AND built
/// (e.g. start/info/stop tests). Performs `send_node_add_and_wait` followed by
/// `send_node_build_and_wait` and returns the build result.
pub async fn send_node_add_then_build<'a>(
    messenger: &MessengerHandle,
    core_node_name: &str,
    source: impl Into<NodeAddSource<'a>>,
    goal_timeout: Duration,
    result_timeout: Duration,
) -> Result<NodeBuildResult, String> {
    let add = send_node_add_and_wait_internal(
        messenger,
        core_node_name,
        source,
        goal_timeout,
        result_timeout,
        None,
        Vec::new(),
        false,
    )
    .await?;
    if !add.success {
        return Err(format!(
            "node_add failed: {}",
            add.error_message.unwrap_or_default()
        ));
    }
    let node_name = add.node_name.expect("node_name on successful add");
    let node_tag = add.node_tag.expect("node_tag on successful add");
    let result = send_node_build_and_wait(
        messenger,
        core_node_name,
        &node_name,
        &node_tag,
        goal_timeout,
        result_timeout,
        Vec::new(),
        None,
    )
    .await?;
    if !result.success {
        return Err(format!(
            "node_build failed: {}",
            result.error_message.unwrap_or_default()
        ));
    }
    Ok(result)
}

pub async fn send_node_add_and_wait_with_force<'a>(
    messenger: &MessengerHandle,
    core_node_name: &str,
    source: impl Into<NodeAddSource<'a>>,
    goal_timeout: Duration,
    result_timeout: Duration,
    feedback_tx: Option<UnboundedSender<NodeAddFeedback>>,
) -> Result<NodeAddResult, String> {
    send_node_add_and_wait_internal(
        messenger,
        core_node_name,
        source,
        goal_timeout,
        result_timeout,
        feedback_tx,
        Vec::new(),
        true,
    )
    .await
}

/// Helper function to send a node_run goal and wait for the result.
/// This wraps the action pattern for simpler test usage.
pub async fn send_node_run_and_wait(
    messenger: &MessengerHandle,
    core_node_name: &str,
    runtime_config_json5: &str,
    node_name: &str,
    tag: &str,
    timeouts: &NodeRunTestTimeouts,
    feedback_tx: Option<UnboundedSender<NodeRunFeedback>>,
) -> Result<NodeRunTestResponse, String> {
    send_node_run_and_wait_internal(
        messenger,
        core_node_name,
        runtime_config_json5,
        node_name,
        tag,
        timeouts,
        feedback_tx,
        Vec::new(),
    )
    .await
}

#[allow(clippy::too_many_arguments)]
pub async fn send_node_run_and_wait_with_env(
    messenger: &MessengerHandle,
    core_node_name: &str,
    runtime_config_json5: &str,
    node_name: &str,
    tag: &str,
    timeouts: &NodeRunTestTimeouts,
    feedback_tx: Option<UnboundedSender<NodeRunFeedback>>,
    env_vars: Vec<(String, String)>,
) -> Result<NodeRunTestResponse, String> {
    send_node_run_and_wait_internal(
        messenger,
        core_node_name,
        runtime_config_json5,
        node_name,
        tag,
        timeouts,
        feedback_tx,
        env_vars,
    )
    .await
}

/// Creates a fresh test node in a new temp directory.
/// Each call creates a completely new node with its own peppygen generation
/// and cargo build, ensuring isolation between tests.
pub fn create_test_node() -> PathBuf {
    init_test_node_project("example_node", "v1", true)
}

/// Creates a fresh test node in a new temp directory.
/// Each call creates a completely new node with its own peppygen generation
/// and cargo build, ensuring isolation between tests.
pub fn create_test_node_with_name(node_name: &str, node_tag: &str) -> PathBuf {
    init_test_node_project(node_name, node_tag, true)
}

pub fn init_test_node_project(node_name: &str, node_tag: &str, build_project: bool) -> PathBuf {
    let node_dir = tempfile::Builder::new()
        .prefix("peppy_test_node_")
        .tempdir()
        .expect("failed to create temp directory for test node")
        .keep();

    init_cargo_project(&node_dir, node_name);
    write_test_node_files(&node_dir, node_name, node_tag);

    let peppy_dirs = PeppyDirs::default();
    generator::generate_peppygen_lib(
        PeppygenLanguage::Rust,
        &node_dir,
        Vec::new(),
        "test-hash",
        &peppy_dirs,
        Default::default(),
        None,
    )
    .expect("failed to generate peppygen for test node");

    if build_project {
        build_cargo_project(&node_dir);
    }

    node_dir
}

fn init_cargo_project(node_dir: &Path, crate_name: &str) {
    let output = Command::new("cargo")
        .arg("init")
        .arg("--bin")
        .arg("--vcs")
        .arg("none")
        .arg("--name")
        .arg(crate_name)
        .env("CARGO_NET_OFFLINE", "true")
        .current_dir(node_dir)
        .stdin(Stdio::null())
        .output()
        .expect("failed to invoke `cargo init` for test node");

    assert!(
        output.status.success(),
        "`cargo init` failed with status: {:?}\nstdout:\n{}\nstderr:\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn write_test_node_files(node_dir: &Path, crate_name: &str, node_tag: &str) {
    std::fs::write(
        node_dir.join("Cargo.toml"),
        format!(
            r#"[package]
name = "{crate_name}"
version = "0.1.0"
edition = "2024"

[dependencies]
peppygen = {{ path = "{PEPPYGEN_OUTPUT_PATH}" }}
"#
        ),
    )
    .expect("failed to write test node Cargo.toml");

    std::fs::write(
        node_dir.join("src/main.rs"),
        r#"use peppygen::{NodeBuilder, Parameters, Result};

fn main() -> Result<()> {
    NodeBuilder::new().run(|args: Parameters, node_runner| async {
        let _ = args;
        let _ = node_runner;
        Ok(())
    })
}
"#,
    )
    .expect("failed to write test node src/main.rs");

    // Use the pre-built binary path in run_cmd instead of "cargo run".
    // This avoids recompilation after the folder is copied to storage,
    // since cargo's fingerprinting invalidates the cache when absolute paths change.
    let binary_path = node_dir.join("target/debug").join(crate_name);
    std::fs::write(
        node_dir.join(NODE_CONFIG_FILE),
        r#"{
  peppy_schema: "node_v1",
  manifest: {
    name: "{crate_name}",
    tag: "{node_tag}",
  },
  interfaces: {
    topics: {
      emits: [
        {
          name: "hello_world",
          qos_profile: "sensor_data",
          message_format: {
            timestamp: "time",
            message: "string"
          }
        }
      ],
    }
  },
  // Avoid `build_cmd` build step here to make the `add` tests faster
  execution: {
    language: "rust",
    build_cmd: [
        "true"
    ],
    run_cmd: [
      "{binary_path}"
    ]
  },
}"#
        .replace("{crate_name}", crate_name)
        .replace("{node_tag}", node_tag)
        .replace("{binary_path}", &binary_path.display().to_string()),
    )
    .expect("failed to write test node peppy.json5");
}

fn build_cargo_project(dir: &Path) {
    let output = Command::new("cargo")
        .arg("build")
        .env("CARGO_NET_OFFLINE", "true")
        .current_dir(dir)
        .stdin(Stdio::null())
        .output()
        .expect("failed to invoke `cargo build` for test node");

    assert!(
        output.status.success(),
        "`cargo build` failed with status: {:?}\nstdout:\n{}\nstderr:\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

pub async fn create_mock_messenger() -> Arc<Mutex<Messenger>> {
    let adapter = MockAdapter::default();
    let mut messenger = Messenger::new(MessengerAdapter::Mock(adapter));
    messenger
        .start_session()
        .await
        .expect("failed to start mock session");
    Arc::new(Mutex::new(messenger))
}

#[allow(dead_code)]
pub struct StartedCoreNode {
    pub shared_messenger: Arc<Mutex<Messenger>>,
    pub caller_handle: MessengerHandle,
    pub core_node_name: String,
    pub core_node_tag: String,
    pub node_stack: NodeStack,
    pub peppy_dirs: PeppyDirs,
    pub task: AbortOnDrop<core_node::Result<()>>,
    _data_dir: TempDir,
}

fn default_node_arguments() -> CoreNodeArguments {
    CoreNodeArguments {
        node_startup_timeout: Duration::from_secs(10),
        node_start_health_timeout: Duration::from_secs(30),
        health_monitor_interval: Duration::from_secs(5),
        health_monitor_timeout: Duration::from_secs(3),
        health_monitor_max_failures: 3,
        // Faster than the production default (100 ms) so publish_clock tests
        // observe several ticks within a small fixed budget without flaking.
        clock_publish_interval: Duration::from_millis(50),
        daemon_use_sim_time: false,
    }
}

pub async fn start_core_node_with_mock_messenger() -> StartedCoreNode {
    let (data_dir, peppy_dirs) = init_test_data_dir();
    let shared_messenger = create_mock_messenger().await;
    start_core_node_with_messenger(
        shared_messenger,
        default_node_arguments(),
        data_dir,
        peppy_dirs,
    )
    .await
}

/// Boots the core node with `daemon_use_sim_time: true`. The daemon stops
/// publishing wall ticks and instead subscribes to the `clock` topic to fill
/// its internal cache, mirroring the production flow where an external
/// simulator drives the clock.
pub async fn start_core_node_with_sim_clock() -> StartedCoreNode {
    let (data_dir, peppy_dirs) = init_test_data_dir();
    let shared_messenger = create_mock_messenger().await;
    let mut args = default_node_arguments();
    args.daemon_use_sim_time = true;
    start_core_node_with_messenger(shared_messenger, args, data_dir, peppy_dirs).await
}

pub async fn start_core_node_with_real_messenger() -> StartedCoreNode {
    start_core_node_with_real_messenger_and_timeouts(
        Duration::from_secs(10),
        Duration::from_secs(30),
    )
    .await
}

pub async fn start_core_node_with_real_messenger_and_timeouts(
    node_startup_timeout: Duration,
    node_start_health_timeout: Duration,
) -> StartedCoreNode {
    let (data_dir, peppy_dirs) = init_test_data_dir();
    let mut instance = pmi::ZenohAdapter::start_router_ephemeral(DEFAULT_MESSAGING_HOST, None)
        .await
        .expect("failed to start zenoh router for test");
    instance
        .messenger()
        .start_session()
        .await
        .expect("failed to start zenoh session");
    let shared_messenger = Arc::new(Mutex::new(instance.take_messenger()));
    let mut args = default_node_arguments();
    args.node_startup_timeout = node_startup_timeout;
    args.node_start_health_timeout = node_start_health_timeout;
    start_core_node_with_messenger(shared_messenger, args, data_dir, peppy_dirs).await
}

pub async fn start_core_node_with_health_timeout(
    node_start_health_timeout: Duration,
) -> StartedCoreNode {
    let (data_dir, peppy_dirs) = init_test_data_dir();
    let shared_messenger = create_mock_messenger().await;
    let mut args = default_node_arguments();
    args.node_start_health_timeout = node_start_health_timeout;
    start_core_node_with_messenger(shared_messenger, args, data_dir, peppy_dirs).await
}

pub async fn start_core_node_with_health_monitor(
    health_monitor_interval: Duration,
    health_monitor_timeout: Duration,
    health_monitor_max_failures: u32,
) -> StartedCoreNode {
    let (data_dir, peppy_dirs) = init_test_data_dir();
    let shared_messenger = create_mock_messenger().await;
    let mut args = default_node_arguments();
    args.health_monitor_interval = health_monitor_interval;
    args.health_monitor_timeout = health_monitor_timeout;
    args.health_monitor_max_failures = health_monitor_max_failures;
    start_core_node_with_messenger(shared_messenger, args, data_dir, peppy_dirs).await
}

async fn start_core_node_with_messenger(
    shared_messenger: Arc<Mutex<Messenger>>,
    node_arguments: CoreNodeArguments,
    data_dir: TempDir,
    peppy_dirs: PeppyDirs,
) -> StartedCoreNode {
    let caller_handle = MessengerHandle::from_shared(Arc::clone(&shared_messenger));
    let root_dir = std::env::current_dir().expect("failed to get current directory");
    let core_node = CoreNode::new(
        Arc::clone(&shared_messenger),
        Some("test_core_node"),
        node_arguments,
        root_dir,
        peppy_dirs.clone(),
    );
    let core_node_name = core_node.node_name().to_string();
    let core_node_tag = core_node.node_config().manifest.tag.clone();
    let node_stack = core_node.node_stack().clone();

    // Use start_with_ready to properly synchronize instead of a time-based sleep
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
    let task = spawn(async move { core_node.start_with_ready(Some(ready_tx)).await });

    // Wait for all services to be fully registered before returning
    ready_rx.await.expect("core node ready signal failed");

    StartedCoreNode {
        shared_messenger,
        caller_handle,
        core_node_name,
        core_node_tag,
        node_stack,
        peppy_dirs,
        task: AbortOnDrop(task),
        _data_dir: data_dir,
    }
}

// =============================================================================
// Real-lifecycle test helpers with calls to NodeEntity::build + prepare_and_spawn + commit_started.
// =============================================================================

/// RAII guard for a test-spawned `Running` instance. On drop it calls
/// `stop_instance` on the entity and SIGTERMs the real child process.
#[must_use = "guard keeps the spawned child alive; drop it to tear down the instance"]
pub struct TestRunningInstance {
    pub pid: u32,
    pub instance_id: config::node::Name,
    handle: node_stack::EntityHandle,
    _working_dir: Option<TempDir>,
    _feedback_drain: tokio::task::JoinHandle<()>,
    _shutdown_listener: Option<AbortOnDrop<peppylib::PeppyResult<()>>>,
}

impl Drop for TestRunningInstance {
    fn drop(&mut self) {
        self.handle.write().stop_instance(&self.instance_id);
        let _ = std::process::Command::new("kill")
            .arg("-TERM")
            .arg(self.pid.to_string())
            .status();
        self._feedback_drain.abort();
    }
}

struct NoOpOutputHooks;
impl node_stack::build_io::OutputReaderHooks for NoOpOutputHooks {}

fn make_real_output_sinks(
    peppy_dirs: &PeppyDirs,
    instance_id: &config::node::Name,
) -> (
    node_stack::OutputSinks,
    tokio::sync::mpsc::UnboundedSender<node_stack::build_io::FeedbackLine>,
    tokio::task::JoinHandle<()>,
) {
    use parking_lot::Mutex as StdMutex;
    use std::sync::atomic::AtomicBool;

    let log_dir = peppy_dirs.logs_dir_run();
    std::fs::create_dir_all(&log_dir).ok();
    let log_file = Arc::new(StdMutex::new(
        std::fs::File::create(log_dir.join(format!("{}.log", instance_id.as_str())))
            .expect("create start log"),
    ));
    let (feedback_tx, mut feedback_rx) =
        tokio::sync::mpsc::unbounded_channel::<node_stack::build_io::FeedbackLine>();
    let drain = tokio::spawn(async move { while feedback_rx.recv().await.is_some() {} });
    let output_sinks = node_stack::OutputSinks {
        feedback_tx: feedback_tx.clone(),
        log_file,
        publish_enabled: Arc::new(AtomicBool::new(true)),
        hooks: Arc::new(NoOpOutputHooks),
    };
    (output_sinks, feedback_tx, drain)
}

/// Drives a real `prepare_and_spawn` + `commit_started` on the entity at
/// `(name, tag)`, which must already be in `Ready`. Spawns a real child via
/// the entity's existing `run_cmd` — callers are responsible for ensuring
/// the node config's run_cmd is spawnable in the test environment (the
/// listener tests use `["sleep", "10"]`). Also installs a `listen_for_shutdown`
/// task on the messenger that SIGKILLs the entity-tracked pid when the
/// production stop/remove flow sends a shutdown signal. This lets production
/// code paths that wait on `wait_for_process_termination` observe the child
/// as terminated rather than timing out against a stubborn `sleep 10`.
/// Returns a guard that SIGTERMs the child on drop.
pub async fn spawn_real_running_instance(
    started: &StartedCoreNode,
    name: &str,
    tag: &str,
    instance_id: &config::node::Name,
) -> TestRunningInstance {
    spawn_real_running_instance_inner(started, name, tag, instance_id, true).await
}

/// Variant of [`spawn_real_running_instance`] that skips installing a
/// shutdown listener. Used by tests that specifically want the production
/// shutdown path to observe a stuck process that never terminates (e.g. the
/// `node_add_same_node_with_running_instance_and_dependents_fails_on_stopped_node_stuck`
/// regression test).
pub async fn spawn_real_stuck_instance(
    started: &StartedCoreNode,
    name: &str,
    tag: &str,
    instance_id: &config::node::Name,
) -> TestRunningInstance {
    spawn_real_running_instance_inner(started, name, tag, instance_id, false).await
}

async fn spawn_real_running_instance_inner(
    started: &StartedCoreNode,
    name: &str,
    tag: &str,
    instance_id: &config::node::Name,
    install_shutdown_listener: bool,
) -> TestRunningInstance {
    let handle = started
        .node_stack
        .find(name, tag)
        .expect("spawn_real_running_instance: entity should exist");
    let (output_sinks, _feedback_tx, drain) =
        make_real_output_sinks(&started.peppy_dirs, instance_id);

    let (child, started_ctx) = node_stack::NodeEntity::prepare_and_spawn(
        &handle,
        node_stack::StartContext {
            instance_id,
            runtime_config_json5: "{}",
            env_vars: &[],
            mount_paths_resolved: &[],
            peppy_dirs: &started.peppy_dirs,
            output_sinks,
        },
    )
    .await
    .expect("prepare_and_spawn should succeed on Ready entity");
    let pid = child.id().expect("child should have pid");
    node_stack::NodeEntity::commit_started(&handle, child, started_ctx, instance_id.clone())
        .await
        .expect("commit_started should succeed");

    // Optionally install a messenger-side shutdown listener that kills the
    // child when the production stop/remove flow fires a SHUTDOWN_SERVICE
    // signal. This replaces the old behavior where tests set fake pids so
    // the production `wait_for_process_termination` quickly observed "no
    // such pid". Tests that want the production shutdown path to observe
    // a stuck process use `spawn_real_stuck_instance` which skips this.
    let shutdown_listener = if install_shutdown_listener {
        let shutdown_handle = MessengerHandle::from_shared(Arc::clone(&started.shared_messenger));
        let (shutdown_task, shutdown_rx) = peppylib::services::shutdown::listen_for_shutdown(
            &shutdown_handle,
            &started.core_node_name,
            instance_id.as_str(),
            test_node_target(name),
        )
        .await
        .expect("failed to start shutdown listener for test instance");
        let pid_for_kill = pid;
        tokio::spawn(async move {
            if shutdown_rx.await.is_ok() {
                let _ = std::process::Command::new("kill")
                    .arg("-KILL")
                    .arg(pid_for_kill.to_string())
                    .status();
            }
        });
        Some(AbortOnDrop(shutdown_task))
    } else {
        None
    };

    TestRunningInstance {
        pid,
        instance_id: instance_id.clone(),
        handle,
        _working_dir: None,
        _feedback_drain: drain,
        _shutdown_listener: shutdown_listener,
    }
}

/// For tests that push a config directly (bypassing `process_node_add`): drives
/// the real `NodeEntity::build` (process-node archive path, no container) and
/// then a real `prepare_and_spawn` + `commit_started`. Replaces the old
/// `force_built_and_start_instance` backdoor helper.
pub async fn real_build_and_spawn_instance(
    started: &StartedCoreNode,
    name: &str,
    tag: &str,
    instance_id: &config::node::Name,
) -> TestRunningInstance {
    use parking_lot::Mutex as StdMutex;

    let handle = started
        .node_stack
        .find(name, tag)
        .expect("real_build_and_spawn_instance: entity should exist");

    let working_dir = TempDir::new().expect("working_dir tempdir");
    let log_dir = started.peppy_dirs.logs_dir_add();
    std::fs::create_dir_all(&log_dir).ok();
    let build_log = Arc::new(StdMutex::new(
        std::fs::File::create(log_dir.join(format!("{}-build.log", instance_id.as_str())))
            .expect("create build log"),
    ));
    let (build_feedback_tx, mut build_feedback_rx) =
        tokio::sync::mpsc::unbounded_channel::<node_stack::build_io::FeedbackLine>();
    let build_drain =
        tokio::spawn(async move { while build_feedback_rx.recv().await.is_some() {} });

    node_stack::NodeEntity::build(
        &handle,
        node_stack::BuildContext {
            working_dir: working_dir.path(),
            peppy_dirs: &started.peppy_dirs,
            feedback_tx: &build_feedback_tx,
            log_file: build_log,
            env_vars: &[],
        },
    )
    .await
    .expect("real build should succeed on process node");
    build_drain.abort();

    let mut running = spawn_real_running_instance(started, name, tag, instance_id).await;
    running._working_dir = Some(working_dir);
    running
}
