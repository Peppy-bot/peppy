#![allow(dead_code)] // Each test binary uses only a subset of these shared helpers.

use super::daemon::StartedCoreNode;
use super::fixtures::write_peppy_json5;
use super::poll::AbortOnDrop;
use super::{CALLER_INSTANCE_ID, TEST_GIT_HASH, test_node_target};
use config::node::{RestartDeclaration, RestartPolicy, RestartRequest};
use core_node_api::DaemonFailureKind;
use core_node_api::encoding::{
    NodeAddFeedback, NodeAddGoal, NodeAddGoalResponse, NodeAddResult, NodeBuildFeedback,
    NodeBuildGoal, NodeBuildGoalResponse, NodeBuildResult, NodeRunFeedback, NodeRunGoal,
    NodeRunGoalResponse, NodeRunResult, NodeSource,
};
use daemon_config::consts::PEPPY_OUTPUT_DIR;
use gix_url::Url as GitUrl;
use peppylib::core_node::transport::send_goal;
use peppylib::messaging::{ActionGoalHandle, Failure, GoalAnswer, GoalOutcome, MessengerHandle};
use peppylib::services::health::listen_for_node_health;
use peppylib::{ActionMessenger, PeppyError};
use std::fmt::{Debug, Display};
use std::ops::ControlFlow;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tempfile::TempDir;
use tokio::sync::mpsc::UnboundedSender;

/// How long a test waits for the daemon to answer a goal. No goal of these
/// tests comes near it on a host of any speed: a test runs into it only when
/// the daemon does not answer.
pub const GOAL_TIMEOUT: Duration = Duration::from_secs(30);

/// How long a test waits for a goal to end, on the terms of
/// [`GOAL_TIMEOUT`].
pub const RESULT_TIMEOUT: Duration = Duration::from_secs(120);

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
    ResolveRef { name: &'a str, tag: &'a str },
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

/// The goal and result bounds of a test's `node_run`: [`GOAL_TIMEOUT`] and
/// [`RESULT_TIMEOUT`].
pub const TIMEOUTS: NodeRunTestTimeouts = NodeRunTestTimeouts {
    goal: GOAL_TIMEOUT,
    result: RESULT_TIMEOUT,
};

/// How a `node_run` goal that a test sends ends, with the goal response of
/// an accepted goal.
pub struct NodeRunTestResponse {
    /// The goal response of an accepted goal; `None` when the daemon refused
    /// the goal.
    pub goal_response: Option<NodeRunGoalResponse>,
    pub end: GoalEnd<NodeRunResult>,
}

impl NodeRunTestResponse {
    /// The goal response of an accepted goal. Panics when the daemon refused
    /// the goal.
    #[track_caller]
    pub fn accepted(&self) -> &NodeRunGoalResponse {
        self.goal_response
            .as_ref()
            .unwrap_or_else(|| panic!("the daemon refused the node_run goal: {:?}", self.end))
    }
}

/// How a daemon goal that a test sends ends, as its client sees it.
#[derive(Debug)]
pub enum GoalEnd<R> {
    /// The daemon refused the goal.
    Refused(Failure),
    /// The goal completed with this result.
    Completed(R),
    /// The daemon accepted the goal, and the goal failed.
    Failed(Failure),
    /// The goal was cancelled for this reason.
    Cancelled(String),
}

impl<R: Debug> GoalEnd<R> {
    /// The result of a completed goal. Panics with the ending of any other
    /// goal.
    #[track_caller]
    pub fn completed(self) -> R {
        match self {
            GoalEnd::Completed(result) => result,
            other => panic!("the goal did not complete: {other:?}"),
        }
    }

    pub fn is_completed(&self) -> bool {
        matches!(self, GoalEnd::Completed(_))
    }

    /// The failure of a refused or failed goal. Panics with the ending of
    /// any other goal.
    #[track_caller]
    pub fn failure(&self) -> &Failure {
        match self {
            GoalEnd::Refused(failure) | GoalEnd::Failed(failure) => failure,
            other => panic!("the goal did not fail: {other:?}"),
        }
    }

