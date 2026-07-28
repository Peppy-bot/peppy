#![allow(dead_code)] // Each test binary uses only a subset of these shared helpers.

use super::daemon::StartedCoreNode;
use super::fixtures::write_peppy_json5;
use super::poll::AbortOnDrop;
use super::{CALLER_INSTANCE_ID, TEST_GIT_HASH, core_node_target, test_node_target};
use config::node::QoSProfile;
use core_node_api::ActionId;
use core_node_api::encoding::{
    NodeAddFeedback, NodeAddGoal, NodeAddGoalResponse, NodeAddResult, NodeBuildFeedback,
    NodeBuildGoal, NodeBuildGoalResponse, NodeBuildResult, NodeRunFeedback, NodeRunGoal,
    NodeRunGoalResponse, NodeRunResult, NodeSource,
};
use daemon_config::consts::PEPPY_OUTPUT_DIR;
use gix_url::Url as GitUrl;
use peppylib::ActionMessenger;
use peppylib::messaging::{ActionGoalHandle, MessengerHandle, ResultStatus};
use peppylib::services::health::listen_for_node_health;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tempfile::TempDir;
use tokio::sync::mpsc::UnboundedSender;

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
    /// Add a node by `(name, tag)` against the repo cache; the daemon
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

/// Builds the instance plan a `node_run` goal carries.
///
/// Note what a test can no longer choose: the messaging endpoint, the bound
/// core node, and the node identity. Those belong to the daemon that spawns the
/// node, so the goal has nowhere to put them and a test cannot accidentally
/// pin a node to the wrong daemon.
pub fn instance_plan(
    instance_id: &str,
    arguments: std::collections::BTreeMap<String, config::AnyType>,
) -> config::runtime::NodeInstancePlan {
    config::runtime::NodeInstancePlan {
        arguments,
        ..config::runtime::NodeInstancePlan::new(
            config::runtime::Name::new(instance_id).expect("valid instance id"),
        )
    }
}

/// [`instance_plan`] with no node arguments: the shape most tests use.
pub fn default_instance_plan(instance_id: &str) -> config::runtime::NodeInstancePlan {
    instance_plan(instance_id, Default::default())
}

/// Why [`drain_node_run_feedback`] returned.
enum FeedbackDrainOutcome {
    /// `stop_when` became true after a feedback line was collected.
    Predicate,
    /// The server closed the feedback stream, i.e. the action completed.
    Closed,
    /// The absolute or idle deadline elapsed before either of the above.
    TimedOut,
}

/// Sends a `node_run` goal and returns the live action handle plus its decoded
/// goal response. Split out so tests that interleave work between the goal and
/// the result (e.g. bringing up a delayed health responder once startup output
/// has streamed) share one goal-send implementation with the plain wait helper.
#[allow(clippy::too_many_arguments)]
async fn send_node_run_goal(
    messenger: &MessengerHandle,
    core_node_name: &str,
    instance_plan: config::runtime::NodeInstancePlan,
    node_name: &str,
    tag: &str,
    goal_timeout: Duration,
    result_secs: u64,
    env_vars: Vec<(String, String)>,
) -> Result<(ActionGoalHandle, NodeRunGoalResponse), String> {
    let goal = NodeRunGoal::new(instance_plan, node_name, tag, result_secs).with_env_vars(env_vars);
    let goal_payload = goal
        .encode()
        .map_err(|e| format!("Failed to encode goal: {}", e))?;

    let action_handle = ActionMessenger::send_goal(
        messenger,
        core_node_name,
        CALLER_INSTANCE_ID,
        core_node_target(core_node_name),
        ActionId::NodeRun.name(),
        None,
        goal_payload,
        QoSProfile::default(),
        goal_timeout,
    )
    .await
    .map_err(|e| format!("Failed to send goal: {}", e))?;

    let goal_response = NodeRunGoalResponse::decode(&action_handle.goal_reply().body)
        .map_err(|e| format!("Failed to decode goal response: {}", e))?;

    // A rejected goal never streams feedback or produces a result, so callers
    // must not proceed to drain; they would just burn the full result budget
    // and surface a generic timeout instead of the actual rejection reason.
    if !goal_response.accepted {
        return Err(format!(
            "node_run goal rejected: {}",
            goal_response
                .rejection_reason
                .as_deref()
                .unwrap_or("rejected without reason")
        ));
    }

    Ok((action_handle, goal_response))
}

