#![allow(dead_code)]

use config::consts::{
    DEFAULT_MESSAGING_HOST, NODE_CONFIG_FILE, PEPPY_OUTPUT_DIR, PEPPYGEN_OUTPUT_PATH,
};
use config::node::{PeppygenLanguage, QoSProfile};
use gix_url::Url as GitUrl;
use master_node::encoding::{
    NodeAddFeedback, NodeAddGoal, NodeAddGoalResponse, NodeAddResult, NodeStartFeedback,
    NodeStartGoal, NodeStartGoalResponse, NodeStartResult,
};
use master_node::names;
use master_node::{MasterNode, MasterNodeArguments};
use node_stack::NodeStack;
use peppylib::messaging::MessengerHandle;
use peppylib::{ActionMessenger, PeppyError};
use pmi::{Messenger, MessengerAdapter, MessengerBackend, MockAdapter};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use tokio::sync::mpsc::UnboundedSender;
use tokio::task::JoinHandle;

/// A wrapper around `JoinHandle` that aborts the task when dropped.
pub struct AbortOnDrop<T>(pub JoinHandle<T>);

impl<T> Drop for AbortOnDrop<T> {
    fn drop(&mut self) {
        self.0.abort();
    }
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

#[allow(clippy::too_many_arguments)]
async fn send_node_start_and_wait_internal(
    messenger: &MessengerHandle,
    master_node_name: &str,
    runtime_config_json5: &str,
    node_name: &str,
    tag: &str,
    timeouts: &NodeStartTestTimeouts,
    feedback_tx: Option<UnboundedSender<NodeStartFeedback>>,
    env_vars: Vec<(String, String)>,
) -> Result<NodeStartTestResponse, String> {
    let goal = NodeStartGoal::new(runtime_config_json5, node_name, tag).with_env_vars(env_vars);
    let (caller_master_node, caller_instance_id) = if feedback_tx.is_some() {
        ("*", "*")
    } else {
        (master_node_name, CALLER_INSTANCE_ID)
    };
    let goal_payload = goal
        .encode()
        .map_err(|e| format!("Failed to encode goal: {}", e))?;

    let mut action_handle = ActionMessenger::send_goal(
        messenger,
        caller_master_node,
        caller_instance_id,
        master_node_name,
        names::NODE_START_ACTION,
        Some(master_node_name),
        None,
        goal_payload,
        QoSProfile::default(),
        timeouts.goal,
    )
    .await
    .map_err(|e| format!("Failed to send goal: {}", e))?;

    // Decode the goal response to get log_path
    let goal_response_payload = action_handle.goal_response().payload().to_bytes();
    let goal_response = NodeStartGoalResponse::decode(&goal_response_payload)
        .map_err(|e| format!("Failed to decode goal response: {}", e))?;

    let deadline = tokio::time::Instant::now() + timeouts.result;
    let feedback_tx = feedback_tx.as_ref();

    loop {
        // Drain feedback so the publisher doesn't block on a full channel.
        loop {
            let now = tokio::time::Instant::now();
            if now >= deadline {
                return Err("Timeout waiting for node_start result".to_string());
            }
            let remaining = deadline - now;
            let drain_timeout = Duration::from_millis(50).min(remaining);
            match tokio::time::timeout(drain_timeout, action_handle.on_next_feedback()).await {
                Ok(Ok(msg)) => {
                    let payload = msg.payload();
                    if let Ok(feedback) = NodeStartFeedback::decode(&payload.to_bytes())
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
        if now >= deadline {
            return Err("Timeout waiting for node_start result".to_string());
        }
        let remaining = deadline - now;
        let poll_timeout = Duration::from_millis(200).min(remaining);

        match ActionMessenger::request_result(messenger, &action_handle, poll_timeout).await {
            Ok(msg) => {
                let payload = msg.payload().to_bytes();
                match NodeStartResult::decode(&payload) {
                    Ok(result) => {
                        // Drain any remaining feedback that may have arrived while polling for the
                        // result so callers can reliably assert on stdout/stderr markers.
                        loop {
                            let Ok(Some(msg)) = action_handle.try_next_feedback() else {
                                break;
                            };
                            let payload = msg.payload();
                            if let Ok(feedback) = NodeStartFeedback::decode(&payload.to_bytes())
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

async fn send_node_add_and_wait_internal<'a>(
    messenger: &MessengerHandle,
    master_node_name: &str,
    source: impl Into<NodeAddSource<'a>>,
    goal_timeout: Duration,
    result_timeout: Duration,
    feedback_tx: Option<UnboundedSender<NodeAddFeedback>>,
    env_vars: Vec<(String, String)>,
) -> Result<NodeAddResult, String> {
    let source = source.into();

    let goal = match &source {
        NodeAddSource::Path(path) => {
            // For filesystem sources, ensure the git hash file exists
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
            NodeAddGoal::new_http(url.clone(), TEST_GIT_HASH, result_timeout.as_secs())
        }
    }
    .with_env_vars(env_vars);

    let (caller_master_node, caller_instance_id) = if feedback_tx.is_some() {
        ("*", "*")
    } else {
        (master_node_name, CALLER_INSTANCE_ID)
    };
    let goal_payload = goal
        .encode()
        .map_err(|e| format!("Failed to encode goal: {}", e))?;

    let mut action_handle = ActionMessenger::send_goal(
        messenger,
        caller_master_node,
        caller_instance_id,
        master_node_name,
        names::NODE_ADD_ACTION,
        Some(master_node_name),
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
    let goal_response_payload = action_handle.goal_response().payload().to_bytes();
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

    let deadline = tokio::time::Instant::now() + result_timeout;
    let feedback_tx = feedback_tx.as_ref();

    loop {
        // Drain feedback so the publisher doesn't block on a full channel.
        loop {
            let now = tokio::time::Instant::now();
            if now >= deadline {
                return Err("Timeout waiting for node_add result".to_string());
            }
            let remaining = deadline - now;
            let drain_timeout = Duration::from_millis(50).min(remaining);
            match tokio::time::timeout(drain_timeout, action_handle.on_next_feedback()).await {
                Ok(Ok(msg)) => {
                    let payload = msg.payload();
                    if let Ok(feedback) = NodeAddFeedback::decode(&payload.to_bytes())
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
        if now >= deadline {
            return Err("Timeout waiting for node_add result".to_string());
        }
        let remaining = deadline - now;
        let poll_timeout = Duration::from_millis(200).min(remaining);

        match ActionMessenger::request_result(messenger, &action_handle, poll_timeout).await {
            Ok(msg) => {
                let payload = msg.payload().to_bytes();
                match NodeAddResult::decode(&payload) {
                    Ok(result) => {
                        // Drain any remaining feedback that may have arrived while polling for the
                        // result so callers can reliably assert on stdout/stderr markers.
                        loop {
                            let Ok(Some(msg)) = action_handle.try_next_feedback() else {
                                break;
                            };
                            let payload = msg.payload();
                            if let Ok(feedback) = NodeAddFeedback::decode(&payload.to_bytes())
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
    master_node_name: &str,
    source: impl Into<NodeAddSource<'a>>,
    goal_timeout: Duration,
    result_timeout: Duration,
    feedback_tx: Option<UnboundedSender<NodeAddFeedback>>,
) -> Result<NodeAddResult, String> {
    send_node_add_and_wait_internal(
        messenger,
        master_node_name,
        source,
        goal_timeout,
        result_timeout,
        feedback_tx,
        Vec::new(),
    )
    .await
}

pub async fn send_node_add_and_wait_with_env<'a>(
    messenger: &MessengerHandle,
    master_node_name: &str,
    source: impl Into<NodeAddSource<'a>>,
    goal_timeout: Duration,
    result_timeout: Duration,
    feedback_tx: Option<UnboundedSender<NodeAddFeedback>>,
    env_vars: Vec<(String, String)>,
) -> Result<NodeAddResult, String> {
    send_node_add_and_wait_internal(
        messenger,
        master_node_name,
        source,
        goal_timeout,
        result_timeout,
        feedback_tx,
        env_vars,
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
    master_node_name: &str,
    runtime_config_json5: &str,
    node_name: &str,
    tag: &str,
    timeouts: &NodeStartTestTimeouts,
    feedback_tx: Option<UnboundedSender<NodeStartFeedback>>,
) -> Result<NodeStartTestResponse, String> {
    send_node_start_and_wait_internal(
        messenger,
        master_node_name,
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
    master_node_name: &str,
    runtime_config_json5: &str,
    node_name: &str,
    tag: &str,
    timeouts: &NodeStartTestTimeouts,
    feedback_tx: Option<UnboundedSender<NodeStartFeedback>>,
    env_vars: Vec<(String, String)>,
) -> Result<NodeStartTestResponse, String> {
    send_node_start_and_wait_internal(
        messenger,
        master_node_name,
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

    generator::generate_peppygen_lib(PeppygenLanguage::Rust, &node_dir, Vec::new(), "test-hash")
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
        format!(
            r#"{{
  schema_version: 1,
  manifest: {{
    name: "{crate_name}",
    tag: "{node_tag}",
    language: "rust",
    // Avoid `add_cmd` build step here to make the `add` tests faster
    add_cmd: [
        "true"
    ],
    start_cmd: [
      "{}"
    ]
  }},
  interfaces: {{
    exposes: {{
      topics: [
        {{
          name: "hello_world",
          qos_profile: "sensor_data",
          message_format: {{
            timestamp: "time",
            message: "string"
          }}
        }}
      ],
    }}
  }}
}}"#,
            binary_path.display()
        ),
    )
    .expect("failed to write test node peppy.json5");
}

fn build_cargo_project(dir: &Path) {
    let output = Command::new("cargo")
        .arg("build")
        .env("CARGO_NET_OFFLINE", "true")
        .current_dir(dir)
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
pub struct StartedMasterNode {
    pub shared_messenger: Arc<Mutex<Messenger>>,
    pub caller_handle: MessengerHandle,
    pub master_node_name: String,
    pub node_stack: NodeStack,
    pub task: AbortOnDrop<master_node::Result<()>>,
}

pub async fn start_master_node_with_mock_messenger() -> StartedMasterNode {
    let shared_messenger = create_mock_messenger().await;
    let node_startup_timeout = Duration::from_secs(10);
    let node_start_health_timeout = Duration::from_secs(30);
    start_master_node_with_messenger(
        shared_messenger,
        node_startup_timeout,
        node_start_health_timeout,
    )
    .await
}

pub async fn start_master_node_with_real_messenger() -> StartedMasterNode {
    let mut instance = pmi::ZenohAdapter::start_router_ephemeral(DEFAULT_MESSAGING_HOST, None)
        .await
        .expect("failed to start zenoh router for test");
    instance
        .messenger()
        .start_session()
        .await
        .expect("failed to start zenoh session");
    let shared_messenger = Arc::new(Mutex::new(instance.take_messenger()));
    let node_startup_timeout = Duration::from_secs(10);
    let node_start_health_timeout = Duration::from_secs(30);
    start_master_node_with_messenger(
        shared_messenger,
        node_startup_timeout,
        node_start_health_timeout,
    )
    .await
}

pub async fn start_master_node_with_health_timeout(
    node_start_health_timeout: Duration,
) -> StartedMasterNode {
    let shared_messenger = create_mock_messenger().await;
    let node_startup_timeout = Duration::from_secs(10);
    start_master_node_with_messenger(
        shared_messenger,
        node_startup_timeout,
        node_start_health_timeout,
    )
    .await
}

async fn start_master_node_with_messenger(
    shared_messenger: Arc<Mutex<Messenger>>,
    node_startup_timeout: Duration,
    node_start_health_timeout: Duration,
) -> StartedMasterNode {
    let caller_handle = MessengerHandle::from_shared(Arc::clone(&shared_messenger));
    let node_arguments = MasterNodeArguments {
        node_startup_timeout,
        node_start_health_timeout,
    };
    let root_dir = std::env::current_dir().expect("failed to get current directory");
    let master_node = MasterNode::new(
        Arc::clone(&shared_messenger),
        Some("test_master_node"),
        node_arguments,
        root_dir,
    );
    let master_node_name = master_node.node_name().to_string();
    let node_stack = master_node.node_stack().clone();

    // Use start_with_ready to properly synchronize instead of a time-based sleep
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(async move { master_node.start_with_ready(Some(ready_tx)).await });

    // Wait for all services to be fully registered before returning
    ready_rx.await.expect("master node ready signal failed");

    StartedMasterNode {
        shared_messenger,
        caller_handle,
        master_node_name,
        node_stack,
        task: AbortOnDrop(task),
    }
}