    /// The message of the failure of a refused or failed goal. Panics with
    /// the ending of any other goal.
    #[track_caller]
    pub fn failure_message(&self) -> &str {
        self.failure().message()
    }

    /// The failure of a refused goal. Panics with the ending of any other
    /// goal.
    #[track_caller]
    pub fn refusal(&self) -> &Failure {
        match self {
            GoalEnd::Refused(failure) => failure,
            other => panic!("the daemon did not refuse the goal: {other:?}"),
        }
    }

    /// The failure of a goal that the daemon accepted and that then failed.
    /// Panics with the ending of any other goal.
    #[track_caller]
    pub fn failed(&self) -> &Failure {
        match self {
            GoalEnd::Failed(failure) => failure,
            other => panic!("the goal did not fail after its acceptance: {other:?}"),
        }
    }

    /// The reason of a cancelled goal. Panics with the ending of any other
    /// goal.
    #[track_caller]
    pub fn cancel_reason(&self) -> &str {
        match self {
            GoalEnd::Cancelled(reason) => reason,
            other => panic!("the goal was not cancelled: {other:?}"),
        }
    }
}

/// Asserts that `failure` has `kind`, a kind of the daemon.
#[track_caller]
pub fn assert_failure_kind(failure: &Failure, kind: impl Into<DaemonFailureKind>) {
    assert_eq!(
        failure.kind().as_str(),
        kind.into().name(),
        "the failure has another kind: {failure}"
    );
}

/// Asserts that `failure` has `kind` and that its message contains `text`.
#[track_caller]
pub fn assert_failure(failure: &Failure, kind: impl Into<DaemonFailureKind>, text: &str) {
    assert_failure_kind(failure, kind);
    assert!(
        failure.message().contains(text),
        "the failure message does not contain {text:?}: {failure}"
    );
}

/// Builds the instance plan a `node_run` goal carries.
///
/// A test cannot choose the messaging endpoint, the bound core node, or the
/// node identity. Those belong to the daemon that spawns the node, so the
/// goal has nowhere to put them and a test cannot accidentally pin a node to
/// the wrong daemon.
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

/// Why [`drain_feedback`] returned.
pub enum FeedbackDrainOutcome {
    /// The feedback handler asked to stop after a feedback line.
    Stopped,
    /// The server closed the feedback stream, i.e. the action completed.
    Closed,
    /// The absolute deadline elapsed before either of the above.
    TimedOut,
}

/// Drains feedback from a live action handle, decoding each line with
/// `decode` and handing it to `on_feedback`. Returns as soon as `on_feedback`
/// breaks, the server closes the stream, or the absolute deadline elapses.
pub async fn drain_feedback<F>(
    action_handle: &mut ActionGoalHandle,
    decode: impl Fn(&[u8]) -> F,
    mut on_feedback: impl FnMut(F) -> ControlFlow<()>,
    absolute_deadline: tokio::time::Instant,
) -> FeedbackDrainOutcome {
    loop {
        if tokio::time::Instant::now() >= absolute_deadline {
            return FeedbackDrainOutcome::TimedOut;
        }
        let drain_timeout = Duration::from_millis(50);
        match tokio::time::timeout(drain_timeout, action_handle.on_next_feedback()).await {
            Ok(Ok(msg)) => {
                if on_feedback(decode(msg.payload().as_ref())).is_break() {
                    return FeedbackDrainOutcome::Stopped;
                }
            }
            Ok(Err(_)) => return FeedbackDrainOutcome::Closed,
            Err(_) => {}
        }
    }
}

/// A feedback handler for [`drain_feedback`] that forwards every line to
/// `feedback_tx`, when there is one, and never stops: the drain runs until the
/// action completes.
fn forward_until_closed<F>(
    feedback_tx: Option<&UnboundedSender<F>>,
) -> impl FnMut(F) -> ControlFlow<()> + '_ {
    move |feedback| {
        if let Some(tx) = feedback_tx {
            let _ = tx.send(feedback);
        }
        ControlFlow::Continue(())
    }
}