/// Drains feedback from a live `node_run` action handle, appending each decoded
/// line to `collected` and forwarding it to `feedback_tx` when present. Returns
/// as soon as `stop_when(&collected)` holds, the server closes the stream, or a
/// deadline elapses. The plain wait helper passes a never-true predicate to
/// drain to close; gated tests stop once the output they expect has streamed,
/// while the start is still blocked waiting on a not-yet-answered health check.
async fn drain_node_run_feedback(
    action_handle: &mut ActionGoalHandle,
    feedback_tx: Option<&UnboundedSender<NodeRunFeedback>>,
    collected: &mut Vec<NodeRunFeedback>,
    absolute_deadline: tokio::time::Instant,
    idle_timeout: Duration,
    stop_when: impl Fn(&[NodeRunFeedback]) -> bool,
) -> FeedbackDrainOutcome {
    let mut last_activity = tokio::time::Instant::now();
    loop {
        let now = tokio::time::Instant::now();
        if now >= absolute_deadline || now.duration_since(last_activity) >= idle_timeout {
            return FeedbackDrainOutcome::TimedOut;
        }
        let drain_timeout = Duration::from_millis(50);
        match tokio::time::timeout(drain_timeout, action_handle.on_next_feedback()).await {
            Ok(Ok(msg)) => {
                last_activity = tokio::time::Instant::now();
                let feedback = NodeRunFeedback::decode(msg.payload().as_ref())
                    .expect("failed to decode NodeRunFeedback");
                if let Some(tx) = feedback_tx {
                    let _ = tx.send(feedback.clone());
                }
                collected.push(feedback);
                if stop_when(collected) {
                    return FeedbackDrainOutcome::Predicate;
                }
            }
            Ok(Err(_)) => return FeedbackDrainOutcome::Closed,
            Err(_) => {}
        }
    }
}

/// Fetches the buffered result of a completed `node_run` action and decodes it.
async fn fetch_node_run_result(
    messenger: &MessengerHandle,
    action_handle: &ActionGoalHandle,
    fetch_timeout: Duration,
) -> Result<NodeRunResult, String> {
    match ActionMessenger::request_result(messenger, action_handle, fetch_timeout).await {
        Ok(reply) => match reply.status {
            ResultStatus::Completed | ResultStatus::Cancelled => {
                NodeRunResult::decode(reply.body.as_ref())
                    .map_err(|err| format!("Failed to decode result: {}", err))
            }
            other => Err(format!("action did not complete with a result: {other:?}")),
        },
        Err(err) => Err(format!("Failed to get result: {}", err)),
    }
}

#[allow(clippy::too_many_arguments)]
async fn send_node_run_and_wait_internal(
    messenger: &MessengerHandle,
    core_node_name: &str,
    instance_plan: config::runtime::NodeInstancePlan,
    node_name: &str,
    tag: &str,
    timeouts: &NodeRunTestTimeouts,
    feedback_tx: Option<UnboundedSender<NodeRunFeedback>>,
    env_vars: Vec<(String, String)>,
) -> Result<NodeRunTestResponse, String> {
    let (mut action_handle, goal_response) = send_node_run_goal(
        messenger,
        core_node_name,
        instance_plan,
        node_name,
        tag,
        timeouts.goal,
        timeouts.result.as_secs(),
        env_vars,
    )
    .await?;

    let absolute_deadline = tokio::time::Instant::now() + timeouts.result;
    let mut collected = Vec::new();
    // Drain feedback until the server closes the stream on completion, honoring
    // the idle / max-timeout budgets, then fetch the buffered result once.
    if let FeedbackDrainOutcome::TimedOut = drain_node_run_feedback(
        &mut action_handle,
        feedback_tx.as_ref(),
        &mut collected,
        absolute_deadline,
        timeouts.result,
        |_| false,
    )
    .await
    {
        return Err("Timeout waiting for node_run result".to_string());
    }

    let fetch_timeout = absolute_deadline.saturating_duration_since(tokio::time::Instant::now());
    let result = fetch_node_run_result(messenger, &action_handle, fetch_timeout).await?;
    Ok(NodeRunTestResponse {
        goal_response,
        result,
    })
}

