#![allow(dead_code)]

use config::consts::{DEFAULT_MESSAGING_HOST, NODE_CONFIG_FILE, PEPPYGEN_OUTPUT_PATH};
use daemon_config::consts::{PEPPY_OUTPUT_DIR, PeppyDirs};
use config::node::{PeppygenLanguage, QoSProfile};
use core_node::names;
use core_node::nodes_repo_cache_path;
use core_node::{CoreNode, CoreNodeArguments, CoreNodeConfig};
use core_node_api::encoding::{
    ClockRequest, ClockResponse, ClockTick, DatastoreGetRequest, DatastoreGetResponse,
    DatastoreListRequest, DatastoreListResponse, DatastoreRemoveRequest, DatastoreRemoveResponse,
    DatastoreStoreRequest, DatastoreStoreResponse, NodeAddFeedback, NodeAddGoal,
    NodeAddGoalResponse, NodeAddResult, NodeBuildFeedback, NodeBuildGoal, NodeBuildGoalResponse,
    NodeBuildResult, NodeRunFeedback, NodeRunGoal, NodeRunGoalResponse, NodeRunResult, NodeSource,
};
use gix_url::Url as GitUrl;
use node_stack::NodeStack;
use peppylib::clock::wall_now_ns;
use peppylib::messaging::{
    ActionGoalHandle, MessengerHandle, ResultStatus, SenderTarget, ServiceTarget, TopicMessenger,
};
use peppylib::runtime::{TaskHandle, spawn};
use peppylib::services::health::listen_for_node_health;
use peppylib::{ActionMessenger, Message, Payload, ServiceMessenger};
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

/// Polls a datastore service on the started core node using the shared test
/// routing and 5-second timeout, returning the response message. Panics on any
/// transport failure — the datastore endpoints should always answer a
/// well-formed request.
async fn poll_datastore(started: &StartedCoreNode, service: &str, payload: Payload) -> Message {
    ServiceMessenger::poll(
        &started.caller_handle,
        &started.core_node_name,
        CALLER_INSTANCE_ID,
        core_node_target(&started.core_node_name),
        service,
        ServiceTarget::Any, // discover the daemon's random per-boot service instance
        payload,
        Duration::from_secs(5),
    )
    .await
    .unwrap_or_else(|e| panic!("datastore {service} poll should succeed: {e}"))
}

/// Sends a `datastore_store` request to the started core node and decodes the
/// (empty) acknowledgement. Panics on any transport or decode failure — the
/// store endpoint should always succeed for a well-formed request.
pub async fn datastore_store(started: &StartedCoreNode, key: &str, value: &[u8], encoding: &str) {
    let payload = DatastoreStoreRequest::new(key, value.to_vec(), encoding)
        .expect("test key should be a valid datastore key")
        .encode()
        .expect("encode store request should succeed");
    let response = poll_datastore(started, names::DATASTORE_STORE, payload).await;
    DatastoreStoreResponse::decode(&response.payload()).expect("decode store response");
}

/// Sends a `datastore_get` request to the started core node and returns the
/// decoded response. Panics on any transport or decode failure.
pub async fn datastore_get(started: &StartedCoreNode, key: &str) -> DatastoreGetResponse {
    let payload = DatastoreGetRequest::new(key)
        .expect("test key should be a valid datastore key")
        .encode()
        .expect("encode get request should succeed");
    let response = poll_datastore(started, names::DATASTORE_GET, payload).await;
    DatastoreGetResponse::decode(&response.payload()).expect("decode get response")
}

/// Sends a `datastore_list` request to the started core node and returns the
/// decoded response. Panics on any transport or decode failure.
pub async fn datastore_list(started: &StartedCoreNode) -> DatastoreListResponse {
    let payload = DatastoreListRequest::new()
        .encode()
        .expect("encode list request should succeed");
    let response = poll_datastore(started, names::DATASTORE_LIST, payload).await;
    DatastoreListResponse::decode(&response.payload()).expect("decode list response")
}