/// The goal response of an accepted goal, decoded with `decode`, or the
/// failure of a refused goal. `what` names the goal response in the errors.
pub fn goal_answer<T, E: Display>(
    action_handle: &ActionGoalHandle,
    decode: impl Fn(&[u8]) -> Result<T, E>,
    what: &str,
) -> Result<Result<T, Failure>, String> {
    match &action_handle.goal_reply().answer {
        GoalAnswer::Accepted(response) => decode(response.as_ref())
            .map(Ok)
            .map_err(|err| format!("Failed to decode {what}: {err}")),
        GoalAnswer::Refused(failure) => Ok(Err(failure.clone())),
    }
}

/// Fetches how an action ended, and decodes the result of a completed one
/// with `decode`. `what` names the result in the errors. A goal that ends
/// without an outcome for its client (abandoned or expired) is an error.
pub async fn fetch_end<R, E: Display>(
    messenger: &MessengerHandle,
    action_handle: &ActionGoalHandle,
    fetch_timeout: Duration,
    decode: impl Fn(&[u8]) -> Result<R, E>,
    what: &str,
) -> Result<GoalEnd<R>, String> {
    let reply = ActionMessenger::request_result(messenger, action_handle, fetch_timeout)
        .await
        .map_err(|err| format!("Failed to get {what}: {err}"))?;
    match reply.outcome {
        GoalOutcome::Completed(body) => decode(body.as_ref())
            .map(GoalEnd::Completed)
            .map_err(|err| format!("Failed to decode {what}: {err}")),
        GoalOutcome::Failed(failure) => Ok(GoalEnd::Failed(failure)),
        GoalOutcome::Cancelled { reason, .. } => Ok(GoalEnd::Cancelled(reason)),
        GoalOutcome::Abandoned => Err(format!("The goal of the {what} was abandoned")),
        GoalOutcome::Expired => Err(format!("The {what} expired")),
    }
}

/// Sends `goal` to the daemon action it belongs to on `core_node_name`, from
/// the test caller.
pub async fn send_daemon_goal(
    messenger: &MessengerHandle,
    core_node_name: &str,
    goal: &impl core_node_api::ActionGoal,
    goal_timeout: Duration,
) -> Result<ActionGoalHandle, PeppyError> {
    send_goal(
        goal,
        messenger,
        core_node_name,
        CALLER_INSTANCE_ID,
        Some(core_node_name),
        goal_timeout,
    )
    .await
}

/// The `--restart never` request: a crash leaves the instance `failed` and
/// schedules nothing, so a test whose subject is the crash itself passes it.
pub fn never() -> RestartRequest {
    RestartRequest::Flag(RestartDeclaration::policy_only(RestartPolicy::Never))
}

/// The `--restart on_failure` request.
pub fn on_failure() -> RestartRequest {
    RestartRequest::Flag(RestartDeclaration::policy_only(RestartPolicy::OnFailure))
}

/// The `--restart always` request.
pub fn always() -> RestartRequest {
    RestartRequest::Flag(RestartDeclaration::policy_only(RestartPolicy::Always))
}

/// The `node_run` goal of `instance_plan`, whose result the daemon keeps for
/// the test's result budget.
pub fn node_run_goal(
    instance_plan: config::runtime::NodeInstancePlan,
    node_name: &str,
    tag: &str,
    timeouts: &NodeRunTestTimeouts,
) -> NodeRunGoal {
    NodeRunGoal::new(instance_plan, node_name, tag, timeouts.result.as_secs())
}

/// Sends `goal` and waits for its result.
pub async fn send_node_run_goal_and_wait(
    messenger: &MessengerHandle,
    core_node_name: &str,
    goal: NodeRunGoal,
    timeouts: &NodeRunTestTimeouts,
) -> Result<NodeRunTestResponse, String> {
    send_node_run_and_wait_internal(messenger, core_node_name, goal, timeouts, None).await
}

/// Sends a `node_run` goal and returns the live action handle with the
/// daemon's answer: the decoded goal response, or the refusal.
async fn send_node_run_goal_answered(
    messenger: &MessengerHandle,
    core_node_name: &str,
    goal: NodeRunGoal,
    goal_timeout: Duration,
) -> Result<(ActionGoalHandle, Result<NodeRunGoalResponse, Failure>), String> {
    let action_handle = send_daemon_goal(messenger, core_node_name, &goal, goal_timeout)
        .await
        .map_err(|e| format!("Failed to send goal: {}", e))?;
    let answer = goal_answer(&action_handle, NodeRunGoalResponse::decode, "goal response")?;
    Ok((action_handle, answer))
}

