#![allow(dead_code)]

use config::consts::{
    DEFAULT_MESSAGING_HOST, NODE_CONFIG_FILE, PEPPY_OUTPUT_DIR, PEPPYGEN_OUTPUT_PATH, PeppyDirs,
};
use config::node::{PeppygenLanguage, QoSProfile};
use core_node::encoding::{
    NodeAddFeedback, NodeAddGoal, NodeAddGoalResponse, NodeAddResult, NodeBuildGoal,
    NodeBuildGoalResponse, NodeBuildResult, NodeSource, NodeStartFeedback, NodeStartGoal,
    NodeStartGoalResponse, NodeStartResult,
};
use core_node::names;
use core_node::{CoreNode, CoreNodeArguments};
use gix_url::Url as GitUrl;
use node_stack::NodeStack;
use peppylib::messaging::MessengerHandle;
use peppylib::runtime::{TaskHandle, spawn};
use peppylib::{ActionMessenger, PeppyError};
use pmi::{Messenger, MessengerAdapter, MessengerBackend, MockAdapter};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::Duration;
use tempfile::TempDir;
use tokio::sync::Mutex;
use tokio::sync::mpsc::UnboundedSender;

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

/// Combined result of `node_add` followed (on success) by `node_build`. The
/// daemon side now exposes `add` and `build` as two separate actions; tests
/// almost always want the full pipeline, so the helpers below transparently
/// chain build and merge both results into this struct. `snapshot_path`
/// holds the build artifact path on success and is empty otherwise.
#[derive(Debug, Clone)]
pub struct NodeAddTestResult {
    pub success: bool,
    pub error_message: Option<String>,
    pub node_name: Option<String>,
    pub node_tag: Option<String>,
    pub snapshot_path: PathBuf,
    pub log_path: PathBuf,
}

/// Tiny helper that decodes a `NodeBuildFeedback` payload but exposes it as a
/// `NodeAddFeedback` so the existing test feedback channels keep working
/// during the build phase too.
struct NodeBuildResultFeedbackBridge;

impl NodeBuildResultFeedbackBridge {
    fn decode(payload: &[u8]) -> Result<NodeAddFeedback, String> {
        let feedback =
            core_node::encoding::NodeBuildFeedback::decode(payload).map_err(|e| e.to_string())?;
        Ok(NodeAddFeedback {
            stream: feedback.stream,
            line: feedback.line,
        })
    }
}

impl NodeAddTestResult {
    fn from_add_failure(add: NodeAddResult) -> Self {
        Self {
            success: false,
            error_message: add.error_message,
            node_name: add.node_name,
            node_tag: add.node_tag,
            snapshot_path: PathBuf::new(),
            log_path: add.log_path,
        }
    }

    fn from_build(build: NodeBuildResult) -> Self {
        Self {
            success: build.success,
            error_message: build.error_message,
            node_name: build.node_name,
            node_tag: build.node_tag,
            snapshot_path: build.artifact_path,
            log_path: build.log_path,
        }
    }
}

/// Source for a node to be added. Used by `send_node_add_and_wait` to support
/// filesystem paths, git repositories, and HTTP URLs.
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

pub struct NodeStartTestTimeouts {
    pub goal: Duration,
    pub result: Duration,
}

/// Combined response from send_node_start_and_wait containing both goal and result responses.
pub struct NodeStartTestResponse {
    pub goal_response: NodeStartGoalResponse,
    pub result: NodeStartResult,
}

