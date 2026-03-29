#![allow(dead_code)]

use config::consts::{
    DEFAULT_MESSAGING_HOST, NODE_CONFIG_FILE, PEPPY_OUTPUT_DIR, PEPPYGEN_OUTPUT_PATH, PeppyDirs,
};
use config::node::{PeppygenLanguage, QoSProfile};
use core_node::encoding::{
    NodeAddFeedback, NodeAddGoal, NodeAddGoalResponse, NodeAddResult, NodeSource,
    NodeStartFeedback, NodeStartGoal, NodeStartGoalResponse, NodeStartResult,
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
    Http(url::Url),
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
                        let pending = std::str::from_utf8(payload.as_ref())
                            .map(|text| text.starts_with("result pending"))
                            .unwrap_or(false);
                        if !pending {
                            return Err(format!("Failed to decode result: {}", err));
                        }
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
) -> Result<NodeAddResult, String> {
    let source = source.into();

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
        NodeAddSource::Http(url) => {
            NodeAddGoal::new_http(url.clone(), None, TEST_GIT_HASH, result_timeout.as_secs())
        }
    }
    .with_env_vars(env_vars);

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
                        let pending = std::str::from_utf8(payload.as_ref())
                            .map(|text| text.starts_with("result pending"))
                            .unwrap_or(false);
                        if !pending {
                            return Err(format!("Failed to decode result: {}", err));
                        }
                    }
                }
            }
            Err(PeppyError::ActionResultTimeout { .. }) => {}
            Err(err) => return Err(format!("Failed to get result: {}", err)),
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
) -> Result<NodeAddResult, String> {
    send_node_add_and_wait_internal(
        messenger,
        core_node_name,
        source,
        None,
        goal_timeout,
        result_timeout,
        feedback_tx,
        Vec::new(),
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
        None,
        goal_timeout,
        result_timeout,
        feedback_tx,
        env_vars,
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
) -> Result<NodeAddResult, String> {
    send_node_add_and_wait_internal(
        messenger,
        core_node_name,
        source,
        Some(NodeSource::Fs(PathBuf::from(variant))),
        goal_timeout,
        result_timeout,
        feedback_tx,
        Vec::new(),
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
  // Avoid `add_cmd` build step here to make the `add` tests faster
  execution: {
    language: "rust",
    add_cmd: [
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
        node_stack,
        peppy_dirs,
        task: AbortOnDrop(task),
        _data_dir: data_dir,
    }
}