/// Sends a `node_run` goal and returns the live action handle plus its decoded
/// goal response, or an error when the daemon refuses the goal. A test calls
/// it directly when it does work between the goal and the result (e.g.
/// bringing up a delayed health responder once startup output has streamed)
/// or never fetches the result at all.
pub async fn send_node_run_goal(
    messenger: &MessengerHandle,
    core_node_name: &str,
    goal: NodeRunGoal,
    goal_timeout: Duration,
) -> Result<(ActionGoalHandle, NodeRunGoalResponse), String> {
    // A refused goal never streams feedback or produces a result, so callers
    // must not proceed to drain; they would just burn the full result budget
    // and surface a generic timeout instead of the actual refusal.
    match send_node_run_goal_answered(messenger, core_node_name, goal, goal_timeout).await? {
        (action_handle, Ok(goal_response)) => Ok((action_handle, goal_response)),
        (_, Err(refusal)) => Err(format!("the daemon refused the node_run goal: {refusal}")),
    }
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
    stop_when: impl Fn(&[NodeRunFeedback]) -> bool,
) -> FeedbackDrainOutcome {
    drain_feedback(
        action_handle,
        |payload| NodeRunFeedback::decode(payload).expect("failed to decode NodeRunFeedback"),
        |feedback| {
            if let Some(tx) = feedback_tx {
                let _ = tx.send(feedback.clone());
            }
            collected.push(feedback);
            if stop_when(collected) {
                return ControlFlow::Break(());
            }
            ControlFlow::Continue(())
        },
        absolute_deadline,
    )
    .await
}

/// Waits for a live action to finish without ever fetching its result: reads
/// its feedback stream, discarding every line, until the server closes it.
/// The daemon frees the action's single-goal slot before it completes the
/// goal, and completing is what closes the stream, so by the time this returns
/// the next goal is admitted. It is the one completion signal a client can
/// observe without a result fetch; the node stack is not one, because the
/// entity's stage changes while the work task is still winding down, ahead of
/// the slot release.
pub async fn wait_until_action_completes(action_handle: &mut ActionGoalHandle, timeout: Duration) {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        match tokio::time::timeout(remaining, action_handle.on_next_feedback()).await {
            Ok(Ok(_feedback)) => {}
            Ok(Err(PeppyError::ActionFeedbackChannelClosed)) => return,
            Ok(Err(err)) => {
                panic!("the action's feedback stream failed before it completed: {err}")
            }
            Err(_) => panic!("the action did not complete within {timeout:?}"),
        }
    }
}

async fn send_node_run_and_wait_internal(
    messenger: &MessengerHandle,
    core_node_name: &str,
    goal: NodeRunGoal,
    timeouts: &NodeRunTestTimeouts,
    feedback_tx: Option<UnboundedSender<NodeRunFeedback>>,
) -> Result<NodeRunTestResponse, String> {
    let (mut action_handle, answer) =
        send_node_run_goal_answered(messenger, core_node_name, goal, timeouts.goal).await?;
    let goal_response = match answer {
        Ok(goal_response) => goal_response,
        Err(refusal) => {
            return Ok(NodeRunTestResponse {
                goal_response: None,
                end: GoalEnd::Refused(refusal),
            });
        }
    };

    let absolute_deadline = tokio::time::Instant::now() + timeouts.result;
    let mut collected = Vec::new();
    // Drain feedback until the server closes the stream on completion, within
    // the result budget, then fetch how the goal ended once.
    let drained = drain_node_run_feedback(
        &mut action_handle,
        feedback_tx.as_ref(),
        &mut collected,
        absolute_deadline,
        |_| false,
    )
    .await;
    if matches!(drained, FeedbackDrainOutcome::TimedOut) {
        return Err("Timeout waiting for node_run result".to_string());
    }

    let fetch_timeout = absolute_deadline.saturating_duration_since(tokio::time::Instant::now());
    let end = fetch_end(
        messenger,
        &action_handle,
        fetch_timeout,
        NodeRunResult::decode,
        "result",
    )
    .await?;
    Ok(NodeRunTestResponse {
        goal_response: Some(goal_response),
        end,
    })
}