/// Sends a `datastore_remove` request to the started core node and returns
/// whether the key existed. Panics on any transport or decode failure.
pub async fn datastore_remove(started: &StartedCoreNode, key: &str) -> bool {
    let payload = DatastoreRemoveRequest::new(key)
        .expect("test key should be a valid datastore key")
        .encode()
        .expect("encode remove request should succeed");
    let response = poll_datastore(started, names::DATASTORE_REMOVE, payload).await;
    DatastoreRemoveResponse::decode(&response.payload())
        .expect("decode remove response")
        .removed
}

/// Stores an arbitrary binary value, reads it back, and asserts the value and
/// encoding survive the round trip. Shared between the mock-messenger and
/// real-zenoh datastore tests — the latter exercises real cross-process
/// serialization of the Cap'n Proto `Data` field.
pub async fn assert_datastore_binary_round_trip(started: &StartedCoreNode) {
    let key = "binary_key_1";
    let value = vec![0u8, 255, 0x80, 0xFE, 0x00, 0x42];
    let encoding = "application/octet-stream";

    datastore_store(started, key, &value, encoding).await;
    let response = datastore_get(started, key).await;

    assert!(response.found, "stored key should be found");
    assert_eq!(response.value, value, "value should survive round trip");
    assert_eq!(
        response.encoding, encoding,
        "encoding should survive round trip"
    );
    assert_eq!(
        response.last_modified_by, CALLER_INSTANCE_ID,
        "get should report the writer's instance_id"
    );
}

/// A wrapper around `TaskHandle` that aborts the task when dropped.
pub struct AbortOnDrop<T>(pub TaskHandle<T>);