/// Drives a `node_run` goal with a deliberately delayed health responder so a
/// feedback-streaming assertion is deterministic instead of racing the daemon's
/// start-success stream close.
///
/// The node's ready responder must already be live so the start advances past
/// the ready wait into the health wait. This helper sends the goal, then drains
/// feedback while the start blocks on the not-yet-answered health check (output
/// streams live throughout). Once `expected_output(&collected)` holds, it brings
/// up the health responder, which lets the health check pass and the start
/// complete; it then drains the remaining feedback, fetches the result, and
/// returns it alongside every feedback line observed. The health responder is
/// kept alive until the result is fetched so health stays answered through
/// commit.
#[allow(clippy::too_many_arguments)]
pub async fn send_node_run_with_delayed_health(
    caller_messenger: &MessengerHandle,
    node_messenger: &MessengerHandle,
    core_node_name: &str,
    instance_plan: config::runtime::NodeInstancePlan,
    node_name: &str,
    tag: &str,
    instance_id: &str,
    timeouts: &NodeRunTestTimeouts,
    expected_output: impl Fn(&[NodeRunFeedback]) -> bool,
) -> Result<(NodeRunTestResponse, Vec<NodeRunFeedback>), String> {
    let (mut action_handle, goal_response) = send_node_run_goal(
        caller_messenger,
        core_node_name,
        instance_plan,
        node_name,
        tag,
        timeouts.goal,
        timeouts.result.as_secs(),
        Vec::new(),
    )
    .await?;

    let absolute_deadline = tokio::time::Instant::now() + timeouts.result;
    let mut feedback = Vec::new();
    match drain_node_run_feedback(
        &mut action_handle,
        None,
        &mut feedback,
        absolute_deadline,
        timeouts.result,
        &expected_output,
    )
    .await
    {
        FeedbackDrainOutcome::Predicate => {}
        FeedbackDrainOutcome::Closed => {
            return Err("feedback stream closed before the expected output streamed".to_string());
        }
        FeedbackDrainOutcome::TimedOut => {
            return Err("timed out waiting for the expected output to stream".to_string());
        }
    }

    // Release the start: the health check now succeeds, so commit + drain run and
    // the action completes. The expected output was already published (we waited
    // for it on the stream), so the daemon's own drain cannot drop it.
    let _health = AbortOnDrop(
        listen_for_node_health(
            node_messenger,
            core_node_name,
            instance_id,
            test_node_target(node_name),
        )
        .await
        .map_err(|e| format!("failed to start node health service: {e}"))?,
    );

    drain_node_run_feedback(
        &mut action_handle,
        None,
        &mut feedback,
        absolute_deadline,
        timeouts.result,
        |_| false,
    )
    .await;

    let fetch_timeout = absolute_deadline.saturating_duration_since(tokio::time::Instant::now());
    let result = fetch_node_run_result(caller_messenger, &action_handle, fetch_timeout).await?;
    Ok((
        NodeRunTestResponse {
            goal_response,
            result,
        },
        feedback,
    ))
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
        ActionId::NodeAdd.name(),
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
    let goal_response_payload = action_handle.goal_reply().body.clone();
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

    // Drain feedback until the server closes the stream on completion, then
    // fetch the buffered result once.
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
                let feedback = NodeAddFeedback::decode(payload.as_ref())
                    .expect("failed to decode NodeAddFeedback");
                if let Some(tx) = feedback_tx {
                    let _ = tx.send(feedback);
                }
            }
            Ok(Err(_)) => break,
            Err(_) => {}
        }
    }

    let fetch_timeout = absolute_deadline.saturating_duration_since(tokio::time::Instant::now());
    match ActionMessenger::request_result(messenger, &action_handle, fetch_timeout).await {
        Ok(reply) => match reply.status {
            ResultStatus::Completed | ResultStatus::Cancelled => {
                NodeAddResult::decode(reply.body.as_ref())
                    .map_err(|err| format!("Failed to decode result: {}", err))
            }
            other => Err(format!("action did not complete with a result: {other:?}")),
        },
        Err(err) => Err(format!("Failed to get result: {}", err)),
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
    send_node_build_and_wait_internal(
        messenger,
        core_node_name,
        node_name,
        node_tag,
        goal_timeout,
        result_timeout,
        env_vars,
        feedback_tx,
        false,
    )
    .await
}