/// Sends a `node_run` goal, runs `before_cancel`, cancels the goal as its
/// client would, and returns how the run ended.
pub async fn send_node_run_then_cancel(
    messenger: &MessengerHandle,
    core_node_name: &str,
    instance_plan: config::runtime::NodeInstancePlan,
    node_name: &str,
    tag: &str,
    timeouts: &NodeRunTestTimeouts,
    before_cancel: impl std::future::Future<Output = ()>,
) -> Result<GoalEnd<NodeRunResult>, String> {
    let (mut action_handle, _) = send_node_run_goal(
        messenger,
        core_node_name,
        node_run_goal(instance_plan, node_name, tag, timeouts),
        timeouts.goal,
    )
    .await?;
    before_cancel.await;
    ActionMessenger::cancel_goal(messenger, &action_handle, timeouts.goal)
        .await
        .map_err(|err| format!("Failed to cancel the goal: {err}"))?;
    let absolute_deadline = tokio::time::Instant::now() + timeouts.result;
    let mut collected = Vec::new();
    if let FeedbackDrainOutcome::TimedOut = drain_node_run_feedback(
        &mut action_handle,
        None,
        &mut collected,
        absolute_deadline,
        |_| false,
    )
    .await
    {
        return Err("Timeout waiting for node_run result".to_string());
    }
    let fetch_timeout = absolute_deadline.saturating_duration_since(tokio::time::Instant::now());
    fetch_end(
        messenger,
        &action_handle,
        fetch_timeout,
        NodeRunResult::decode,
        "result",
    )
    .await
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
        node_run_goal(instance_plan, node_name, tag, timeouts),
        timeouts.goal,
    )
    .await?;

    let absolute_deadline = tokio::time::Instant::now() + timeouts.result;
    let mut feedback = Vec::new();
    match drain_node_run_feedback(
        &mut action_handle,
        None,
        &mut feedback,
        absolute_deadline,
        &expected_output,
    )
    .await
    {
        FeedbackDrainOutcome::Stopped => {}
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
        |_| false,
    )
    .await;

    let fetch_timeout = absolute_deadline.saturating_duration_since(tokio::time::Instant::now());
    let end = fetch_end(
        caller_messenger,
        &action_handle,
        fetch_timeout,
        NodeRunResult::decode,
        "result",
    )
    .await?;
    Ok((
        NodeRunTestResponse {
            goal_response: Some(goal_response),
            end,
        },
        feedback,
    ))
}