impl<T> Drop for AbortOnDrop<T> {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Generic polling helper: repeatedly calls `predicate` until it returns
/// `Some(value)`, then returns that value. If `timeout` elapses first, panics
/// with `timeout_message`. Polls every 20 ms. `predicate` is synchronous on
/// purpose: the current callers only touch the filesystem, the node stack, and
/// child processes, none of which await.
pub async fn poll_until<T>(
    timeout: Duration,
    timeout_message: &str,
    mut predicate: impl FnMut() -> Option<T>,
) -> T {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if let Some(value) = predicate() {
            return value;
        }
        if std::time::Instant::now() > deadline {
            panic!("{timeout_message}");
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// True if a process exists and is not a zombie — matches the daemon's own
/// liveness definition (sysinfo, status != Zombie), so tests agree with what
/// `node_stop`/teardown consider "gone". A libc `kill(pid, 0)` check would
/// report a reaped-but-unwaited zombie as still running.
pub fn is_process_running(pid: u32) -> bool {
    let system = sysinfo::System::new_with_specifics(
        sysinfo::RefreshKind::nothing().with_processes(sysinfo::ProcessRefreshKind::nothing()),
    );
    match system.process(sysinfo::Pid::from_u32(pid)) {
        Some(process) => process.status() != sysinfo::ProcessStatus::Zombie,
        None => false,
    }
}

/// PIDs of live children of `parent_pid`, via sysinfo's parent links.
pub fn children_of(parent_pid: u32) -> Vec<u32> {
    let system = sysinfo::System::new_with_specifics(
        sysinfo::RefreshKind::nothing().with_processes(sysinfo::ProcessRefreshKind::nothing()),
    );
    let parent = sysinfo::Pid::from_u32(parent_pid);
    system
        .processes()
        .values()
        .filter(|p| p.parent() == Some(parent))
        .map(|p| p.pid().as_u32())
        .collect()
}

/// The state of an instance regardless of lifecycle stage, including terminal
/// (`Finished`/`Failed`) instances that the `Running`-only
/// `NodeStack::find_by_instance_id` no longer returns. Lets a test observe the
/// exit watcher's terminal transition, or confirm an instance is gone from every
/// state (not lingering as terminal after a stop/reset).
pub fn instance_state_in_any_state(
    node_stack: &NodeStack,
    instance_id: &config::node::Name,
) -> Option<core_node_api::InstanceState> {
    node_stack.snapshot().into_iter().find_map(|handle| {
        handle
            .read()
            .instances()
            .iter()
            .find(|inst| inst.instance_id() == instance_id)
            .map(|inst| inst.state())
    })
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
    target_core_node: &str,
    target_instance_id: &str,
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
            ServiceTarget::Producer(&peppylib::messaging::ProducerRef::new(
                target_core_node,
                target_instance_id,
            )),
        )
        .await
        {
            return;
        }
        if std::time::Instant::now() >= deadline {
            panic!(
                "service {to_node_name}/{to_service_name} on \
                 {target_core_node}/{target_instance_id} did not become \
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
        ServiceTarget::Any, // discover the daemon's random per-boot service instance
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
        false,
        names::CLOCK,
        &peppylib::messaging::ConsumerFilter::Any,
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
    let dir = TempDir::new_in(config_test_support::test_tmp_root()).expect("test data dir");
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
            arguments,
            ..config::runtime::NodeInstanceConfig::new(
                config::runtime::Name::new(instance_id).expect("valid instance id"),
            )
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
    runtime_config_json5: &str,
    node_name: &str,
    tag: &str,
    goal_timeout: Duration,
    result_secs: u64,
    env_vars: Vec<(String, String)>,
) -> Result<(ActionGoalHandle, NodeRunGoalResponse), String> {
    let goal =
        NodeRunGoal::new(runtime_config_json5, node_name, tag, result_secs).with_env_vars(env_vars);
    let goal_payload = goal
        .encode()
        .map_err(|e| format!("Failed to encode goal: {}", e))?;

    let action_handle = ActionMessenger::send_goal(
        messenger,
        core_node_name,
        CALLER_INSTANCE_ID,
        core_node_target(core_node_name),
        names::NODE_RUN_ACTION,
        None,
        goal_payload,
        QoSProfile::default(),
        goal_timeout,
    )
    .await
    .map_err(|e| format!("Failed to send goal: {}", e))?;

    let goal_response = NodeRunGoalResponse::decode(&action_handle.goal_response().payload())
        .map_err(|e| format!("Failed to decode goal response: {}", e))?;

    // A rejected goal never streams feedback or produces a result, so callers
    // must not proceed to drain — they would just burn the full result budget
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
    runtime_config_json5: &str,
    node_name: &str,
    tag: &str,
    timeouts: &NodeRunTestTimeouts,
    feedback_tx: Option<UnboundedSender<NodeRunFeedback>>,
    env_vars: Vec<(String, String)>,
) -> Result<NodeRunTestResponse, String> {
    let (mut action_handle, goal_response) = send_node_run_goal(
        messenger,
        core_node_name,
        runtime_config_json5,
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
    runtime_config_json5: &str,
    node_name: &str,
    tag: &str,
    instance_id: &str,
    timeouts: &NodeRunTestTimeouts,
    expected_output: impl Fn(&[NodeRunFeedback]) -> bool,
) -> Result<(NodeRunTestResponse, Vec<NodeRunFeedback>), String> {
    let (mut action_handle, goal_response) = send_node_run_goal(
        caller_messenger,
        core_node_name,
        runtime_config_json5,
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
        names::NODE_ADD_ACTION,
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
        names::NODE_BUILD_ACTION,
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

/// Builder for `nodes.json5` and `interfaces.json5` cache fixtures. Tests call
/// [`TestPackagesCache::fs_entry`] / `git_entry` / `interface_git_entry` to
/// declare discovered items, then [`TestPackagesCache::write`] to serialize
/// the files under `peppy_dirs.cache_dir()`.
#[derive(Default)]
pub struct TestPackagesCache {
    entries: Vec<serde_json::Value>,
    interfaces: Vec<serde_json::Value>,
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

    /// Adds an `interfaces.json5` entry for a git-sourced interface. `body`
    /// is the on-disk interface JSON5 (assumed already committed at
    /// `path_in_repo` inside `repo_url`); its sha256 is computed here so
    /// the cache fingerprint matches what `ensure_checkout` will read.
    pub fn interface_git_entry(
        mut self,
        name: &str,
        tag: &str,
        repo_url: &str,
        resolved_ref: &str,
        path_in_repo: &str,
        body: &str,
    ) -> Self {
        let sha = config::fingerprint::fingerprint_for_bytes(body.as_bytes());
        let mut m = serde_json::Map::new();
        m.insert(
            "interface_name".into(),
            serde_json::Value::String(name.into()),
        );
        m.insert("tag".into(), serde_json::Value::String(tag.into()));
        m.insert("sha256".into(), serde_json::Value::String(sha));
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
            serde_json::Value::String(path_in_repo.into()),
        );
        self.interfaces.push(serde_json::Value::Object(m));
        self
    }

    /// Adds an `interfaces.json5` entry for a filesystem-sourced interface.
    /// `body` is the on-disk interface JSON5 (assumed already written at
    /// `absolute_path`); its sha256 is computed here so the cache
    /// fingerprint matches what `resolve_interface_doc` reads back.
    pub fn interface_fs_entry(
        mut self,
        name: &str,
        tag: &str,
        absolute_path: impl AsRef<Path>,
        body: &str,
    ) -> Self {
        let sha = config::fingerprint::fingerprint_for_bytes(body.as_bytes());
        let mut m = serde_json::Map::new();
        m.insert(
            "interface_name".into(),
            serde_json::Value::String(name.into()),
        );
        m.insert("tag".into(), serde_json::Value::String(tag.into()));
        m.insert("sha256".into(), serde_json::Value::String(sha));
        m.insert("source_type".into(), serde_json::Value::String("fs".into()));
        m.insert(
            "path".into(),
            serde_json::Value::String(absolute_path.as_ref().to_string_lossy().into_owned()),
        );
        self.interfaces.push(serde_json::Value::Object(m));
        self
    }

    pub fn write(self, peppy_dirs: &daemon_config::consts::PeppyDirs) {
        let cache_dir = peppy_dirs.cache_dir();
        std::fs::create_dir_all(&cache_dir).expect("failed to create cache dir");
        let content =
            serde_json::to_string_pretty(&self.entries).expect("failed to serialize cache entries");
        std::fs::write(nodes_repo_cache_path(peppy_dirs), content)
            .expect("failed to write nodes.json5 fixture");
        let interfaces_path = core_node::interfaces_repo_cache_path(peppy_dirs);
        if self.interfaces.is_empty() {
            match std::fs::remove_file(&interfaces_path) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => panic!("failed to remove stale interfaces.json5 fixture: {e}"),
            }
        } else {
            let interfaces_content = serde_json::to_string_pretty(&self.interfaces)
                .expect("failed to serialize interface cache entries");
            std::fs::write(interfaces_path, interfaces_content)
                .expect("failed to write interfaces.json5 fixture");
        }
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

/// Adds and builds a node whose `run_cmd` forks two grandchild `sleep`s and
/// waits — all three processes share the node's process group (nodes are
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
pub fn create_test_node() -> TempDir {
    init_test_node_project("example_node", "v1", true)
}

/// Creates a fresh test node in a new temp directory.
/// Each call creates a completely new node with its own peppygen generation
/// and cargo build, ensuring isolation between tests.
///
/// The returned [`TempDir`] owns the directory and deletes it — including the
/// multi-GB cargo `target/` — when it drops, so test runs never accumulate
/// build artifacts. Bind it for as long as the node is needed (e.g. for the
/// whole test body) and let it drop at scope end.
pub fn create_test_node_with_name(node_name: &str, node_tag: &str) -> TempDir {
    init_test_node_project(node_name, node_tag, true)
}

pub fn init_test_node_project(node_name: &str, node_tag: &str, build_project: bool) -> TempDir {
    // Build under the shared test-tmp root (see `config_test_support::test_tmp_root`) and keep the
    // `TempDir` guard rather than `.keep()`-ing it, so the directory and its
    // ~2 GB cargo build are reclaimed when the returned guard drops.
    let node_dir = tempfile::Builder::new()
        .prefix("peppy_test_node_")
        .tempdir_in(config_test_support::test_tmp_root())
        .expect("failed to create temp directory for test node");

    init_cargo_project(node_dir.path(), node_name);
    write_test_node_files(node_dir.path(), node_name, node_tag);

    let peppy_dirs = PeppyDirs::default();
    generator::generate_peppygen_lib(
        PeppygenLanguage::Rust,
        node_dir.path(),
        Vec::new(),
        "test-hash",
        &peppy_dirs,
        Default::default(),
        None,
    )
    .expect("failed to generate peppygen for test node");

    if build_project {
        build_cargo_project(node_dir.path());
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
  peppy_schema: "node/v1",
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
    /// The same daemon-shutdown token the core node threads into every spawned
    /// node's health monitor and exit watcher. Exposed so a test can cancel it
    /// and assert the shutdown-time suppression (no spurious "became unhealthy",
    /// no crash relabeling of intentionally-stopped nodes).
    pub shutdown_token: tokio_util::sync::CancellationToken,
    _data_dir: TempDir,
}

fn default_node_arguments() -> CoreNodeArguments {
    CoreNodeArguments {
        node_startup_timeout: Duration::from_secs(10),
        node_start_health_timeout: Duration::from_secs(30),
        health_monitor_interval: Duration::from_secs(5),
        health_monitor_timeout: Duration::from_secs(3),
        // Faster than the production default (100 ms) so publish_clock tests
        // observe several ticks within a small fixed budget without flaking.
        clock_publish_interval: Duration::from_millis(50),
        // Faster than production (5 s) so the heartbeat test observes beats
        // quickly without flaking.
        heartbeat_interval: Duration::from_millis(200),
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
        daemon_config::peppy_config::PeppyConfig::default(),
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
    start_core_node_with_messenger(
        shared_messenger,
        args,
        data_dir,
        peppy_dirs,
        daemon_config::peppy_config::PeppyConfig::default(),
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

/// Convenience wrapper over [`start_core_node_with_real_messenger_in_mode`] with
/// the default timeouts, for the dual-mode e2e tests parameterized over the mode.
pub async fn start_core_node_with_real_messenger_mode(
    mode: daemon_config::peppy_config::Mode,
) -> StartedCoreNode {
    start_core_node_with_real_messenger_in_mode(
        Duration::from_secs(10),
        Duration::from_secs(30),
        mode,
    )
    .await
}

pub async fn start_core_node_with_real_messenger_and_timeouts(
    node_startup_timeout: Duration,
    node_start_health_timeout: Duration,
) -> StartedCoreNode {
    start_core_node_with_real_messenger_in_mode(
        node_startup_timeout,
        node_start_health_timeout,
        daemon_config::peppy_config::Mode::Peer,
    )
    .await
}

/// Like [`start_core_node_with_real_messenger_and_timeouts`] but the messaging
/// `mode` (peer vs router) is explicit. The core node's own session is built in
/// that mode, and its `PeppyConfig` carries it so spawned nodes are injected with
/// the same mode (faithful to production). Used by the dual-mode e2e tests.
pub async fn start_core_node_with_real_messenger_in_mode(
    node_startup_timeout: Duration,
    node_start_health_timeout: Duration,
    mode: daemon_config::peppy_config::Mode,
) -> StartedCoreNode {
    let (data_dir, peppy_dirs) = init_test_data_dir();
    let mut instance = pmi::ZenohAdapter::start_router_ephemeral_in_mode(
        DEFAULT_MESSAGING_HOST,
        None,
        mode.gossip(),
        pmi::SubscriberBufferSizes::default(),
        // The core node stamps the `local` org namespace onto every node it
        // spawns (see `organization_namespace` below); its own session must open
        // under the same namespace or it cannot reach a spawned node's
        // node_ready/health services. Mirrors the daemon's
        // `with_router(...).with_namespace(...)` pairing in production.
        Some(config::org::OrgNamespace::local()),
    )
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
    let peppy_config = daemon_config::peppy_config::PeppyConfig {
        mode,
        ..Default::default()
    };
    start_core_node_with_messenger(shared_messenger, args, data_dir, peppy_dirs, peppy_config).await
}

/// Variant of [`start_core_node_with_mock_messenger`] with a custom
/// cooperative-shutdown grace period (`peppy_config.lifecycle
/// .shutdown_grace_secs`). For tests that assert timing around the grace
/// window and need wider margins than the 5s default gives under parallel
/// test load.
pub async fn start_core_node_with_shutdown_grace(shutdown_grace_secs: u64) -> StartedCoreNode {
    let (data_dir, peppy_dirs) = init_test_data_dir();
    let shared_messenger = create_mock_messenger().await;
    let peppy_config = daemon_config::peppy_config::PeppyConfig {
        lifecycle: daemon_config::peppy_config::LifecycleConfig {
            shutdown_grace_secs,
            ..Default::default()
        },
        ..Default::default()
    };
    start_core_node_with_messenger(
        shared_messenger,
        default_node_arguments(),
        data_dir,
        peppy_dirs,
        peppy_config,
    )
    .await
}

pub async fn start_core_node_with_health_timeout(
    node_start_health_timeout: Duration,
) -> StartedCoreNode {
    let (data_dir, peppy_dirs) = init_test_data_dir();
    let shared_messenger = create_mock_messenger().await;
    let mut args = default_node_arguments();
    args.node_start_health_timeout = node_start_health_timeout;
    start_core_node_with_messenger(
        shared_messenger,
        args,
        data_dir,
        peppy_dirs,
        daemon_config::peppy_config::PeppyConfig::default(),
    )
    .await
}

pub async fn start_core_node_with_health_monitor(
    health_monitor_interval: Duration,
    health_monitor_timeout: Duration,
) -> StartedCoreNode {
    let (data_dir, peppy_dirs) = init_test_data_dir();
    let shared_messenger = create_mock_messenger().await;
    let mut args = default_node_arguments();
    args.health_monitor_interval = health_monitor_interval;
    args.health_monitor_timeout = health_monitor_timeout;
    start_core_node_with_messenger(
        shared_messenger,
        args,
        data_dir,
        peppy_dirs,
        daemon_config::peppy_config::PeppyConfig::default(),
    )
    .await
}

async fn start_core_node_with_messenger(
    shared_messenger: Arc<Mutex<Messenger>>,
    node_arguments: CoreNodeArguments,
    data_dir: TempDir,
    peppy_dirs: PeppyDirs,
    peppy_config: daemon_config::peppy_config::PeppyConfig,
) -> StartedCoreNode {
    let caller_handle = MessengerHandle::from_shared(Arc::clone(&shared_messenger));
    let root_dir = std::env::current_dir().expect("failed to get current directory");
    let shutdown_token = tokio_util::sync::CancellationToken::new();
    let core_node = CoreNode::new(CoreNodeConfig {
        messenger: Arc::clone(&shared_messenger),
        node_name: Some("test_core_node".to_string()),
        arguments: node_arguments,
        root_dir,
        peppy_dirs: peppy_dirs.clone(),
        peppy_config,
        organization_namespace: "local".to_string(),
        shutdown_token: shutdown_token.clone(),
    });
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
        shutdown_token,
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
/// production stop/remove flow sends a shutdown signal. This lets the
/// production stop path observe the child as cooperatively terminated within
/// its graceful window rather than having to force-kill a stubborn `sleep 10`.
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
            slot_bindings: std::collections::BTreeMap::new(),
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
    // signal, so the cooperative phase succeeds within its graceful window.
    // Tests that want the production stop path to fall through to force-kill
    // (a stuck process) use `spawn_real_stuck_instance`, which skips this.
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

/// Installs a messenger-side shutdown listener that SIGKILLs `pid` when the
/// daemon fires a cooperative `SHUTDOWN_SERVICE` signal at `(name,
/// instance_id)`. A node started through the real `node_run` service path gets a
/// live exit watcher but no node-side shutdown handling (the `run_cmd` is a bare
/// `sleep`), so without this it would ignore the cooperative phase and force the
/// stop/reset/teardown to wait out the whole force-kill deadline. Bridging the
/// signal to a kill lets those tests cooperate quickly while still exercising the
/// watcher-versus-stop interaction. The returned guard aborts the listener on
/// drop.
pub async fn install_kill_on_shutdown_listener(
    started: &StartedCoreNode,
    name: &str,
    instance_id: &config::node::Name,
    pid: u32,
) -> AbortOnDrop<peppylib::PeppyResult<()>> {
    let shutdown_handle = MessengerHandle::from_shared(Arc::clone(&started.shared_messenger));
    let (shutdown_task, shutdown_rx) = peppylib::services::shutdown::listen_for_shutdown(
        &shutdown_handle,
        &started.core_node_name,
        instance_id.as_str(),
        test_node_target(name),
    )
    .await
    .expect("failed to start shutdown listener for test instance");
    tokio::spawn(async move {
        if shutdown_rx.await.is_ok() {
            let _ = std::process::Command::new("kill")
                .arg("-KILL")
                .arg(pid.to_string())
                .status();
        }
    });
    AbortOnDrop(shutdown_task)
}

/// RAII guard for a test-spawned instance deliberately left in `Starting`:
/// `prepare_and_spawn` was driven but `commit_started` was NOT, so the instance
/// is registered as `Starting` with a live child, exactly the state a node is in
/// mid-launch. Holds the `Child` and `StartedInstanceCtx` so the launch is
/// neither committed nor aborted. On drop it SIGKILLs the child's whole process
/// group (negative pid) so the held `sleep`s don't leak past the test.
#[must_use = "guard keeps the half-started child alive; drop it to clean up"]
pub struct TestStartingInstance {
    pub pid: u32,
    pub instance_id: config::node::Name,
    _child: tokio::process::Child,
    _started_ctx: node_stack::StartedInstanceCtx,
    _feedback_drain: tokio::task::JoinHandle<()>,
}

impl Drop for TestStartingInstance {
    fn drop(&mut self) {
        // Best-effort: by the time a test drops this, teardown has usually
        // already reaped the group, so silence the expected ESRCH/EPERM noise.
        let _ = std::process::Command::new("kill")
            .arg("-KILL")
            .arg(format!("-{}", self.pid))
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
        self._feedback_drain.abort();
    }
}

/// Drives a real `prepare_and_spawn` on the entity at `(name, tag)` (which must
/// already be in `Ready`) but intentionally does NOT call `commit_started`,
/// leaving the instance in `Starting` with a live child. Used to prove that a
/// daemon teardown during the start window force-kills the `Starting`-window
/// child instead of orphaning it. The caller is responsible for ensuring the
/// node config's `run_cmd` is spawnable in the test environment.
pub async fn spawn_real_starting_instance(
    started: &StartedCoreNode,
    name: &str,
    tag: &str,
    instance_id: &config::node::Name,
) -> TestStartingInstance {
    let handle = started
        .node_stack
        .find(name, tag)
        .expect("spawn_real_starting_instance: entity should exist");
    let (output_sinks, _feedback_tx, drain) =
        make_real_output_sinks(&started.peppy_dirs, instance_id);

    let (child, started_ctx) = node_stack::NodeEntity::prepare_and_spawn(
        &handle,
        node_stack::StartContext {
            instance_id,
            runtime_config_json5: "{}",
            slot_bindings: std::collections::BTreeMap::new(),
            env_vars: &[],
            mount_paths_resolved: &[],
            peppy_dirs: &started.peppy_dirs,
            output_sinks,
        },
    )
    .await
    .expect("prepare_and_spawn should succeed on Ready entity");
    let pid = child.id().expect("child should have pid");

    TestStartingInstance {
        pid,
        instance_id: instance_id.clone(),
        _child: child,
        _started_ctx: started_ctx,
        _feedback_drain: drain,
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
            cancel_token: tokio_util::sync::CancellationToken::new(),
        },
    )
    .await
    .expect("real build should succeed on process node");
    build_drain.abort();

    let mut running = spawn_real_running_instance(started, name, tag, instance_id).await;
    running._working_dir = Some(working_dir);
    running
}