/// Like [`send_node_build_and_wait`] but sets the `--force` flag, which cancels
/// any in-flight build for the node and supersedes it.
#[allow(clippy::too_many_arguments)]
pub async fn send_node_build_and_wait_forced(
    messenger: &MessengerHandle,
    core_node_name: &str,
    node_name: &str,
    node_tag: &str,
    goal_timeout: Duration,
    result_timeout: Duration,
    env_vars: Vec<(String, String)>,
    feedback_tx: Option<UnboundedSender<NodeBuildFeedback>>,
) -> Result<NodeBuildResult, String> {
    send_node_build_and_wait_internal(
        messenger,
        core_node_name,
        node_name,
        node_tag,
        goal_timeout,
        result_timeout,
        env_vars,
        feedback_tx,
        true,
    )
    .await
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
    feedback_tx: Option<UnboundedSender<NodeBuildFeedback>>,
    force: bool,
) -> Result<NodeBuildResult, String> {
    let goal = NodeBuildGoal::new(node_name, node_tag, result_timeout.as_secs())
        .with_env_vars(env_vars)
        .with_force(force);
    let goal_payload = goal
        .encode()
        .map_err(|e| format!("Failed to encode build goal: {}", e))?;

    let mut action_handle = ActionMessenger::send_goal(
        messenger,
        core_node_name,
        CALLER_INSTANCE_ID,
        core_node_target(core_node_name),
        ActionId::NodeBuild.name(),
        None,
        goal_payload,
        QoSProfile::default(),
        goal_timeout,
    )
    .await
    .map_err(|e| format!("Failed to send build goal: {}", e))?;

    let goal_response_payload = action_handle.goal_reply().body.clone();
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

    // Drain feedback until the server closes the stream on completion, then
    // fetch the buffered result once.
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
                let feedback = NodeBuildFeedback::decode(msg.payload().as_ref())
                    .expect("failed to decode NodeBuildFeedback");
                if let Some(tx) = feedback_tx {
                    let _ = tx.send(feedback);
                }
            }
            Ok(Err(_)) => break,
            Err(_) => {}
        }
    }

    let fetch_timeout = absolute_deadline.saturating_duration_since(tokio::time::Instant::now());
    match ActionMessenger::request_result(messenger, &action_handle, fetch_timeout).await {
        Ok(reply) => match reply.status {
            ResultStatus::Completed | ResultStatus::Cancelled => {
                NodeBuildResult::decode(reply.body.as_ref())
                    .map_err(|err| format!("Failed to decode build result: {}", err))
            }
            other => Err(format!("action did not complete with a result: {other:?}")),
        },
        Err(err) => Err(format!("Failed to get build result: {}", err)),
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

/// Adds and builds a node whose `run_cmd` forks two grandchild `sleep`s and
/// waits; all three processes share the node's process group (nodes are
/// spawned as group leaders). Used by the force-kill tests to prove a group
/// kill reaps descendants, not just the leader. Returns the source dir guard.
pub async fn add_and_build_forking_node(
    started: &StartedCoreNode,
    node_name: &str,
    node_tag: &str,
) -> TempDir {
    let source_dir = tempfile::tempdir().expect("temp source dir");
    let peppy_json5 = r#"{
            peppy_schema: "node/v1",
            manifest: { name: "{NAME}", tag: "{TAG}" },
            execution: {
                language: "rust",
                run_cmd: ["sh", "-c", "sleep 1000 & sleep 1000 & wait"]
            }
        }"#
    .replace("{NAME}", node_name)
    .replace("{TAG}", node_tag);
    write_peppy_json5(source_dir.path(), &peppy_json5);

    let add_response = send_node_add_and_wait(
        &started.caller_handle,
        &started.core_node_name,
        source_dir.path(),
        Duration::from_secs(5),
        Duration::from_secs(5),
        None,
    )
    .await
    .expect("node_add should complete");
    assert!(add_response.success, "node_add failed: {add_response:?}");
    build_staged_node(started, node_name, node_tag).await;
    source_dir
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
    instance_plan: config::runtime::NodeInstancePlan,
    node_name: &str,
    tag: &str,
    timeouts: &NodeRunTestTimeouts,
    feedback_tx: Option<UnboundedSender<NodeRunFeedback>>,
) -> Result<NodeRunTestResponse, String> {
    send_node_run_and_wait_internal(
        messenger,
        core_node_name,
        instance_plan,
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
    instance_plan: config::runtime::NodeInstancePlan,
    node_name: &str,
    tag: &str,
    timeouts: &NodeRunTestTimeouts,
    feedback_tx: Option<UnboundedSender<NodeRunFeedback>>,
    env_vars: Vec<(String, String)>,
) -> Result<NodeRunTestResponse, String> {
    send_node_run_and_wait_internal(
        messenger,
        core_node_name,
        instance_plan,
        node_name,
        tag,
        timeouts,
        feedback_tx,
        env_vars,
    )
    .await
}