async fn send_node_add_and_wait_internal<'a>(
    messenger: &MessengerHandle,
    core_node_name: &str,
    source: impl Into<NodeAddSource<'a>>,
    goal_timeout: Duration,
    result_timeout: Duration,
    feedback_tx: Option<UnboundedSender<NodeAddFeedback>>,
    force: bool,
) -> Result<GoalEnd<NodeAddResult>, String> {
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
        NodeAddSource::ResolveRef { name, tag } => {
            let src = NodeSource::resolve_ref(*name, *tag)
                .map_err(|e| format!("invalid resolve-ref source in test: {e}"))?;
            NodeAddGoal::from_source(src, TEST_GIT_HASH, result_timeout.as_secs())
        }
    }
    .with_force(force);

    let mut action_handle = send_daemon_goal(messenger, core_node_name, &goal, goal_timeout)
        .await
        .map_err(|e| format!("Failed to send goal: {}", e))?;

    // A refused goal ends here, as it does for the CLI, which polls no
    // result of a refused goal.
    if let Err(refusal) = goal_answer(&action_handle, NodeAddGoalResponse::decode, "goal response")?
    {
        return Ok(GoalEnd::Refused(refusal));
    }

    let absolute_deadline = tokio::time::Instant::now() + result_timeout;

    // Drain feedback until the server closes the stream on completion, then
    // fetch the buffered result once.
    let drained = drain_feedback(
        &mut action_handle,
        |payload| NodeAddFeedback::decode(payload).expect("failed to decode NodeAddFeedback"),
        forward_until_closed(feedback_tx.as_ref()),
        absolute_deadline,
    )
    .await;
    match drained {
        FeedbackDrainOutcome::TimedOut => {
            return Err("Timeout waiting for node_add result".to_string());
        }
        FeedbackDrainOutcome::Stopped | FeedbackDrainOutcome::Closed => {}
    }

    let fetch_timeout = absolute_deadline.saturating_duration_since(tokio::time::Instant::now());
    fetch_end(
        messenger,
        &action_handle,
        fetch_timeout,
        NodeAddResult::decode,
        "result",
    )
    .await
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
) -> Result<GoalEnd<NodeBuildResult>, String> {
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
) -> Result<GoalEnd<NodeBuildResult>, String> {
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
) -> Result<GoalEnd<NodeBuildResult>, String> {
    let goal = NodeBuildGoal::new(node_name, node_tag, result_timeout.as_secs())
        .with_env_vars(env_vars)
        .with_force(force);
    let mut action_handle = send_daemon_goal(messenger, core_node_name, &goal, goal_timeout)
        .await
        .map_err(|e| format!("Failed to send build goal: {}", e))?;

    if let Err(refusal) = goal_answer(
        &action_handle,
        NodeBuildGoalResponse::decode,
        "build goal response",
    )? {
        return Ok(GoalEnd::Refused(refusal));
    }
    let absolute_deadline = tokio::time::Instant::now() + result_timeout;

    // Drain feedback until the server closes the stream on completion, then
    // fetch the buffered result once.
    let drained = drain_feedback(
        &mut action_handle,
        |payload| NodeBuildFeedback::decode(payload).expect("failed to decode NodeBuildFeedback"),
        forward_until_closed(feedback_tx.as_ref()),
        absolute_deadline,
    )
    .await;
    match drained {
        FeedbackDrainOutcome::TimedOut => {
            return Err("Timeout waiting for node_build result".to_string());
        }
        FeedbackDrainOutcome::Stopped | FeedbackDrainOutcome::Closed => {}
    }

    let fetch_timeout = absolute_deadline.saturating_duration_since(tokio::time::Instant::now());
    fetch_end(
        messenger,
        &action_handle,
        fetch_timeout,
        NodeBuildResult::decode,
        "build result",
    )
    .await
}

/// A build that [`send_logged_node_build`] sent: the log path that the goal
/// response of the accepted goal names, and how the build ended.
pub struct LoggedBuild {
    pub log_path: PathBuf,
    pub end: GoalEnd<NodeBuildResult>,
}