/// Builds a `RuntimeConfig` from the given parts and returns its JSON5 serialization,
/// ready to be passed to a `node_start` request.
pub fn build_runtime_config_json5(
    host: &str,
    port: u16,
    core_node_name: &str,
    node_name: &str,
    instance_id: &str,
    arguments: std::collections::BTreeMap<String, config::AnyType>,
) -> String {
    let runtime_config = config::runtime::RuntimeConfig::new(
        host,
        port,
        config::runtime::NodeInstanceConfig {
            instance_id: config::launcher::Name::new(instance_id).expect("valid instance id"),
            arguments,
        },
        node_name,
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
    instance_id: &str,
) -> String {
    build_runtime_config_json5(
        "127.0.0.1",
        config::consts::DEFAULT_MESSAGING_PORT,
        core_node_name,
        node_name,
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
async fn send_node_start_and_wait_internal(
    messenger: &MessengerHandle,
    core_node_name: &str,
    runtime_config_json5: &str,
    node_name: &str,
    tag: &str,
    timeouts: &NodeStartTestTimeouts,
    feedback_tx: Option<UnboundedSender<NodeStartFeedback>>,
    env_vars: Vec<(String, String)>,
) -> Result<NodeStartTestResponse, String> {
    let goal = NodeStartGoal::new(
        runtime_config_json5,
        node_name,
        tag,
        timeouts.result.as_secs(),
    )
    .with_env_vars(env_vars);
    let (caller_core_node, caller_instance_id) = if feedback_tx.is_some() {
        ("*", "*")
    } else {
        (core_node_name, CALLER_INSTANCE_ID)
    };
    let goal_payload = goal
        .encode()
        .map_err(|e| format!("Failed to encode goal: {}", e))?;

    let mut action_handle = ActionMessenger::send_goal(
        messenger,
        caller_core_node,
        caller_instance_id,
        core_node_name,
        names::NODE_START_ACTION,
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
    let goal_response = NodeStartGoalResponse::decode(&goal_response_payload)
        .map_err(|e| format!("Failed to decode goal response: {}", e))?;

    let absolute_deadline = tokio::time::Instant::now() + timeouts.result;
    let mut last_activity = tokio::time::Instant::now();
    let feedback_tx = feedback_tx.as_ref();

    loop {
        // Drain feedback so the publisher doesn't block on a full channel.
        loop {
            let now = tokio::time::Instant::now();
            if now >= absolute_deadline {
                return Err("Timeout waiting for node_start result".to_string());
            }
            if now.duration_since(last_activity) >= timeouts.result {
                return Err("Timeout waiting for node_start result (idle)".to_string());
            }
            let drain_timeout = Duration::from_millis(50);
            match tokio::time::timeout(drain_timeout, action_handle.on_next_feedback()).await {
                Ok(Ok(msg)) => {
                    last_activity = tokio::time::Instant::now();
                    let payload = msg.payload();
                    if let Ok(feedback) = NodeStartFeedback::decode(payload.as_ref())
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
            return Err("Timeout waiting for node_start result".to_string());
        }
        if now.duration_since(last_activity) >= timeouts.result {
            return Err("Timeout waiting for node_start result (idle)".to_string());
        }
        let poll_timeout = Duration::from_millis(200);

        match ActionMessenger::request_result(messenger, &action_handle, poll_timeout).await {
            Ok(msg) => {
                let payload = msg.payload();
                match NodeStartResult::decode(&payload) {
                    Ok(result) => {
                        // Drain any remaining feedback that may have arrived while polling for the
                        // result so callers can reliably assert on stdout/stderr markers.
                        loop {
                            let Ok(Some(msg)) = action_handle.try_next_feedback() else {
                                break;
                            };
                            let payload = msg.payload();
                            if let Ok(feedback) = NodeStartFeedback::decode(payload.as_ref())
                                && let Some(tx) = feedback_tx
                            {
                                let _ = tx.send(feedback);
                            }
                        }
                        return Ok(NodeStartTestResponse {
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
    variant: Option<NodeSource>,
    goal_timeout: Duration,
    result_timeout: Duration,
    feedback_tx: Option<UnboundedSender<NodeAddFeedback>>,
    env_vars: Vec<(String, String)>,
    force: bool,
) -> Result<NodeAddTestResult, String> {
    let source = source.into();
    let build_env_vars = env_vars.clone();
    let feedback_tx = feedback_tx;

    let mut goal = match &source {
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

                // git.hash verification always targets the root source path
                // (alongside the peppy.json5 with the manifest), so no
                // provisioning is needed in variant directories.
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
    }
    .with_env_vars(env_vars)
    .with_force(force);

    if let Some(v) = variant {
        goal = goal.with_variant_source(v);
    }

    let (caller_core_node, caller_instance_id) = if feedback_tx.is_some() {
        ("*", "*")
    } else {
        (core_node_name, CALLER_INSTANCE_ID)
    };
    let goal_payload = goal
        .encode()
        .map_err(|e| format!("Failed to encode goal: {}", e))?;

    let mut action_handle = ActionMessenger::send_goal(
        messenger,
        caller_core_node,
        caller_instance_id,
        core_node_name,
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
        return Ok(NodeAddTestResult::from_add_failure(NodeAddResult::failure(
            PathBuf::new(),
            goal_response
                .rejection_reason
                .unwrap_or_else(|| "Goal rejected without reason".to_string()),
        )));
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
                        if !result.success {
                            return Ok(NodeAddTestResult::from_add_failure(result));
                        }
                        // Add succeeded — chain into a build call so the
                        // helper's return value matches the pre-split
                        // semantics (artifact ready in storage).
                        let node_name = result.node_name.clone().ok_or_else(|| {
                            "node_name missing from successful add result".to_string()
                        })?;
                        let node_tag = result.node_tag.clone().ok_or_else(|| {
                            "node_tag missing from successful add result".to_string()
                        })?;
                        return send_node_build_and_wait_internal(
                            messenger,
                            core_node_name,
                            &node_name,
                            &node_tag,
                            goal_timeout,
                            result_timeout,
                            build_env_vars,
                            feedback_tx,
                            result.log_path.clone(),
                        )
                        .await;
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
async fn send_node_build_and_wait_internal(
    messenger: &MessengerHandle,
    core_node_name: &str,
    node_name: &str,
    node_tag: &str,
    goal_timeout: Duration,
    result_timeout: Duration,
    env_vars: Vec<(String, String)>,
    feedback_tx: Option<&UnboundedSender<NodeAddFeedback>>,
    add_log_path: PathBuf,
) -> Result<NodeAddTestResult, String> {
    let goal =
        NodeBuildGoal::new(node_name, node_tag, result_timeout.as_secs()).with_env_vars(env_vars);
    let goal_payload = goal
        .encode()
        .map_err(|e| format!("Failed to encode build goal: {}", e))?;

    let (caller_core_node, caller_instance_id) = if feedback_tx.is_some() {
        ("*", "*")
    } else {
        (core_node_name, CALLER_INSTANCE_ID)
    };
    let mut action_handle = ActionMessenger::send_goal(
        messenger,
        caller_core_node,
        caller_instance_id,
        core_node_name,
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
        return Ok(NodeAddTestResult::from_build(NodeBuildResult::failure(
            PathBuf::new(),
            goal_response
                .rejection_reason
                .unwrap_or_else(|| "Build goal rejected without reason".to_string()),
        )));
    }

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
                        && let Ok(feedback) =
                            NodeBuildResultFeedbackBridge::decode(msg.payload().as_ref())
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
                        let mut test_result = NodeAddTestResult::from_build(result);
                        // Tests assert the log_path lives in `logs_dir_add()`,
                        // so preserve the *add* log even when build succeeded.
                        test_result.log_path = add_log_path;
                        return Ok(test_result);
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
///
/// When `feedback_tx` is provided, wildcard caller IDs are used so mock pub/sub
/// can match feedback topics with "*" segments.
pub async fn send_node_add_and_wait<'a>(
    messenger: &MessengerHandle,
    core_node_name: &str,
    source: impl Into<NodeAddSource<'a>>,
    goal_timeout: Duration,
    result_timeout: Duration,
    feedback_tx: Option<UnboundedSender<NodeAddFeedback>>,
) -> Result<NodeAddTestResult, String> {
    send_node_add_and_wait_internal(
        messenger,
        core_node_name,
        source,
        None,
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
) -> Result<NodeAddTestResult, String> {
    send_node_add_and_wait_internal(
        messenger,
        core_node_name,
        source,
        None,
        goal_timeout,
        result_timeout,
        feedback_tx,
        env_vars,
        false,
    )
    .await
}

pub async fn send_node_add_and_wait_with_variant<'a>(
    messenger: &MessengerHandle,
    core_node_name: &str,
    source: impl Into<NodeAddSource<'a>>,
    variant: &str,
    goal_timeout: Duration,
    result_timeout: Duration,
    feedback_tx: Option<UnboundedSender<NodeAddFeedback>>,
) -> Result<NodeAddTestResult, String> {
    send_node_add_and_wait_internal(
        messenger,
        core_node_name,
        source,
        Some(NodeSource::Fs(PathBuf::from(variant))),
        goal_timeout,
        result_timeout,
        feedback_tx,
        Vec::new(),
        false,
    )
    .await
}

pub async fn send_node_add_and_wait_with_force<'a>(
    messenger: &MessengerHandle,
    core_node_name: &str,
    source: impl Into<NodeAddSource<'a>>,
    goal_timeout: Duration,
    result_timeout: Duration,
    feedback_tx: Option<UnboundedSender<NodeAddFeedback>>,
) -> Result<NodeAddTestResult, String> {
    send_node_add_and_wait_internal(
        messenger,
        core_node_name,
        source,
        None,
        goal_timeout,
        result_timeout,
        feedback_tx,
        Vec::new(),
        true,
    )
    .await
}

/// Helper function to send a node_start goal and wait for the result.
/// This wraps the action pattern for simpler test usage.
///
/// When `feedback_tx` is provided, wildcard caller IDs are used so mock pub/sub
/// can match feedback topics with "*" segments.
pub async fn send_node_start_and_wait(
    messenger: &MessengerHandle,
    core_node_name: &str,
    runtime_config_json5: &str,
    node_name: &str,
    tag: &str,
    timeouts: &NodeStartTestTimeouts,
    feedback_tx: Option<UnboundedSender<NodeStartFeedback>>,
) -> Result<NodeStartTestResponse, String> {
    send_node_start_and_wait_internal(
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
pub async fn send_node_start_and_wait_with_env(
    messenger: &MessengerHandle,
    core_node_name: &str,
    runtime_config_json5: &str,
    node_name: &str,
    tag: &str,
    timeouts: &NodeStartTestTimeouts,
    feedback_tx: Option<UnboundedSender<NodeStartFeedback>>,
    env_vars: Vec<(String, String)>,
) -> Result<NodeStartTestResponse, String> {
    send_node_start_and_wait_internal(
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
    init_test_node_project("example_node", "0.1.0", true)
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

    // Use the pre-built binary path in start_cmd instead of "cargo run".
    // This avoids recompilation after the folder is copied to storage,
    // since cargo's fingerprinting invalidates the cache when absolute paths change.
    let binary_path = node_dir.join("target/debug").join(crate_name);
    std::fs::write(
        node_dir.join(NODE_CONFIG_FILE),
        r#"{
  schema_version: 1,
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
    start_cmd: [
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

pub async fn start_core_node_with_mock_messenger() -> StartedCoreNode {
    let (data_dir, peppy_dirs) = init_test_data_dir();
    let shared_messenger = create_mock_messenger().await;
    let node_startup_timeout = Duration::from_secs(10);
    let node_start_health_timeout = Duration::from_secs(30);
    start_core_node_with_messenger(
        shared_messenger,
        node_startup_timeout,
        node_start_health_timeout,
        data_dir,
        peppy_dirs,
    )
    .await
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
    start_core_node_with_messenger(
        shared_messenger,
        node_startup_timeout,
        node_start_health_timeout,
        data_dir,
        peppy_dirs,
    )
    .await
}

pub async fn start_core_node_with_health_timeout(
    node_start_health_timeout: Duration,
) -> StartedCoreNode {
    let (data_dir, peppy_dirs) = init_test_data_dir();
    let shared_messenger = create_mock_messenger().await;
    let node_startup_timeout = Duration::from_secs(10);
    start_core_node_with_messenger(
        shared_messenger,
        node_startup_timeout,
        node_start_health_timeout,
        data_dir,
        peppy_dirs,
    )
    .await
}

async fn start_core_node_with_messenger(
    shared_messenger: Arc<Mutex<Messenger>>,
    node_startup_timeout: Duration,
    node_start_health_timeout: Duration,
    data_dir: TempDir,
    peppy_dirs: PeppyDirs,
) -> StartedCoreNode {
    let caller_handle = MessengerHandle::from_shared(Arc::clone(&shared_messenger));
    let node_arguments = CoreNodeArguments {
        node_startup_timeout,
        node_start_health_timeout,
    };
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
    handle: std::sync::Arc<parking_lot::RwLock<node_stack::NodeEntity>>,
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

    let log_dir = peppy_dirs.logs_dir_start();
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
/// the entity's existing `start_cmd` — callers are responsible for ensuring
/// the node config's start_cmd is spawnable in the test environment (the
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
            name,
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