/// Sends the build goal of the staged node `(node_name, node_tag)` with no
/// env overrides, forwards each feedback line of the build to `feedback_tx`
/// when there is one, and returns the log path of the accepted goal with how
/// the build ended. Panics when the daemon refuses the goal.
pub async fn send_logged_node_build(
    started: &StartedCoreNode,
    node_name: &str,
    node_tag: &str,
    goal_timeout: Duration,
    result_timeout: Duration,
    feedback_tx: Option<&UnboundedSender<NodeBuildFeedback>>,
) -> LoggedBuild {
    let goal = NodeBuildGoal::new(node_name, node_tag, result_timeout.as_secs());
    let mut action_handle = send_daemon_goal(
        &started.caller_handle,
        &started.core_node_name,
        &goal,
        goal_timeout,
    )
    .await
    .expect("the node_build goal should be sent");
    let goal_response = goal_answer(
        &action_handle,
        NodeBuildGoalResponse::decode,
        "build goal response",
    )
    .expect("the goal answer should decode")
    .unwrap_or_else(|refusal| panic!("the daemon refused the node_build goal: {refusal}"));
    let deadline = tokio::time::Instant::now() + result_timeout;
    let drained = drain_feedback(
        &mut action_handle,
        |payload| NodeBuildFeedback::decode(payload).expect("failed to decode NodeBuildFeedback"),
        forward_until_closed(feedback_tx),
        deadline,
    )
    .await;
    assert!(
        !matches!(drained, FeedbackDrainOutcome::TimedOut),
        "the build did not end within {result_timeout:?}"
    );
    let end = fetch_end(
        &started.caller_handle,
        &action_handle,
        deadline.saturating_duration_since(tokio::time::Instant::now()),
        NodeBuildResult::decode,
        "build result",
    )
    .await
    .expect("the end of the build should be fetched");
    LoggedBuild {
        log_path: goal_response.log_path,
        end,
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
) -> Result<GoalEnd<NodeAddResult>, String> {
    send_node_add_and_wait_internal(
        messenger,
        core_node_name,
        source,
        goal_timeout,
        result_timeout,
        feedback_tx,
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
        GOAL_TIMEOUT,
        RESULT_TIMEOUT,
        Vec::new(),
        None,
    )
    .await
    .expect("node_build request should complete");
    if !result.is_completed() {
        panic!("build_staged_node failed: {result:?}");
    }
}

/// Convenience helper for tests that need a node to be both added AND built
/// (e.g. start/info/stop tests). Performs `send_node_add_and_wait` followed by
/// `send_node_build_and_wait` and returns the build result. Each goal waits
/// [`GOAL_TIMEOUT`] for its answer and [`RESULT_TIMEOUT`] for its end.
pub async fn send_node_add_then_build<'a>(
    messenger: &MessengerHandle,
    core_node_name: &str,
    source: impl Into<NodeAddSource<'a>>,
) -> Result<NodeBuildResult, String> {
    let add = send_node_add_and_wait_internal(
        messenger,
        core_node_name,
        source,
        GOAL_TIMEOUT,
        RESULT_TIMEOUT,
        None,
        false,
    )
    .await?;
    let NodeAddResult {
        node_name,
        node_tag,
    } = match add {
        GoalEnd::Completed(result) => result,
        other => return Err(format!("node_add did not complete: {other:?}")),
    };
    let result = send_node_build_and_wait(
        messenger,
        core_node_name,
        &node_name,
        &node_tag,
        GOAL_TIMEOUT,
        RESULT_TIMEOUT,
        Vec::new(),
        None,
    )
    .await?;
    match result {
        GoalEnd::Completed(result) => Ok(result),
        other => Err(format!("node_build did not complete: {other:?}")),
    }
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
    add_and_build_process_node(
        started,
        node_name,
        node_tag,
        &["sh", "-c", "sleep 1000 & sleep 1000 & wait"],
    )
    .await
}

/// Adds and builds a process node that runs `run_cmd`. Returns the source
/// dir guard.
pub async fn add_and_build_process_node(
    started: &StartedCoreNode,
    node_name: &str,
    node_tag: &str,
    run_cmd: &[&str],
) -> TempDir {
    let source_dir = tempfile::tempdir().expect("temp source dir");
    let peppy_json5 = r#"{
            peppy_schema: "node/v1",
            manifest: { name: "{NAME}", tag: "{TAG}" },
            execution: {
                language: "rust",
                run_cmd: {RUN_CMD}
            }
        }"#
    .replace("{NAME}", node_name)
    .replace("{TAG}", node_tag)
    .replace(
        "{RUN_CMD}",
        &serde_json::to_string(run_cmd).expect("a run_cmd serializes"),
    );
    write_peppy_json5(source_dir.path(), &peppy_json5);

    let add_response = send_node_add_and_wait(
        &started.caller_handle,
        &started.core_node_name,
        source_dir.path(),
        GOAL_TIMEOUT,
        RESULT_TIMEOUT,
        None,
    )
    .await
    .expect("node_add should complete");
    add_response.completed();
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
) -> Result<GoalEnd<NodeAddResult>, String> {
    send_node_add_and_wait_internal(
        messenger,
        core_node_name,
        source,
        goal_timeout,
        result_timeout,
        feedback_tx,
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
        node_run_goal(instance_plan, node_name, tag, timeouts),
        timeouts,
        feedback_tx,
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
        node_run_goal(instance_plan, node_name, tag, timeouts).with_env_vars(env_vars),
        timeouts,
        feedback_tx,
    )
    .await
}
